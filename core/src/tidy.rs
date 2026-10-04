//! Tidy: slim an SVG without redrawing it.
//!
//! A good trace (VTracer's, say) is already compact geometry; redrawing it
//! through the raster pipeline only loses detail. Its waste is elsewhere: one
//! `<path>` element per shape, and dozens of colours too close to tell apart.
//! Tidy keeps every outline as drawn and
//!
//! - snaps colours closer than `o.merge` to the most used of them,
//! - with `o.edge_bg`, leaves out the background: shapes of the background
//!   colour that touch the canvas edge (white inside the artwork stays),
//! - joins paths of one colour into a single element wherever that cannot change
//!   what is painted over what,
//! - rewrites the path data compactly: relative or absolute, whichever is
//!   shorter, `h`/`v` for level lines, no repeated command letters.
//!
//! It only reads flat SVGs, the kind tracers write: `<path>` elements with a
//! fill, optionally inside plain `<g>`s, optionally moved by `translate()`.
//! Anything else (strokes, styles, gradients, other shapes) is declined with a
//! reason, and the caller redraws instead.

use std::collections::HashMap;

use svgtypes::{Color, PathParser, PathSegment, Transform};

use crate::palette::{Pal, Share};
use crate::{Opts, Rgb};

pub struct Tidied {
    pub svg: String,
    /// Paths read, and `<path>` elements written.
    pub paths_in: usize,
    pub paths_out: usize,
    /// Distinct fills read, and left after snapping.
    pub colours_in: usize,
    pub colours_out: usize,
    /// Background shapes left out.
    pub background: usize,
    /// The colours used, for the palette chips: in the same form as Redraw's.
    pub palette: Pal,
}

/// One path command in absolute coordinates, in thousandths of a unit, so the
/// relative steps written later are exact.
#[derive(Clone, Copy)]
enum Cmd {
    M(i64, i64),
    L(i64, i64),
    C(i64, i64, i64, i64, i64, i64),
    S(i64, i64, i64, i64),
    Q(i64, i64, i64, i64),
    A { rx: i64, ry: i64, rot: i64, large: bool, sweep: bool, x: i64, y: i64 },
    Z,
}

#[derive(Clone, Copy)]
struct Bbox {
    x0: i64,
    y0: i64,
    x1: i64,
    y1: i64,
}

impl Bbox {
    fn empty() -> Self {
        Bbox { x0: i64::MAX, y0: i64::MAX, x1: i64::MIN, y1: i64::MIN }
    }
    fn add(&mut self, x: i64, y: i64, pad: i64) {
        self.x0 = self.x0.min(x - pad);
        self.y0 = self.y0.min(y - pad);
        self.x1 = self.x1.max(x + pad);
        self.y1 = self.y1.max(y + pad);
    }
    fn union(&mut self, b: &Bbox) {
        self.x0 = self.x0.min(b.x0);
        self.y0 = self.y0.min(b.y0);
        self.x1 = self.x1.max(b.x1);
        self.y1 = self.y1.max(b.y1);
    }
    /// Touching counts as meeting: anti-aliased edges that touch interact.
    fn meets(&self, b: &Bbox) -> bool {
        self.x0 <= b.x1 && b.x0 <= self.x1 && self.y0 <= b.y1 && b.y0 <= self.y1
    }
}

struct Shape {
    fill: [u8; 4],
    evenodd: bool,
    cmds: Vec<Cmd>,
    bbox: Bbox,
    /// Area enclosed, in square units, curves taken as chords.
    area: f64,
}

/// The shoelace area of a path, counting each subpath with its winding, so
/// holes drawn the other way round subtract.
fn area(cmds: &[Cmd]) -> f64 {
    let (mut total, mut sub) = (0.0f64, 0.0f64);
    let (mut sx, mut sy, mut cx, mut cy) = (0f64, 0f64, 0f64, 0f64);
    let to = |x: i64, y: i64, cx: &mut f64, cy: &mut f64, sub: &mut f64| {
        let (x, y) = (x as f64 / UNIT, y as f64 / UNIT);
        *sub += *cx * y - x * *cy;
        (*cx, *cy) = (x, y);
    };
    for &c in cmds {
        match c {
            Cmd::M(x, y) => {
                sub += cx * sy - sx * cy;
                total += sub;
                sub = 0.0;
                (sx, sy) = (x as f64 / UNIT, y as f64 / UNIT);
                (cx, cy) = (sx, sy);
            }
            Cmd::L(x, y) | Cmd::C(_, _, _, _, x, y) | Cmd::S(_, _, x, y) | Cmd::Q(_, _, x, y) | Cmd::A { x, y, .. } => to(x, y, &mut cx, &mut cy, &mut sub),
            Cmd::Z => {}
        }
    }
    sub += cx * sy - sx * cy;
    total += sub;
    (total / 2.0).abs()
}

/// Inherited presentation attributes.
#[derive(Clone, Copy)]
struct Paint {
    fill: Option<[u8; 4]>, // None: fill="none"
    evenodd: bool,
    dx: f64,
    dy: f64,
}

const UNIT: f64 = 1000.0;

fn units(v: f64) -> i64 {
    (v * UNIT).round() as i64
}

// ---------- reading ----------

struct Tag<'a> {
    name: &'a str,
    attrs: Vec<(&'a str, &'a str)>,
    closing: bool,
    empty: bool,
}

/// Split the text into tags, skipping the declaration, comments, doctype and
/// text between tags.
fn tags(text: &str) -> Result<Vec<Tag<'_>>, String> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    let mut i = 0;
    while let Some(off) = text[i..].find('<') {
        i += off;
        let rest = &text[i..];
        if rest.starts_with("<!--") {
            i += rest.find("-->").ok_or("unclosed comment")? + 3;
            continue;
        }
        if rest.starts_with("<?") {
            i += rest.find("?>").ok_or("unclosed declaration")? + 2;
            continue;
        }
        if rest.starts_with("<![CDATA[") {
            return Err("it has embedded CDATA".into());
        }
        if rest.starts_with("<!") {
            if rest.contains("<!ENTITY") {
                return Err("it defines entities".into());
            }
            i += rest.find('>').ok_or("unclosed doctype")? + 1;
            continue;
        }
        // An element tag.
        let mut j = i + 1;
        let closing = b.get(j) == Some(&b'/');
        if closing {
            j += 1;
        }
        let start = j;
        while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'>' && b[j] != b'/' {
            j += 1;
        }
        let name = &text[start..j];
        let mut attrs = Vec::new();
        loop {
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            match b.get(j) {
                None => return Err("unclosed tag".into()),
                Some(b'>') => {
                    out.push(Tag { name, attrs, closing, empty: false });
                    j += 1;
                    break;
                }
                Some(b'/') if b.get(j + 1) == Some(&b'>') => {
                    out.push(Tag { name, attrs, closing, empty: true });
                    j += 2;
                    break;
                }
                _ => {}
            }
            let a0 = j;
            while j < b.len() && b[j] != b'=' && !b[j].is_ascii_whitespace() && b[j] != b'>' {
                j += 1;
            }
            let key = &text[a0..j];
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if b.get(j) != Some(&b'=') {
                return Err(format!("attribute {key} has no value"));
            }
            j += 1;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            let q = *b.get(j).ok_or("unclosed tag")?;
            if q != b'"' && q != b'\'' {
                return Err(format!("attribute {key} is unquoted"));
            }
            let v0 = j + 1;
            let len = text[v0..].find(q as char).ok_or("unclosed attribute")?;
            let value = &text[v0..v0 + len];
            if value.contains('&') {
                return Err("an attribute uses character references".into());
            }
            attrs.push((key, value));
            j = v0 + len + 1;
        }
        i = j;
    }
    Ok(out)
}

fn colour(v: &str) -> Result<Option<[u8; 4]>, String> {
    let v = v.trim();
    if v == "none" || v == "transparent" {
        return Ok(None);
    }
    let c: Color = v.parse().map_err(|_| format!("it has a fill Tidy can't read ({v})"))?;
    Ok(Some([c.red, c.green, c.blue, c.alpha]))
}

/// Apply one element's presentation attributes on top of what it inherits.
/// `keep` names attributes the element itself handles.
fn paint(parent: Paint, attrs: &[(&str, &str)], keep: &[&str]) -> Result<Paint, String> {
    let mut p = parent;
    for &(k, v) in attrs {
        match k {
            "fill" => p.fill = colour(v)?,
            "fill-rule" => p.evenodd = v.trim() == "evenodd",
            "transform" => {
                let t: Transform = v.parse().map_err(|_| "it has a transform Tidy can't read")?;
                if (t.a, t.b, t.c, t.d) != (1.0, 0.0, 0.0, 1.0) {
                    return Err("it scales or rotates shapes".into());
                }
                p.dx += t.e;
                p.dy += t.f;
            }
            "stroke" if v.trim() == "none" => {}
            "opacity" | "fill-opacity" if v.trim().parse::<f64>() == Ok(1.0) => {}
            "id" | "class" | "version" | "baseProfile" | "xml:space" | "data-name" => {}
            // Namespaced attributes are editor notes (sodipodi:, inkscape:) or
            // xml:space; none of them changes what is drawn.
            k if k.starts_with("xmlns") || k.starts_with("data-") || k.contains(':') || keep.contains(&k) => {}
            "stroke" | "stroke-width" | "opacity" | "fill-opacity" | "style" | "clip-path" | "mask" | "filter" => {
                return Err(format!("it uses {k}"))
            }
            k => return Err(format!("it uses the {k} attribute")),
        }
    }
    Ok(p)
}

fn read_path(d: &str, p: &Paint) -> Result<(Vec<Cmd>, Bbox), String> {
    let (ox, oy) = (p.dx, p.dy);
    let mut cmds = Vec::new();
    let mut bbox = Bbox::empty();
    let (mut cx, mut cy, mut sx, mut sy) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    // The last cubic's second control point, which a smooth curve reflects.
    let mut ctrl: Option<(f64, f64)> = None;
    let u = |x: f64, y: f64| (units(x + ox), units(y + oy));
    for seg in PathParser::from(d) {
        let seg = seg.map_err(|_| "a path's data is malformed")?;
        // Resolve relative coordinates against the current point.
        let rel = |abs: bool| if abs { (0.0, 0.0) } else { (cx, cy) };
        let prev_ctrl = ctrl.take();
        match seg {
            PathSegment::MoveTo { abs, x, y } => {
                let (bx, by) = rel(abs);
                (cx, cy) = (x + bx, y + by);
                (sx, sy) = (cx, cy);
                let (x, y) = u(cx, cy);
                cmds.push(Cmd::M(x, y));
                bbox.add(x, y, 0);
            }
            PathSegment::LineTo { abs, x, y } => {
                let (bx, by) = rel(abs);
                (cx, cy) = (x + bx, y + by);
                let (x, y) = u(cx, cy);
                cmds.push(Cmd::L(x, y));
                bbox.add(x, y, 0);
            }
            PathSegment::HorizontalLineTo { abs, x } => {
                cx = if abs { x } else { cx + x };
                let (x, y) = u(cx, cy);
                cmds.push(Cmd::L(x, y));
                bbox.add(x, y, 0);
            }
            PathSegment::VerticalLineTo { abs, y } => {
                cy = if abs { y } else { cy + y };
                let (x, y) = u(cx, cy);
                cmds.push(Cmd::L(x, y));
                bbox.add(x, y, 0);
            }
            PathSegment::CurveTo { abs, x1, y1, x2, y2, x, y } => {
                let (bx, by) = rel(abs);
                let (a, b) = u(x1 + bx, y1 + by);
                let (c, d) = u(x2 + bx, y2 + by);
                ctrl = Some((x2 + bx, y2 + by));
                (cx, cy) = (x + bx, y + by);
                let (e, f) = u(cx, cy);
                cmds.push(Cmd::C(a, b, c, d, e, f));
                for (x, y) in [(a, b), (c, d), (e, f)] {
                    bbox.add(x, y, 0);
                }
            }
            PathSegment::SmoothCurveTo { abs, x2, y2, x, y } => {
                let (bx, by) = rel(abs);
                let (rx, ry) = prev_ctrl.map_or((cx, cy), |(px, py)| (2.0 * cx - px, 2.0 * cy - py));
                let (a, b) = u(rx, ry);
                let (c, d) = u(x2 + bx, y2 + by);
                ctrl = Some((x2 + bx, y2 + by));
                (cx, cy) = (x + bx, y + by);
                let (e, f) = u(cx, cy);
                cmds.push(Cmd::S(c, d, e, f));
                for (x, y) in [(a, b), (c, d), (e, f)] {
                    bbox.add(x, y, 0);
                }
            }
            PathSegment::Quadratic { abs, x1, y1, x, y } => {
                let (bx, by) = rel(abs);
                let (a, b) = u(x1 + bx, y1 + by);
                (cx, cy) = (x + bx, y + by);
                let (e, f) = u(cx, cy);
                cmds.push(Cmd::Q(a, b, e, f));
                bbox.add(a, b, 0);
                bbox.add(e, f, 0);
            }
            PathSegment::SmoothQuadratic { .. } => return Err("it uses T path commands".into()),
            PathSegment::EllipticalArc { abs, rx, ry, x_axis_rotation, large_arc, sweep, x, y } => {
                let (bx, by) = rel(abs);
                let (px, py) = u(cx, cy);
                (cx, cy) = (x + bx, y + by);
                let (e, f) = u(cx, cy);
                let r = units(rx.abs().max(ry.abs()) * 2.0);
                cmds.push(Cmd::A { rx: units(rx.abs()), ry: units(ry.abs()), rot: units(x_axis_rotation), large: large_arc, sweep, x: e, y: f });
                bbox.add(px, py, r);
                bbox.add(e, f, r);
            }
            PathSegment::ClosePath { .. } => {
                (cx, cy) = (sx, sy);
                cmds.push(Cmd::Z);
            }
        }
    }
    Ok((cmds, bbox))
}

struct Root {
    attrs: Vec<(String, String)>,
    long_side: f64,
    /// The canvas, in thousandths.
    canvas: Bbox,
}

fn read(text: &str) -> Result<(Root, Vec<Shape>), String> {
    let tags = tags(text)?;
    let mut shapes = Vec::new();
    let mut stack: Vec<Paint> = Vec::new();
    let mut root: Option<Root> = None;
    let mut skip: Option<&str> = None; // inside title, desc or metadata
    for t in &tags {
        if let Some(name) = skip {
            if t.closing && t.name == name {
                skip = None;
            }
            continue;
        }
        if t.closing {
            if t.name == "svg" || t.name == "g" {
                stack.pop();
            }
            continue;
        }
        match t.name {
            "svg" if root.is_none() => {
                let p = paint(Paint { fill: Some([0, 0, 0, 255]), evenodd: false, dx: 0.0, dy: 0.0 }, &t.attrs, &["viewBox", "width", "height", "preserveAspectRatio"])?;
                let get = |k: &str| t.attrs.iter().find(|a| a.0 == k).map(|a| a.1);
                let vb: Vec<f64> = get("viewBox").map(|v| v.split(|c: char| c == ',' || c.is_whitespace()).filter(|s| !s.is_empty()).filter_map(|s| s.parse().ok()).collect()).unwrap_or_default();
                let len = |k: &str| get(k).filter(|v| !v.ends_with('%')).and_then(|v| v.trim_end_matches("px").parse::<f64>().ok());
                let (cx, cy, cw, ch) = if vb.len() == 4 { (vb[0], vb[1], vb[2], vb[3]) } else { (0.0, 0.0, len("width").unwrap_or(300.0), len("height").unwrap_or(150.0)) };
                let long_side = cw.max(ch);
                let canvas = Bbox { x0: units(cx), y0: units(cy), x1: units(cx + cw), y1: units(cy + ch) };
                let mut attrs = Vec::new();
                if vb.len() == 4 {
                    attrs.push(("viewBox".to_string(), get("viewBox").unwrap().to_string()));
                }
                for k in ["width", "height"] {
                    // Percentages only matter without a viewBox.
                    if let Some(v) = get(k).filter(|v| !(v.ends_with('%') && vb.len() == 4)) {
                        attrs.push((k.to_string(), v.to_string()));
                    }
                }
                if let Some(v) = get("preserveAspectRatio") {
                    attrs.push(("preserveAspectRatio".to_string(), v.to_string()));
                }
                root = Some(Root { attrs, long_side, canvas });
                stack.push(p);
                if t.empty {
                    break;
                }
            }
            "g" => {
                let parent = *stack.last().ok_or("a group sits outside the svg")?;
                let p = paint(parent, &t.attrs, &[])?;
                if !t.empty {
                    stack.push(p);
                }
            }
            "path" => {
                let parent = *stack.last().ok_or("a path sits outside the svg")?;
                let p = paint(parent, &t.attrs, &["d"])?;
                let d = t.attrs.iter().find(|a| a.0 == "d").map(|a| a.1).unwrap_or("");
                if let Some(fill) = p.fill {
                    let (cmds, bbox) = read_path(d, &p)?;
                    if cmds.len() > 1 && fill[3] > 0 {
                        let area = area(&cmds);
                        shapes.push(Shape { fill, evenodd: p.evenodd, cmds, bbox, area });
                    }
                }
                if !t.empty {
                    skip = Some("path");
                }
            }
            // Editor elements such as sodipodi:namedview hold settings, not drawing.
            name if name.contains(':') || name == "title" || name == "desc" || name == "metadata" => {
                if !t.empty {
                    skip = Some(name);
                }
            }
            "defs" if t.empty => {}
            other => return Err(format!("it has {} elements", if other.is_empty() { "unnamed" } else { other })),
        }
    }
    let root = root.ok_or("there's no svg element")?;
    if shapes.is_empty() {
        return Err("there are no filled paths".into());
    }
    Ok((root, shapes))
}

// ---------- merging ----------

/// How close a colour must be to count as one the viewer picked (as in Redraw).
const SAME: f64 = 24.0 * 24.0;

fn rgb(c: &[u8; 4]) -> Rgb {
    [c[0] as i32, c[1] as i32, c[2] as i32]
}

fn d2(a: &Rgb, b: &Rgb) -> f64 {
    (0..3).map(|k| ((a[k] - b[k]) as f64).powi(2)).sum()
}

/// CIE L*a*b* (D65) of an sRGB colour.
fn lab(c: &[u8; 4]) -> [f64; 3] {
    let lin = |v: u8| {
        let v = v as f64 / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    let (r, g, b) = (lin(c[0]), lin(c[1]), lin(c[2]));
    let x = (0.4124 * r + 0.3576 * g + 0.1805 * b) / 0.95047;
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let z = (0.0193 * r + 0.1192 * g + 0.9505 * b) / 1.08883;
    let f = |t: f64| if t > 0.008856 { t.cbrt() } else { 7.787 * t + 16.0 / 116.0 };
    let (fx, fy, fz) = (f(x), f(y), f(z));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// How different two colours look: squared distance in L*a*b*. Plain RGB
/// distance would call a mid grey closer to yellow than to cream.
fn seen2(a: &[u8; 4], b: &[u8; 4]) -> f64 {
    let (p, q) = (lab(a), lab(b));
    (0..3).map(|k| (p[k] - q[k]).powi(2)).sum()
}

/// One palette entry: a real fill colour from the file (Tidy never averages),
/// the area it now covers, and whether the viewer locked it in.
struct Entry {
    c: [u8; 4],
    n: f64,
    pin: bool,
    /// The fills folded into it.
    fills: Vec<[u8; 4]>,
}

/// Choose the colours to keep and map every fill onto one of them, as Redraw's
/// palette does: fills within `merge` group under the one covering the most
/// area; locked colours stay; left-out colours go to the nearest kept one; the
/// closest pair merges (weighted by the smaller) until `max_col` are left.
/// Fills with different opacity never merge.
fn choose_colours(shapes: &mut [Shape], o: &Opts) -> (usize, Pal) {
    let mut weight: Vec<([u8; 4], f64)> = Vec::new();
    for s in shapes.iter() {
        match weight.iter_mut().find(|w| w.0 == s.fill) {
            Some(w) => w.1 += s.area,
            None => weight.push((s.fill, s.area)),
        }
    }
    let colours_in = weight.len();
    let total: f64 = weight.iter().map(|w| w.1).sum::<f64>().max(1e-9);
    weight.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));
    let m2 = o.merge * o.merge;
    let picked = |c: &[u8; 4], list: &[Rgb]| list.iter().any(|q| d2(q, &rgb(c)) <= SAME);
    // Group by the merge distance.
    // Each locked colour is the one fill nearest it, heading its own group, so a
    // colour that merging folded away comes back when the viewer locks it.
    let mut cand: Vec<Entry> = Vec::new();
    for k in &o.choices.keep {
        let best = weight.iter().filter(|w| d2(&rgb(&w.0), k) <= SAME).min_by(|a, b| d2(&rgb(&a.0), k).partial_cmp(&d2(&rgb(&b.0), k)).unwrap());
        if let Some(&(c, n)) = best {
            if !cand.iter().any(|e| e.c == c) {
                cand.push(Entry { c, n, pin: true, fills: vec![c] });
            }
        }
    }
    for &(c, n) in &weight {
        if cand.iter().any(|e| e.pin && e.c == c) {
            continue;
        }
        // Shades of a locked colour join it; others join the nearest group within the merge distance.
        let near = |e: &&mut Entry| {
            let d = d2(&rgb(&e.c), &rgb(&c));
            e.c[3] == c[3] && ((e.pin && d <= SAME) || (o.merge > 0.0 && d <= m2))
        };
        let join = cand.iter_mut().filter(near).min_by(|a, b| d2(&rgb(&a.c), &rgb(&c)).partial_cmp(&d2(&rgb(&b.c), &rgb(&c))).unwrap());
        match join {
            Some(e) => {
                e.n += n;
                e.fills.push(c);
            }
            None => cand.push(Entry { c, n, pin: false, fills: vec![c] }),
        }
    }
    let (mut pal, mut removed): (Vec<Entry>, Vec<Entry>) = cand.into_iter().partition(|e| !picked(&e.c, &o.choices.drop));
    if pal.is_empty() {
        // Everything left out: keep the biggest so there is something to draw.
        pal.push(removed.remove(0));
    }
    let fold = |x: &mut Entry, y: Entry| {
        if (y.pin && !x.pin) || (!x.pin && !y.pin && y.n > x.n) {
            x.c = y.c;
        }
        x.n += y.n;
        x.pin |= y.pin;
        x.fills.extend(y.fills);
    };
    while pal.len() > o.max_col.max(1) {
        let (mut bi, mut bj, mut best) = (usize::MAX, 0, f64::INFINITY);
        for i in 0..pal.len() {
            for j in i + 1..pal.len() {
                if (pal[i].pin && pal[j].pin) || pal[i].c[3] != pal[j].c[3] {
                    continue;
                }
                // The colour that changes is the unlocked one, so its area is what counts;
                // otherwise a tiny locked eye would make every pairing with it look cheap.
                let n = if pal[i].pin { pal[j].n } else if pal[j].pin { pal[i].n } else { pal[i].n.min(pal[j].n) };
                let e = seen2(&pal[i].c, &pal[j].c) * n;
                if e < best {
                    (bi, bj, best) = (i, j, e);
                }
            }
        }
        if bi == usize::MAX {
            break;
        }
        let y = pal.remove(bj);
        fold(&mut pal[bi], y);
    }
    // Every fill's new colour.
    let mut to: HashMap<[u8; 4], [u8; 4]> = HashMap::new();
    for e in &pal {
        for f in &e.fills {
            to.insert(*f, e.c);
        }
    }
    for e in &removed {
        let near = pal.iter().min_by(|a, b| seen2(&a.c, &e.c).partial_cmp(&seen2(&b.c, &e.c)).unwrap()).unwrap();
        for f in &e.fills {
            to.insert(*f, near.c);
        }
    }
    for s in shapes.iter_mut() {
        s.fill = to[&s.fill];
    }
    // The chips: kept colours biggest first, then fills that lost their own colour.
    pal.sort_by(|a, b| b.n.partial_cmp(&a.n).unwrap());
    let mut merged: Vec<Share> = Vec::new();
    for e in &pal {
        for f in &e.fills {
            if *f != e.c && d2(&rgb(f), &rgb(&e.c)) > SAME && !merged.iter().any(|m| d2(&m.c, &rgb(f)) <= SAME) {
                let n: f64 = weight.iter().filter(|w| d2(&rgb(&w.0), &rgb(f)) <= SAME).map(|w| w.1).sum();
                merged.push(Share { c: rgb(f), share: n / total });
            }
        }
    }
    let p = Pal {
        pal: pal.iter().map(|e| rgb(&e.c)).collect(),
        bg: -1,
        order: (0..pal.len()).collect(),
        share: pal.iter().map(|e| e.n / total).collect(),
        pinned: pal.iter().map(|e| e.pin).collect(),
        merged,
        removed: removed.iter().map(|e| Share { c: rgb(&e.c), share: e.n / total }).collect(),
    };
    (colours_in, p)
}

/// Leave out the background: the colour of a bottom shape that covers the
/// canvas or, failing that, the colour with the most area along the edges.
/// Only its shapes that touch an edge go, so the same colour inside the
/// artwork, such as the white of an eye, stays. Returns how many went.
fn remove_background(shapes: &mut Vec<Shape>, canvas: &Bbox, merge: f64) -> usize {
    let slack = ((canvas.x1 - canvas.x0).max(canvas.y1 - canvas.y0) / 1000).max(1);
    let edge = |b: &Bbox| b.x0 <= canvas.x0 + slack || b.y0 <= canvas.y0 + slack || b.x1 >= canvas.x1 - slack || b.y1 >= canvas.y1 - slack;
    let covers = |b: &Bbox| b.x0 <= canvas.x0 + slack && b.y0 <= canvas.y0 + slack && b.x1 >= canvas.x1 - slack && b.y1 >= canvas.y1 - slack;
    let bg = match shapes.first().filter(|s| covers(&s.bbox)) {
        Some(s) => s.fill,
        None => {
            let mut area: HashMap<[u8; 4], f64> = HashMap::new();
            for s in shapes.iter().filter(|s| edge(&s.bbox)) {
                *area.entry(s.fill).or_default() += s.area;
            }
            match area.into_iter().max_by(|a, b| a.1.partial_cmp(&b.1).unwrap().then(b.0.cmp(&a.0))) {
                Some((c, _)) => c,
                None => return 0,
            }
        }
    };
    // Shades of the background count as background, up to the merge distance
    // or, below that, shades the eye can't tell apart.
    let near = merge.max(8.0).powi(2);
    let before = shapes.len();
    shapes.retain(|s| !(s.fill[3] == bg[3] && d2(&rgb(&s.fill), &rgb(&bg)) <= near && edge(&s.bbox)));
    before - shapes.len()
}

struct Group {
    fill: [u8; 4],
    evenodd: bool,
    members: Vec<Bbox>,
    bbox: Bbox,
    cmds: Vec<Cmd>,
}

/// How far back a shape may look for a group to join.
const REACH: usize = 256;

/// Join each shape to an earlier group of its colour when nothing painted in
/// between overlaps it (so moving it down changes nothing) and it overlaps
/// nothing already in the group (so fill rules can't open holes).
fn group(shapes: Vec<Shape>) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for s in shapes {
        let mut join = None;
        for (gi, g) in groups.iter().enumerate().rev().take(REACH) {
            let overlaps = g.bbox.meets(&s.bbox) && g.members.iter().any(|m| m.meets(&s.bbox));
            if g.fill == s.fill && g.evenodd == s.evenodd && !overlaps {
                join = Some(gi);
                break;
            }
            // Order between solid shapes of one colour doesn't show, so move past them.
            let same_solid = g.fill == s.fill && s.fill[3] == 255;
            if overlaps && !same_solid {
                break;
            }
        }
        match join {
            Some(gi) => {
                let g = &mut groups[gi];
                g.members.push(s.bbox);
                g.bbox.union(&s.bbox);
                g.cmds.extend(s.cmds);
            }
            None => groups.push(Group { fill: s.fill, evenodd: s.evenodd, members: vec![s.bbox], bbox: s.bbox, cmds: s.cmds }),
        }
    }
    groups
}

// ---------- writing ----------

/// A number in thousandths, written with at most `dec` decimals.
fn num(v: i64, dec: u32) -> String {
    let step = 10i64.pow(3 - dec);
    let v = (v as f64 / step as f64).round() as i64 * step;
    let neg = v < 0;
    let a = v.unsigned_abs();
    let (int, frac) = (a / 1000, a % 1000);
    let mut s = String::new();
    if neg {
        s.push('-');
    }
    if int > 0 || frac == 0 {
        s.push_str(&int.to_string());
    }
    if frac > 0 {
        let f = format!("{:03}", frac);
        s.push('.');
        s.push_str(f.trim_end_matches('0'));
    }
    s
}

/// Append numbers to a command, with separators only where needed.
fn push_nums(out: &mut String, vals: &[String]) {
    for (i, v) in vals.iter().enumerate() {
        if i > 0 {
            let prev = &vals[i - 1];
            let needs = !(v.starts_with('-') || (v.starts_with('.') && prev.contains('.')));
            if needs {
                out.push(' ');
            }
        }
        out.push_str(v);
    }
}

struct Writer {
    dec: u32,
    out: String,
    last: char,
    last_num: String,
}

impl Writer {
    /// Emit a command, dropping the letter when it repeats the previous one.
    fn emit(&mut self, cmd: char, vals: Vec<String>) {
        if cmd != self.last || cmd == 'M' || cmd == 'm' {
            self.out.push(cmd);
            self.last_num.clear();
        } else if let Some(first) = vals.first() {
            let needs = !(first.starts_with('-') || (first.starts_with('.') && self.last_num.contains('.')));
            if needs && !self.last_num.is_empty() {
                self.out.push(' ');
            }
        }
        push_nums(&mut self.out, &vals);
        self.last_num = vals.last().cloned().unwrap_or_default();
        // After a move, implicit repeats mean line-to, so never treat M as repeatable.
        self.last = cmd;
    }

    /// Write `rel` (relative form) or `abs`, whichever is shorter.
    fn either(&mut self, rel: (char, Vec<i64>), abs: (char, Vec<i64>)) {
        let f = |v: &Vec<i64>| v.iter().map(|&x| num(x, self.dec)).collect::<Vec<_>>();
        let (r, a) = (f(&rel.1), f(&abs.1));
        let len = |c: char, v: &Vec<String>| v.iter().map(|s| s.len() + 1).sum::<usize>() + usize::from(c != self.last);
        if len(abs.0, &a) < len(rel.0, &r) {
            self.emit(abs.0, a);
        } else {
            self.emit(rel.0, r);
        }
    }
}

fn path_data(cmds: &[Cmd], dec: u32) -> String {
    let mut w = Writer { dec, out: String::new(), last: '\0', last_num: String::new() };
    let (mut cx, mut cy, mut sx, mut sy) = (0i64, 0i64, 0i64, 0i64);
    let mut first = true;
    let q = |v: i64| {
        let step = 10i64.pow(3 - dec);
        (v as f64 / step as f64).round() as i64 * step
    };
    for &c in cmds {
        match c {
            Cmd::M(x, y) => {
                let (x, y) = (q(x), q(y));
                if first {
                    w.emit('M', vec![num(x, dec), num(y, dec)]);
                    first = false;
                } else {
                    w.either(('m', vec![x - cx, y - cy]), ('M', vec![x, y]));
                }
                (cx, cy, sx, sy) = (x, y, x, y);
            }
            Cmd::L(x, y) => {
                let (x, y) = (q(x), q(y));
                let (dx, dy) = (x - cx, y - cy);
                if dx == 0 && dy == 0 {
                    continue;
                }
                if dy == 0 {
                    w.either(('h', vec![dx]), ('H', vec![x]));
                } else if dx == 0 {
                    w.either(('v', vec![dy]), ('V', vec![y]));
                } else {
                    w.either(('l', vec![dx, dy]), ('L', vec![x, y]));
                }
                (cx, cy) = (x, y);
            }
            Cmd::C(a, b, c2, d, e, f) => {
                let p = [a, b, c2, d, e, f].map(q);
                let rel = vec![p[0] - cx, p[1] - cy, p[2] - cx, p[3] - cy, p[4] - cx, p[5] - cy];
                w.either(('c', rel), ('C', p.to_vec()));
                (cx, cy) = (p[4], p[5]);
            }
            Cmd::S(c2, d, e, f) => {
                let p = [c2, d, e, f].map(q);
                w.either(('s', vec![p[0] - cx, p[1] - cy, p[2] - cx, p[3] - cy]), ('S', p.to_vec()));
                (cx, cy) = (p[2], p[3]);
            }
            Cmd::Q(a, b, e, f) => {
                let p = [a, b, e, f].map(q);
                w.either(('q', vec![p[0] - cx, p[1] - cy, p[2] - cx, p[3] - cy]), ('Q', p.to_vec()));
                (cx, cy) = (p[2], p[3]);
            }
            Cmd::A { rx, ry, rot, large, sweep, x, y } => {
                let (x, y) = (q(x), q(y));
                let flags = |v: Vec<i64>| {
                    let mut s: Vec<String> = vec![num(q(rx), dec), num(q(ry), dec), num(rot, 3), (large as u8).to_string(), (sweep as u8).to_string()];
                    s.extend(v.iter().map(|&n| num(n, dec)));
                    s
                };
                let (r, a) = (flags(vec![x - cx, y - cy]), flags(vec![x, y]));
                let len = |v: &Vec<String>| v.iter().map(|s| s.len() + 1).sum::<usize>();
                if len(&a) < len(&r) {
                    w.emit('A', a);
                } else {
                    w.emit('a', r);
                }
                (cx, cy) = (x, y);
            }
            Cmd::Z => {
                w.emit('z', vec![]);
                (cx, cy) = (sx, sy);
            }
        }
    }
    w.out
}

fn hex(c: [u8; 4]) -> String {
    let h = format!("{:02x}{:02x}{:02x}", c[0], c[1], c[2]);
    let b = h.as_bytes();
    if b[0] == b[1] && b[2] == b[3] && b[4] == b[5] {
        format!("#{}{}{}", b[0] as char, b[2] as char, b[4] as char)
    } else {
        format!("#{h}")
    }
}

/// Decimal places that keep coordinates within about a twentieth of a pixel
/// at 4096 px across, so rounding never shows.
fn decimals(long_side: f64) -> u32 {
    let unit = long_side / 4096.0 / 20.0;
    (-unit.log10()).ceil().clamp(0.0, 3.0) as u32
}

pub fn tidy(text: &str, o: &Opts) -> Result<Tidied, String> {
    let (root, mut shapes) = read(text)?;
    let paths_in = shapes.len();
    // The background goes first, so its area can't sway which colours stay.
    let background = if o.edge_bg { remove_background(&mut shapes, &root.canvas, o.merge) } else { 0 };
    if shapes.is_empty() {
        return Err("nothing is left once the background is removed".into());
    }
    let (colours_in, palette) = choose_colours(&mut shapes, o);
    let colours_out = palette.pal.len();
    let groups = group(shapes);
    let dec = decimals(root.long_side);
    let all_evenodd = groups.iter().all(|g| g.evenodd);
    let mut svg = String::from("<svg xmlns=\"http://www.w3.org/2000/svg\"");
    for (k, v) in &root.attrs {
        svg.push_str(&format!(" {k}=\"{v}\""));
    }
    if all_evenodd {
        svg.push_str(" fill-rule=\"evenodd\"");
    }
    svg.push('>');
    for g in &groups {
        svg.push_str("<path");
        if g.fill[..3] != [0, 0, 0] {
            svg.push_str(&format!(" fill=\"{}\"", hex(g.fill)));
        }
        if g.fill[3] < 255 {
            svg.push_str(&format!(" fill-opacity=\"{}\"", num((g.fill[3] as i64 * 1000 + 127) / 255, 3)));
        }
        if g.evenodd && !all_evenodd {
            svg.push_str(" fill-rule=\"evenodd\"");
        }
        svg.push_str(&format!(" d=\"{}\"/>", path_data(&g.cmds, dec)));
    }
    svg.push_str("</svg>\n");
    Ok(Tidied { svg, paths_in, paths_out: groups.len(), colours_in, colours_out, background, palette })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Choices;

    #[test]
    fn numbers() {
        assert_eq!(num(1500, 3), "1.5");
        assert_eq!(num(-500, 3), "-.5");
        assert_eq!(num(0, 3), "0");
        assert_eq!(num(2000, 0), "2");
        assert_eq!(num(1499, 0), "1");
    }

    #[test]
    fn joins_and_shortens() {
        let svg = r##"<?xml version="1.0"?><svg viewBox="0 0 100 100" width="100%" height="100%">
            <path d="M0,0L100,0l0,100L0,100Z" fill="#FEFEFE"/>
            <path d="M10,10l5,0l0,5l-5,0Z" fill="#000000" transform="translate(1,1)"/>
            <path d="M50,50l5,0l0,5l-5,0Z" fill="#010101"/></svg>"##;
        let t = tidy(svg, &Opts::default()).unwrap();
        assert_eq!(t.paths_out, 2);
        assert_eq!(t.colours_out, 2);
        assert!(t.svg.contains(r#"<path d="M11 11h5v5h-5zm39 39h5v5h-5z"/>"#), "{}", t.svg);
    }

    #[test]
    fn keeps_overlapping_order() {
        // Red over black over red: the second red overlaps the black, so it can't join the first.
        let svg = r##"<svg viewBox="0 0 10 10"><path fill="red" d="M0 0h4v4H0z"/><path d="M2 2h4v4H2z"/><path fill="red" d="M3 3h4v4H3z"/></svg>"##;
        assert_eq!(tidy(svg, &Opts::default()).unwrap().paths_out, 3);
    }

    #[test]
    fn removes_edge_background_only() {
        // White canvas, black shape, white eye inside it, white blob on the edge.
        let svg = r##"<svg viewBox="0 0 100 100"><path fill="#fff" d="M0 0h100v100H0z"/><path d="M20 20h40v40H20z"/>
            <path fill="#fff" d="M30 30h5v5h-5z"/><path fill="#fefefe" d="M90 0h10v10H90z"/></svg>"##;
        let t = tidy(svg, &Opts { edge_bg: true, ..Opts::default() }).unwrap();
        assert_eq!(t.background, 2);
        assert!(t.svg.contains(r##"<path fill="#fff" d="M30 30h5v5h-5z"/>"##), "{}", t.svg);
    }

    #[test]
    fn locks_and_most_colours() {
        // Big white, mid grey, cream envelope, small yellow eye.
        let svg = r##"<svg viewBox="0 0 100 100"><path fill="#fefefe" d="M0 0h100v100H0z"/><path fill="#efece7" d="M10 10h30v30H10z"/>
            <path fill="#62615f" d="M50 50h20v20H50z"/><path fill="#fadf1c" d="M80 80h2v2h-2z"/></svg>"##;
        let o = Opts { merge: 80.0, edge_bg: true, ..Opts::default() };
        // The background goes first, so the cream envelope is no longer swallowed by white.
        let t = tidy(svg, &o).unwrap();
        assert!(t.palette.pal.contains(&[0xef, 0xec, 0xe7]), "{:?}", t.palette.pal);
        // Two colours, eye locked: grey folds away, eye and cream stay.
        let keep = Choices { keep: vec![[0xfa, 0xdf, 0x1c]], drop: vec![] };
        let t = tidy(svg, &Opts { merge: 0.0, max_col: 2, choices: keep, ..o.clone() }).unwrap();
        assert_eq!(t.palette.pal.len(), 2);
        assert!(t.palette.pal.contains(&[0xfa, 0xdf, 0x1c]));
        // The big grey went to the cream, not to the tiny locked eye.
        let eye = t.palette.pal.iter().position(|c| *c == [0xfa, 0xdf, 0x1c]).unwrap();
        assert!(t.palette.share[eye] < 0.01, "{:?}", t.palette.share);
        // Left out: the grey takes the nearest kept colour.
        let drop = Choices { keep: vec![], drop: vec![[0x62, 0x61, 0x5f]] };
        let t = tidy(svg, &Opts { merge: 0.0, choices: drop, ..o }).unwrap();
        assert!(!t.palette.pal.contains(&[0x62, 0x61, 0x5f]));
        assert_eq!(t.palette.removed.len(), 1);
        // Locking a colour that merging folded away brings it back.
        let keep = Choices { keep: vec![[0xef, 0xec, 0xe7]], drop: vec![] };
        let t = tidy(svg, &Opts { merge: 120.0, edge_bg: false, choices: keep, ..Opts::default() }).unwrap();
        assert!(t.palette.pal.contains(&[0xef, 0xec, 0xe7]), "{:?}", t.palette.pal);
    }

    #[test]
    fn declines_strokes() {
        let svg = r##"<svg viewBox="0 0 10 10"><path stroke="#000" d="M0 0h4"/></svg>"##;
        assert!(tidy(svg, &Opts::default()).is_err());
    }
}
