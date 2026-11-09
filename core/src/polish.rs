//! Polish: nudge an AI-drawn SVG towards the original it was drawn from,
//! without changing what it is made of or bending the shapes it draws.
//!
//! The SVG's text is kept and only its numbers are rewritten. What may change
//! comes in tiers, so a drawing gets closer by moving its parts rather than by
//! reshaping them, which is what a score alone would rather do: a letter
//! nudged point by point into whatever covers the most pixels stops looking
//! like the letter.
//!
//! 1. Whole elements and groups, as rigid pieces: moved, scaled, turned and
//!    stretched a little. A blurred glow sitting on a shape moves with it.
//! 2. The separate outlines of one path (a letter's counter, a dot, a hole):
//!    moved and scaled within it.
//! 3. Paint: colours, opacities, gradient stops and geometry, and stroke
//!    widths, by at most 12% so a line can't thin out to dodge pixels.
//! 4. Single points of a path, last and least: a point moves with its control
//!    handles, by at most 1.2% of the picture and 4% of its outline's size,
//!    and points joined by a level or upright line move together, so straight
//!    edges stay straight. A change here has to earn more than elsewhere.
//!
//! Filters are never touched, and nothing inside a blurred element changes
//! shape. A last pass puts back any change the drawing does as well without.
//!
//! The caller renders: the search is an ask/tell loop. [`Polish::ask`] hands
//! out SVG texts sized to the comparison raster; the caller draws each (the
//! web app with the browser's canvas, the CLI with resvg) and passes the
//! pixels, straight-alpha RGBA, to [`Polish::tell`] in the same order. Renders
//! are scored as AI rounds are: CIE76 ΔE, the worse of over white and over
//! black, averaged over the pixels either image covers; only pixels that
//! differ from the current best's render are scored again. Several texts can
//! be asked for at once (the CLI renders them in parallel); they assume none
//! of them is an improvement, so after one is, the rest are dropped.

type Aff = [f64; 6]; // x' = a x + c y + e, y' = b x + d y + f

const ID: Aff = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

fn mul(m: &Aff, n: &Aff) -> Aff {
    [
        m[0] * n[0] + m[2] * n[1],
        m[1] * n[0] + m[3] * n[1],
        m[0] * n[2] + m[2] * n[3],
        m[1] * n[2] + m[3] * n[3],
        m[0] * n[4] + m[2] * n[5] + m[4],
        m[1] * n[4] + m[3] * n[5] + m[5],
    ]
}

fn apply(m: &Aff, p: [f64; 2]) -> [f64; 2] {
    [m[0] * p[0] + m[2] * p[1] + m[4], m[1] * p[0] + m[3] * p[1] + m[5]]
}

fn scale_of(m: &Aff) -> f64 {
    (m[0] * m[3] - m[1] * m[2]).abs().sqrt().max(1e-9)
}

/// An SVG transform list as one matrix (identity where it can't be read).
fn parse_transform(s: &str) -> Aff {
    let b = s.as_bytes();
    let mut m = ID;
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let ns = i;
        while i < b.len() && b[i].is_ascii_alphabetic() {
            i += 1;
        }
        let name = &s[ns..i];
        let Some(open) = s[i..].find('(').map(|k| i + k) else { return ID };
        let Some(close) = s[open..].find(')').map(|k| open + k) else { return ID };
        let mut v = Vec::new();
        let mut j = open + 1;
        while j < close {
            match num_at(b, j) {
                Some((e, x)) if e <= close => {
                    v.push(x);
                    j = e;
                }
                _ => j += 1,
            }
        }
        i = close + 1;
        let g = |k: usize, d: f64| v.get(k).copied().unwrap_or(d);
        let t: Aff = match name {
            "matrix" if v.len() == 6 => [v[0], v[1], v[2], v[3], v[4], v[5]],
            "translate" => [1.0, 0.0, 0.0, 1.0, g(0, 0.0), g(1, 0.0)],
            "scale" => [g(0, 1.0), 0.0, 0.0, g(1, g(0, 1.0)), 0.0, 0.0],
            "rotate" => {
                let (si, co) = g(0, 0.0).to_radians().sin_cos();
                let (cx, cy) = (g(1, 0.0), g(2, 0.0));
                let r = [co, si, -si, co, 0.0, 0.0];
                mul(&mul(&[1.0, 0.0, 0.0, 1.0, cx, cy], &r), &[1.0, 0.0, 0.0, 1.0, -cx, -cy])
            }
            "skewX" => [1.0, 0.0, g(0, 0.0).to_radians().tan(), 1.0, 0.0, 0.0],
            "skewY" => [1.0, g(0, 0.0).to_radians().tan(), 0.0, 1.0, 0.0, 0.0],
            _ => return ID,
        };
        m = mul(&m, &t);
    }
    m
}

/// A box as (x0, y0, x1, y1).
type BBox = [f64; 4];

fn bbox_of(pts: impl Iterator<Item = [f64; 2]>) -> Option<BBox> {
    let mut b: Option<BBox> = None;
    for p in pts {
        b = Some(match b {
            None => [p[0], p[1], p[0], p[1]],
            Some(o) => [o[0].min(p[0]), o[1].min(p[1]), o[2].max(p[0]), o[3].max(p[1])],
        });
    }
    b
}

fn bbox_map(m: &Aff, b: &BBox) -> BBox {
    bbox_of([[b[0], b[1]], [b[2], b[1]], [b[0], b[3]], [b[2], b[3]]].into_iter().map(|p| apply(m, p))).unwrap()
}

fn bbox_union(a: Option<BBox>, b: BBox) -> BBox {
    match a {
        None => b,
        Some(o) => [o[0].min(b[0]), o[1].min(b[1]), o[2].max(b[2]), o[3].max(b[3])],
    }
}

fn centre(b: &BBox) -> [f64; 2] {
    [(b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0]
}

fn half_diag(b: &BBox) -> f64 {
    ((b[2] - b[0]).powi(2) + (b[3] - b[1]).powi(2)).sqrt() / 2.0
}

// ---------------------------------------------------------------- scanning

struct Attr {
    name: String,
    vs: usize,
    ve: usize,
}

struct Elem {
    name: String,
    name_end: usize,
    parent: Option<usize>,
    children: Vec<usize>,
    attrs: Vec<Attr>,
    /// Inside defs, gradients, masks, clip paths, symbols: not drawn where it stands.
    in_defs: bool,
    in_filter: bool,
}

impl Elem {
    fn attr<'a>(&self, text: &'a str, name: &str) -> Option<&'a str> {
        self.attrs.iter().find(|a| a.name == name).map(|a| &text[a.vs..a.ve])
    }
    fn attr_at(&self, name: &str) -> Option<&Attr> {
        self.attrs.iter().find(|a| a.name == name)
    }
}

const DEFS: [&str; 9] = ["defs", "linearGradient", "radialGradient", "mask", "clipPath", "pattern", "symbol", "marker", "filter"];

/// Every element with its attributes' value spans. Comments, CDATA and the
/// bodies of `<style>` and `<script>` are skipped.
fn scan(text: &str) -> Vec<Elem> {
    let b = text.as_bytes();
    let mut out: Vec<Elem> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut i = 0;
    let find = |from: usize, pat: &str| text[from..].find(pat).map(|k| from + k + pat.len()).unwrap_or(b.len());
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        if text[i..].starts_with("<!--") {
            i = find(i, "-->");
        } else if text[i..].starts_with("<![CDATA[") {
            i = find(i, "]]>");
        } else if text[i..].starts_with("<?") {
            i = find(i, "?>");
        } else if text[i..].starts_with("<!") {
            i = find(i, ">");
        } else if text[i..].starts_with("</") {
            stack.pop();
            i = find(i, ">");
        } else {
            let mut j = i + 1;
            while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'>' && b[j] != b'/' {
                j += 1;
            }
            let name = text[i + 1..j].to_string();
            let name_end = j;
            let mut attrs = Vec::new();
            let mut closed = false;
            loop {
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j >= b.len() {
                    break;
                }
                if b[j] == b'>' {
                    j += 1;
                    break;
                }
                if b[j] == b'/' {
                    closed = true;
                    j = find(j, ">");
                    break;
                }
                let ns = j;
                while j < b.len() && b[j] != b'=' && !b[j].is_ascii_whitespace() && b[j] != b'>' {
                    j += 1;
                }
                let an = text[ns..j].to_string();
                while j < b.len() && (b[j].is_ascii_whitespace() || b[j] == b'=') {
                    j += 1;
                }
                if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                    let q = b[j];
                    let vs = j + 1;
                    let mut k = vs;
                    while k < b.len() && b[k] != q {
                        k += 1;
                    }
                    attrs.push(Attr { name: an, vs, ve: k });
                    j = k + 1;
                } else if ns == j {
                    j += 1;
                }
            }
            let parent = stack.last().copied();
            let pd = parent.map(|p| out[p].in_defs).unwrap_or(false);
            let pf = parent.map(|p| out[p].in_filter).unwrap_or(false);
            let in_defs = pd || DEFS.contains(&name.as_str());
            let in_filter = pf || name == "filter";
            let idx = out.len();
            if let Some(p) = parent {
                out[p].children.push(idx);
            }
            let skip_body = name == "style" || name == "script";
            out.push(Elem { name, name_end, parent, children: Vec::new(), attrs, in_defs, in_filter });
            i = j;
            if skip_body && !closed {
                i = find(i, "</");
                i = find(i, ">");
            } else if !closed {
                stack.push(idx);
            }
        }
    }
    out
}

// ---------------------------------------------------------------- numbers

fn num_at(b: &[u8], mut i: usize) -> Option<(usize, f64)> {
    let s = i;
    if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        i += 1;
    }
    let ds = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i == ds || (i == ds + 1 && b[ds] == b'.') {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut k = i + 1;
        if k < b.len() && (b[k] == b'-' || b[k] == b'+') {
            k += 1;
        }
        if k < b.len() && b[k].is_ascii_digit() {
            while k < b.len() && b[k].is_ascii_digit() {
                k += 1;
            }
            i = k;
        }
    }
    std::str::from_utf8(&b[s..i]).ok()?.parse().ok().map(|v| (i, v))
}

/// A whole attribute value that is one plain number (no unit, no percentage).
fn plain_num(b: &[u8], vs: usize, ve: usize) -> Option<(usize, usize, f64)> {
    let mut s = vs;
    while s < ve && b[s].is_ascii_whitespace() {
        s += 1;
    }
    let (e, v) = num_at(b, s)?;
    let mut k = e;
    while k < ve && b[k].is_ascii_whitespace() {
        k += 1;
    }
    (e <= ve && k == ve).then_some((s, e, v))
}

fn fmt_num(v: f64, dec: u32) -> String {
    let p = 10f64.powi(dec as i32);
    let r = (v * p).round() / p;
    let mut s = format!("{:.*}", dec as usize, r);
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    if s == "-0" {
        s = "0".into();
    }
    s
}

/// Decimals that keep rounding under 0.05 raster px, for numbers at `ppu`
/// raster px per unit.
fn decimals(ppu: f64) -> u32 {
    let u = 0.05 / ppu.max(1e-9);
    if u >= 1.0 {
        0
    } else {
        ((-u.log10()).ceil() as u32).min(4)
    }
}

#[derive(Clone, Copy)]
struct Span {
    s: usize,
    e: usize,
    v: f64,
}

/// Rewrites collected while building a text: (start, end, new text, numeric).
type Edits = Vec<(usize, usize, String, bool)>;

/// Write `target` over the number at `sp`, unless it rounds to the number
/// already there. Returns the value the text will hold.
fn put(out: &mut Edits, sp: Span, target: f64, dec: u32) -> f64 {
    if (target - sp.v).abs() <= 0.5 * 10f64.powi(-(dec as i32)) + 1e-9 {
        return sp.v;
    }
    let t = fmt_num(target, dec);
    let v = t.parse().unwrap_or(target);
    out.push((sp.s, sp.e, t, true));
    v
}

// ---------------------------------------------------------------- shapes

#[derive(Clone, Copy)]
enum Role {
    X(usize),
    Y(usize),
    Len(usize),
}

#[derive(Clone, Copy, PartialEq)]
enum Form {
    Path,
    Circle,
    Ellipse,
    Rect,
    Line,
    Poly,
}

/// One path segment: its command (upper case), whether written relative,
/// the points it places and its numbers' spans, flags left out.
struct Seg {
    cmd: u8,
    rel: bool,
    pts: Vec<usize>,
    nums: Vec<Span>,
    /// Arcs: index of rx in `lens` (ry follows) and of the angle in `angs`.
    arc: (usize, usize),
}

/// The geometry of one drawn element, in its own user space.
struct Shape {
    elem: usize,
    form: Form,
    pts: Vec<[f64; 2]>,
    /// Outline (subpath) of each point.
    pt_sub: Vec<usize>,
    lens: Vec<f64>,
    /// How each length scales: 0 with the area, 1 along x, 2 along y.
    len_axis: Vec<u8>,
    len_sub: Vec<usize>,
    angs: Vec<f64>,
    segs: Vec<Seg>,
    nums: Vec<(Span, Role)>,
    nsub: usize,
    /// Has level or upright lines (H/V), which can't be turned.
    hv: bool,
    /// Every number a move needs is written out (a missing cx defaults to 0
    /// and can't be rewritten).
    complete: bool,
    ppu: f64,
    dec: u32,
    /// The mover whose moves are written into these numbers.
    mover: Option<usize>,
    /// Its outlines' entries in `Problem::subs`, when it has several.
    sub0: Option<usize>,
    classes: Vec<usize>,
}

fn path_shape(b: &[u8], vs: usize, ve: usize) -> Option<Shape> {
    let mut sh = Shape {
        elem: 0,
        form: Form::Path,
        pts: vec![],
        pt_sub: vec![],
        lens: vec![],
        len_axis: vec![],
        len_sub: vec![],
        angs: vec![],
        segs: vec![],
        nums: vec![],
        nsub: 0,
        hv: false,
        complete: true,
        ppu: 1.0,
        dec: 2,
        mover: None,
        sub0: None,
        classes: vec![],
    };
    let (mut i, mut cmd) = (vs, 0u8);
    let (mut cur, mut start) = ([0.0f64, 0.0], [0.0f64, 0.0]);
    let need = |c: u8| match c {
        b'M' | b'L' | b'T' => 2,
        b'H' | b'V' => 1,
        b'C' => 6,
        b'S' | b'Q' => 4,
        b'A' => 7,
        _ => 0,
    };
    loop {
        while i < ve && (b[i].is_ascii_whitespace() || b[i] == b',') {
            i += 1;
        }
        if i >= ve {
            break;
        }
        let c = b[i];
        if c.is_ascii_alphabetic() && c != b'e' && c != b'E' {
            cmd = c;
            i += 1;
            if c.to_ascii_uppercase() == b'Z' {
                sh.segs.push(Seg { cmd: b'Z', rel: false, pts: vec![], nums: vec![], arc: (0, 0) });
                cur = start;
            }
            continue;
        }
        let up = cmd.to_ascii_uppercase();
        if up == 0 || up == b'Z' || !b"MLHVCSQTA".contains(&up) {
            return None;
        }
        let rel = cmd.is_ascii_lowercase();
        let mut v = [0f64; 7];
        let mut spans = Vec::with_capacity(7);
        for (k, slot) in v.iter_mut().enumerate().take(need(up)) {
            while i < ve && (b[i].is_ascii_whitespace() || b[i] == b',') {
                i += 1;
            }
            if i >= ve {
                return None;
            }
            if up == b'A' && (k == 3 || k == 4) {
                // flag: a single 0 or 1
                *slot = (b[i] == b'1') as u8 as f64;
                i += 1;
                continue;
            }
            let (e, x) = num_at(b, i)?;
            if e > ve {
                return None;
            }
            *slot = x;
            spans.push(Span { s: i, e, v: x });
            i = e;
        }
        let base = cur;
        let abs = |x: f64, y: f64| if rel { [base[0] + x, base[1] + y] } else { [x, y] };
        let add = |sh: &mut Shape, p: [f64; 2]| {
            sh.pts.push(p);
            sh.pt_sub.push(sh.nsub.saturating_sub(1));
            sh.pts.len() - 1
        };
        let mut seg = Seg { cmd: up, rel, pts: vec![], nums: spans, arc: (0, 0) };
        match up {
            b'M' => {
                sh.nsub += 1;
                let p = abs(v[0], v[1]);
                seg.pts.push(add(&mut sh, p));
                start = p;
                cur = p;
                // Further pairs after M are lines.
                cmd = if rel { b'l' } else { b'L' };
            }
            b'L' | b'T' => {
                let p = abs(v[0], v[1]);
                seg.pts.push(add(&mut sh, p));
                cur = p;
            }
            b'H' => {
                let p = [if rel { base[0] + v[0] } else { v[0] }, base[1]];
                seg.pts.push(add(&mut sh, p));
                cur = p;
                sh.hv = true;
            }
            b'V' => {
                let p = [base[0], if rel { base[1] + v[0] } else { v[0] }];
                seg.pts.push(add(&mut sh, p));
                cur = p;
                sh.hv = true;
            }
            b'C' => {
                for k in 0..3 {
                    let p = abs(v[2 * k], v[2 * k + 1]);
                    seg.pts.push(add(&mut sh, p));
                }
                cur = sh.pts[*seg.pts.last().unwrap()];
            }
            b'S' | b'Q' => {
                for k in 0..2 {
                    let p = abs(v[2 * k], v[2 * k + 1]);
                    seg.pts.push(add(&mut sh, p));
                }
                cur = sh.pts[*seg.pts.last().unwrap()];
            }
            _ => {
                // A
                let s = sh.nsub.saturating_sub(1);
                seg.arc = (sh.lens.len(), sh.angs.len());
                sh.lens.extend([v[0], v[1]]);
                sh.len_axis.extend([1, 2]);
                sh.len_sub.extend([s, s]);
                sh.angs.push(v[2]);
                let p = abs(v[5], v[6]);
                seg.pts.push(add(&mut sh, p));
                cur = p;
            }
        }
        sh.segs.push(seg);
    }
    (!sh.pts.is_empty()).then_some(sh)
}

/// The path's numbers for points `q`, lengths and angles, as edits. Relative
/// numbers are measured from where the text written so far puts the pen, so
/// rounding never adds up along the path.
fn write_path(sh: &Shape, q: &[[f64; 2]], lens: &[f64], angs: &[f64], out: &mut Edits) {
    let dec = sh.dec;
    let (mut cur, mut start) = ([0.0f64, 0.0], [0.0f64, 0.0]);
    for s in &sh.segs {
        let off = if s.rel { cur } else { [0.0, 0.0] };
        let pt = |out: &mut Edits, k: usize, p: usize| -> [f64; 2] {
            [put(out, s.nums[k], q[p][0] - off[0], dec) + off[0], put(out, s.nums[k + 1], q[p][1] - off[1], dec) + off[1]]
        };
        match s.cmd {
            b'M' => {
                cur = pt(out, 0, s.pts[0]);
                start = cur;
            }
            b'L' | b'T' => cur = pt(out, 0, s.pts[0]),
            b'H' => cur[0] = put(out, s.nums[0], q[s.pts[0]][0] - off[0], dec) + off[0],
            b'V' => cur[1] = put(out, s.nums[0], q[s.pts[0]][1] - off[1], dec) + off[1],
            b'C' => {
                pt(out, 0, s.pts[0]);
                pt(out, 2, s.pts[1]);
                cur = pt(out, 4, s.pts[2]);
            }
            b'S' | b'Q' => {
                pt(out, 0, s.pts[0]);
                cur = pt(out, 2, s.pts[1]);
            }
            b'A' => {
                put(out, s.nums[0], lens[s.arc.0], dec);
                put(out, s.nums[1], lens[s.arc.0 + 1], dec);
                put(out, s.nums[2], angs[s.arc.1], 1);
                cur = pt(out, 3, s.pts[0]);
            }
            _ => cur = start,
        }
    }
}

/// A basic shape's geometry, from its attributes.
fn basic_shape(e: &Elem, text: &str) -> Option<Shape> {
    let b = text.as_bytes();
    let form = match e.name.as_str() {
        "circle" => Form::Circle,
        "ellipse" => Form::Ellipse,
        "rect" => Form::Rect,
        "line" => Form::Line,
        "polygon" | "polyline" => Form::Poly,
        _ => return None,
    };
    let mut sh = path_shape(b"M0 0", 0, 4).unwrap();
    sh.form = form;
    sh.pts.clear();
    sh.pt_sub.clear();
    sh.segs.clear();
    sh.nsub = 1;
    let get = |n: &str| -> Option<Option<Span>> {
        match e.attr_at(n) {
            None => Some(None),
            Some(a) => plain_num(b, a.vs, a.ve).map(|(s, en, v)| Some(Span { s, e: en, v })),
        }
    };
    let complete = std::cell::Cell::new(true);
    let pt = |sh: &mut Shape, x: &str, y: &str| -> Option<()> {
        let (sx, sy) = (get(x)?, get(y)?);
        complete.set(complete.get() && sx.is_some() && sy.is_some());
        let p = sh.pts.len();
        sh.pts.push([sx.map_or(0.0, |s| s.v), sy.map_or(0.0, |s| s.v)]);
        sh.pt_sub.push(0);
        if let Some(s) = sx {
            sh.nums.push((s, Role::X(p)));
        }
        if let Some(s) = sy {
            sh.nums.push((s, Role::Y(p)));
        }
        Some(())
    };
    let len = |sh: &mut Shape, n: &str, axis: u8, need: bool| -> Option<f64> {
        let s = get(n)?;
        if need {
            complete.set(complete.get() && s.is_some());
        }
        let k = sh.lens.len();
        let v = s.map_or(0.0, |s| s.v);
        sh.lens.push(v);
        sh.len_axis.push(axis);
        sh.len_sub.push(0);
        if let Some(s) = s {
            sh.nums.push((s, Role::Len(k)));
        }
        Some(v)
    };
    match form {
        Form::Circle => {
            pt(&mut sh, "cx", "cy")?;
            len(&mut sh, "r", 0, true)?;
        }
        Form::Ellipse => {
            pt(&mut sh, "cx", "cy")?;
            len(&mut sh, "rx", 1, true)?;
            len(&mut sh, "ry", 2, true)?;
        }
        Form::Rect => {
            pt(&mut sh, "x", "y")?;
            len(&mut sh, "width", 1, true)?;
            len(&mut sh, "height", 2, true)?;
            len(&mut sh, "rx", 1, false)?;
            len(&mut sh, "ry", 2, false)?;
        }
        Form::Line => {
            pt(&mut sh, "x1", "y1")?;
            pt(&mut sh, "x2", "y2")?;
        }
        _ => {
            let a = e.attr_at("points")?;
            let mut i = a.vs;
            let mut nums = Vec::new();
            while i < a.ve {
                if let Some((en, v)) = num_at(b, i) {
                    nums.push(Span { s: i, e: en, v });
                    i = en;
                } else if b[i].is_ascii_whitespace() || b[i] == b',' {
                    i += 1;
                } else {
                    return None;
                }
            }
            for xy in nums.chunks_exact(2) {
                let p = sh.pts.len();
                sh.pts.push([xy[0].v, xy[1].v]);
                sh.pt_sub.push(0);
                sh.nums.push((xy[0], Role::X(p)));
                sh.nums.push((xy[1], Role::Y(p)));
            }
        }
    }
    sh.complete = complete.get();
    (!sh.pts.is_empty()).then_some(sh)
}

fn shape_bbox(sh: &Shape) -> Option<BBox> {
    match sh.form {
        Form::Circle => {
            let (c, r) = (sh.pts[0], sh.lens[0]);
            (r > 0.0).then_some([c[0] - r, c[1] - r, c[0] + r, c[1] + r])
        }
        Form::Ellipse => {
            let (c, rx, ry) = (sh.pts[0], sh.lens[0], sh.lens[1]);
            (rx > 0.0 && ry > 0.0).then_some([c[0] - rx, c[1] - ry, c[0] + rx, c[1] + ry])
        }
        Form::Rect => {
            let (p, w, h) = (sh.pts[0], sh.lens[0], sh.lens[1]);
            (w > 0.0 && h > 0.0).then_some([p[0], p[1], p[0] + w, p[1] + h])
        }
        _ => bbox_of(sh.pts.iter().copied()),
    }
}

// ---------------------------------------------------------------- the problem

/// A whole element or group that moves as one piece: translate (in its
/// parent's units), log scale, turn (radians), log stretch, about `c`.
struct Mover {
    elem: usize,
    /// Where its transform prefix goes: (position, whether a new attribute).
    /// None when the move is written into its own numbers instead.
    prefix: Option<(usize, bool)>,
    shape: Option<usize>,
    c: [f64; 2],
    /// Raster px per parent unit, and the element's half-diagonal in px.
    ppu: f64,
    half: f64,
    rot: bool,
    asp: bool,
    /// A filtered element with nothing to follow: moves very little.
    tight: bool,
    /// Copies another piece's move instead of having its own: (leader, px ratio).
    follows: Option<(Lead, f64)>,
    dec: u32,
    /// Raster position of its centre, for pairing glows with shapes.
    at: [f64; 2],
}

#[derive(Clone, Copy, PartialEq)]
enum Lead {
    Mover(usize),
    /// Joined at a seam: takes the whole move, about the same centre.
    Twin(usize),
    Sub(usize),
}

/// One outline of a path with several.
struct Sub {
    c: [f64; 2],
    ppu: f64,
    half: f64,
    at: [f64; 2],
    /// Part of line art (see `setup`): moves half as far.
    line: bool,
}

/// Path points that move together along one axis, with how much of the move
/// each takes (control handles follow their point; a quadratic's single
/// handle takes half of each end's move).
struct Class {
    axis: usize,
    members: Vec<(usize, f64)>,
    /// Furthest it may go, in the shape's units.
    cap: f64,
    ppu: f64,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// User-space position in a gradient.
    Coord,
    /// A stroke width.
    Width,
    /// Bounded plain number (offsets, opacities, bounding-box units).
    Unit { lo: f64, hi: f64, step: f64, cap: f64 },
    Colour,
    Angle,
    /// Linear part of scale()/matrix() in a gradientTransform.
    Mat,
}

/// A paint number in the text.
struct Slot {
    s: usize,
    e: usize,
    kind: Kind,
    orig: [f64; 3],
    ppu: f64,
}

#[derive(Clone)]
struct State {
    mv: Vec<[f64; 5]>,
    sub: Vec<[f64; 3]>,
    node: Vec<f64>,
    num: Vec<[f64; 3]>,
}

struct Problem {
    text: String,
    /// Edits that size the root to the raster, for rendering.
    sizing: Edits,
    shapes: Vec<Shape>,
    movers: Vec<Mover>,
    subs: Vec<Sub>,
    classes: Vec<Class>,
    slots: Vec<Slot>,
    /// Movers that copy each mover's or outline's move.
    followers_m: Vec<Vec<usize>>,
    followers_s: Vec<Vec<usize>>,
    w: usize,
    h: usize,
    long: f64,
}

/// The move about `c`: translate t, then turn and scale (sx, sy) about c.
fn move_aff(c: [f64; 2], p: &[f64; 5]) -> Aff {
    let s = p[2].exp();
    let (sx, sy) = (s * p[4].exp(), s * (-p[4]).exp());
    let (co, si) = (p[3].cos(), p[3].sin());
    let l = [co * sx, si * sx, -si * sy, co * sy, 0.0, 0.0];
    let rc = apply(&l, c);
    [l[0], l[1], l[2], l[3], c[0] + p[0] - rc[0], c[1] + p[1] - rc[1]]
}

fn is_identity(p: &[f64]) -> bool {
    p.iter().all(|v| *v == 0.0)
}

impl Problem {
    fn state0(&self) -> State {
        State {
            mv: vec![[0.0; 5]; self.movers.len()],
            sub: vec![[0.0; 3]; self.subs.len()],
            node: vec![0.0; self.classes.len()],
            num: self.slots.iter().map(|s| s.orig).collect(),
        }
    }

    fn edits(&self, st: &State, out: &mut Edits) {
        for (k, sh) in self.shapes.iter().enumerate() {
            self.write_shape(k, sh, st, out);
        }
        for (i, s) in self.slots.iter().enumerate() {
            let v = &st.num[i];
            if *v == s.orig {
                continue;
            }
            let t = match s.kind {
                Kind::Coord | Kind::Width => fmt_num(v[0], decimals(s.ppu)),
                Kind::Angle => fmt_num(v[0], 1),
                Kind::Mat => fmt_num(v[0], 4),
                Kind::Unit { step, .. } => fmt_num(v[0], if step >= 1.0 { 1 } else { 3 }),
                Kind::Colour => {
                    let c: Vec<u8> = v.iter().map(|x| x.round().clamp(0.0, 255.0) as u8).collect();
                    if c.iter().all(|x| x % 17 == 0) {
                        format!("#{:x}{:x}{:x}", c[0] / 17, c[1] / 17, c[2] / 17)
                    } else {
                        format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
                    }
                }
            };
            out.push((s.s, s.e, t, !matches!(s.kind, Kind::Colour)));
        }
        for (m, mo) in self.movers.iter().enumerate() {
            let Some((at, insert)) = mo.prefix else { continue };
            let p = &st.mv[m];
            if is_identity(p) {
                continue;
            }
            let a = move_aff(mo.c, p);
            let s = p[2].exp();
            let (sx, sy) = (s * p[4].exp(), s * (-p[4]).exp());
            let mut t = format!("translate({} {})", fmt_num(a[4], mo.dec), fmt_num(a[5], mo.dec));
            if p[3] != 0.0 {
                t += &format!(" rotate({})", fmt_num(p[3].to_degrees(), 2));
            }
            if (sx - 1.0).abs() > 5e-5 || (sy - 1.0).abs() > 5e-5 {
                if (sx - sy).abs() < 5e-5 {
                    t += &format!(" scale({})", fmt_num(sx, 4));
                } else {
                    t += &format!(" scale({} {})", fmt_num(sx, 4), fmt_num(sy, 4));
                }
            }
            out.push((at, at, if insert { format!(" transform=\"{t}\"") } else { t + " " }, false));
        }
    }

    fn write_shape(&self, k: usize, sh: &Shape, st: &State, out: &mut Edits) {
        let moved = sh.classes.iter().any(|&c| st.node[c] != 0.0)
            || sh.sub0.is_some_and(|s0| (0..sh.nsub).any(|i| !is_identity(&st.sub[s0 + i])))
            || sh.mover.is_some_and(|m| !is_identity(&st.mv[m]));
        if !moved {
            return;
        }
        let mut q = sh.pts.clone();
        for &c in &sh.classes {
            let d = st.node[c];
            if d != 0.0 {
                let cl = &self.classes[c];
                for &(p, w) in &cl.members {
                    q[p][cl.axis] += d * w;
                }
            }
        }
        let mut lens = sh.lens.clone();
        let mut angs = sh.angs.clone();
        if let Some(s0) = sh.sub0 {
            let affs: Vec<Option<Aff>> = (0..sh.nsub)
                .map(|i| {
                    let p = &st.sub[s0 + i];
                    (!is_identity(p)).then(|| move_aff(self.subs[s0 + i].c, &[p[0], p[1], p[2], 0.0, 0.0]))
                })
                .collect();
            for (i, p) in q.iter_mut().enumerate() {
                if let Some(a) = &affs[sh.pt_sub[i]] {
                    *p = apply(a, *p);
                }
            }
            for (i, l) in lens.iter_mut().enumerate() {
                *l *= st.sub[s0 + sh.len_sub[i]][2].exp();
            }
        }
        if let Some(m) = sh.mover {
            let p = &st.mv[m];
            if !is_identity(p) {
                let a = move_aff(self.movers[m].c, p);
                for v in q.iter_mut() {
                    *v = apply(&a, *v);
                }
                for (i, l) in lens.iter_mut().enumerate() {
                    *l *= match sh.len_axis[i] {
                        0 => (a[0] * a[3] - a[1] * a[2]).abs().sqrt(),
                        1 => a[0].hypot(a[1]),
                        _ => a[2].hypot(a[3]),
                    };
                }
                for g in angs.iter_mut() {
                    *g += p[3].to_degrees();
                }
            }
        }
        let _ = k;
        if sh.form == Form::Path {
            write_path(sh, &q, &lens, &angs, out);
        } else {
            for &(sp, role) in &sh.nums {
                let t = match role {
                    Role::X(p) => q[p][0],
                    Role::Y(p) => q[p][1],
                    Role::Len(l) => lens[l],
                };
                put(out, sp, t, sh.dec);
            }
        }
    }

    /// The SVG text for a state; `sized` for rendering at the raster size.
    fn build(&self, st: &State, sized: bool) -> String {
        let mut ed: Edits = Vec::new();
        if sized {
            ed.extend(self.sizing.iter().cloned());
        }
        self.edits(st, &mut ed);
        // A prefix goes before a number starting at the same place.
        ed.sort_by_key(|e| (e.0, e.1 != e.0));
        let b = self.text.as_bytes();
        let mut out = String::with_capacity(self.text.len() + 64);
        let mut prev = 0;
        for (s, e, t, numeric) in ed {
            if s < prev {
                continue;
            }
            out.push_str(&self.text[prev..s]);
            if numeric && !t.starts_with('-') && out.as_bytes().last().is_some_and(|c| c.is_ascii_digit() || *c == b'.') {
                out.push(' ');
            }
            out.push_str(&t);
            if numeric && e < b.len() && (b[e].is_ascii_digit() || b[e] == b'.') {
                out.push(' ');
            }
            prev = e;
        }
        out.push_str(&self.text[prev..]);
        out
    }
}

fn hex_colours(t: &str, vs: usize, ve: usize) -> Vec<(usize, usize, [f64; 3])> {
    let b = t.as_bytes();
    let mut out = Vec::new();
    let mut i = vs;
    while i < ve {
        if b[i] == b'#' {
            let mut e = i + 1;
            while e < ve && b[e].is_ascii_hexdigit() {
                e += 1;
            }
            let h = &t[i + 1..e];
            let v = |s: &str| u8::from_str_radix(s, 16).unwrap_or(0) as f64;
            match h.len() {
                3 => out.push((i, e, [v(&h[0..1]) * 17.0, v(&h[1..2]) * 17.0, v(&h[2..3]) * 17.0])),
                6 => out.push((i, e, [v(&h[0..2]), v(&h[2..4]), v(&h[4..6])])),
                _ => {}
            }
            i = e;
        } else {
            i += 1;
        }
    }
    out
}

fn transform_nums(b: &[u8], vs: usize, ve: usize) -> Vec<(usize, usize, f64, Kind)> {
    let mut out = Vec::new();
    let (mut i, mut f, mut k) = (vs, String::new(), 0usize);
    while i < ve {
        let c = b[i];
        if c.is_ascii_alphabetic() {
            let s = i;
            while i < ve && b[i].is_ascii_alphabetic() {
                i += 1;
            }
            f = String::from_utf8_lossy(&b[s..i]).into_owned();
            k = 0;
            continue;
        }
        if let Some((e, v)) = num_at(b, i) {
            let kind = match (f.as_str(), k) {
                ("scale", _) | ("matrix", 0..=3) => Kind::Mat,
                ("rotate", 0) | ("skewX", _) | ("skewY", _) => Kind::Angle,
                _ => Kind::Coord,
            };
            out.push((i, e, v, kind));
            k += 1;
            i = e;
        } else {
            i += 1;
        }
    }
    out
}

const PIECES: [&str; 9] = ["path", "circle", "ellipse", "rect", "line", "polygon", "polyline", "use", "g"];

fn href(e: &Elem, text: &str) -> Option<String> {
    let v = e.attr(text, "href").or_else(|| e.attr(text, "xlink:href"))?;
    v.trim().strip_prefix('#').map(|s| s.to_string())
}

fn has_filter(e: &Elem, text: &str) -> bool {
    let f = e.attr(text, "filter").is_some_and(|v| v.trim() != "none");
    let s = e.attr(text, "style").is_some_and(|v| v.contains("filter:") && !v.contains("filter:none"));
    f || s
}

fn setup(text: String, w: usize, h: usize) -> Result<Problem, String> {
    let elems = scan(&text);
    let tb = text.as_bytes();
    let root = elems.first().filter(|e| e.name == "svg").ok_or("that isn't an SVG")?;
    let (w_f, h_f) = (w as f64, h as f64);
    let vb: Vec<f64> = root.attr(&text, "viewBox").map(|v| v.split(|c: char| c.is_whitespace() || c == ',').filter_map(|s| s.parse().ok()).collect()).unwrap_or_default();
    let vb = if vb.len() == 4 && vb[2] > 0.0 && vb[3] > 0.0 {
        [vb[0], vb[1], vb[2], vb[3]]
    } else {
        let dim = |n: &str, d: f64| root.attr(&text, n).and_then(|v| v.trim().trim_end_matches("px").parse().ok()).unwrap_or(d);
        [0.0, 0.0, dim("width", w_f), dim("height", h_f)]
    };
    // Drawn into w x h as a browser does: scaled to fit, centred.
    let k = (w_f / vb[2]).min(h_f / vb[3]);
    let root_abs: Aff = [k, 0.0, 0.0, k, (w_f - vb[2] * k) / 2.0 - vb[0] * k, (h_f - vb[3] * k) / 2.0 - vb[1] * k];
    let mut sizing: Edits = Vec::new();
    for (n, v) in [("width", w), ("height", h)] {
        match root.attr_at(n) {
            Some(a) => sizing.push((a.vs, a.ve, v.to_string(), false)),
            None => sizing.push((root.name_end, root.name_end, format!(" {n}=\"{v}\""), false)),
        }
    }

    let ids: std::collections::HashMap<String, usize> = elems.iter().enumerate().filter_map(|(i, e)| e.attr(&text, "id").map(|v| (v.to_string(), i))).collect();
    let mut first_use: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (i, e) in elems.iter().enumerate() {
        if e.name == "use" {
            if let Some(t) = href(e, &text).and_then(|id| ids.get(&id).copied()) {
                first_use.entry(t).or_insert(i);
            }
        }
    }
    // A presentation attribute as it reaches an element: its own, else its
    // parent's; an element in defs looks on from the first use that draws it.
    let inherited = |i: usize, name: &str| -> Option<&str> {
        let mut x = Some(i);
        let mut hops = 0;
        while let Some(j) = x {
            let e = &elems[j];
            if let Some(v) = e.attr(&text, name) {
                return Some(v.trim());
            }
            if let Some(st) = e.attr(&text, "style") {
                for decl in st.split(';') {
                    if let Some((k, v)) = decl.split_once(':') {
                        if k.trim() == name {
                            return Some(v.trim());
                        }
                    }
                }
            }
            hops += 1;
            if hops > 64 {
                return None;
            }
            x = first_use.get(&j).copied().or(e.parent);
        }
        None
    };
    // Each element's own transform (a use's x/y included), and where it lands:
    // elements in defs take the place of the first use that draws them.
    let own: Vec<Aff> = elems
        .iter()
        .map(|e| {
            let mut t = if DEFS.contains(&e.name.as_str()) || e.name == "svg" { ID } else { e.attr(&text, "transform").map(parse_transform).unwrap_or(ID) };
            if e.name == "use" {
                let g = |n: &str| e.attr(&text, n).and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(0.0);
                t = mul(&t, &[1.0, 0.0, 0.0, 1.0, g("x"), g("y")]);
            }
            t
        })
        .collect();
    // Masks and clip paths draw in the units of the element that uses them.
    let mut referrer: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (i, e) in elems.iter().enumerate() {
        for n in ["mask", "clip-path"] {
            let Some(v) = e.attr(&text, n) else { continue };
            let id = v.trim().strip_prefix("url(").and_then(|x| x.strip_suffix(')')).map(|x| x.trim().trim_matches(|c| c == '"' || c == '\'').trim_start_matches('#'));
            if let Some(&t) = id.and_then(|id| ids.get(id)) {
                referrer.entry(t).or_insert(i);
            }
        }
    }
    let mut abs: Vec<Aff> = vec![root_abs; elems.len()];
    for pass in 0..2 {
        for (i, e) in elems.iter().enumerate().skip(1) {
            if (pass == 0) == e.in_defs {
                continue;
            }
            let p = e.parent.unwrap_or(0);
            let base = if let (true, Some(&r)) = (e.in_defs, referrer.get(&p)) {
                abs[r]
            } else if e.in_defs && (DEFS.contains(&elems[p].name.as_str()) || first_use.contains_key(&i)) {
                first_use.get(&i).map(|&u| abs[u]).unwrap_or(root_abs)
            } else {
                abs[p]
            };
            abs[i] = mul(&base, &own[i]);
        }
    }

    // Filters: a filtered element only moves whole; inside one, nothing moves
    // on its own.
    let filtered: Vec<bool> = elems.iter().map(|e| has_filter(e, &text)).collect();
    let mut frozen = vec![false; elems.len()];
    for (i, e) in elems.iter().enumerate() {
        if let Some(p) = e.parent {
            frozen[i] = frozen[p] || filtered[p] || elems[p].in_filter;
        }
        frozen[i] |= e.in_filter;
    }

    // Shapes.
    let mut shapes: Vec<Shape> = Vec::new();
    let mut shape_of: Vec<Option<usize>> = vec![None; elems.len()];
    for (i, e) in elems.iter().enumerate() {
        if e.in_filter {
            continue;
        }
        let sh = if e.name == "path" {
            e.attr_at("d").and_then(|a| path_shape(tb, a.vs, a.ve))
        } else {
            basic_shape(e, &text)
        };
        if let Some(mut sh) = sh {
            sh.elem = i;
            sh.ppu = scale_of(&abs[i]);
            sh.dec = decimals(sh.ppu);
            shape_of[i] = Some(shapes.len());
            shapes.push(sh);
        }
    }

    // Boxes in each element's own units.
    fn local_box(i: usize, elems: &[Elem], text: &str, shapes: &[Shape], shape_of: &[Option<usize>], ids: &std::collections::HashMap<String, usize>, own: &[Aff], memo: &mut Vec<Option<Option<BBox>>>, depth: usize) -> Option<BBox> {
        if let Some(b) = memo[i] {
            return b;
        }
        if depth > 32 {
            return None;
        }
        let e = &elems[i];
        let b = if let Some(k) = shape_of[i] {
            shape_bbox(&shapes[k])
        } else if e.name == "use" {
            href(e, text).and_then(|id| ids.get(&id).copied()).and_then(|t| local_box(t, elems, text, shapes, shape_of, ids, own, memo, depth + 1).map(|b| bbox_map(&own[t], &b)))
        } else if e.name == "g" || e.name == "a" || e.name == "switch" || e.name == "symbol" {
            let mut acc = None;
            for &c in &e.children {
                if DEFS.contains(&elems[c].name.as_str()) {
                    continue;
                }
                if let Some(cb) = local_box(c, elems, text, shapes, shape_of, ids, own, memo, depth + 1) {
                    acc = Some(bbox_union(acc, bbox_map(&own[c], &cb)));
                }
            }
            acc
        } else {
            None
        };
        memo[i] = Some(b);
        b
    }
    let mut memo = vec![None; elems.len()];
    let boxes: Vec<Option<BBox>> = (0..elems.len()).map(|i| local_box(i, &elems, &text, &shapes, &shape_of, &ids, &own, &mut memo, 0)).collect();

    // Uses drawing the same geometry in the same place (an outline under a
    // colour, say) must not part: they don't move on their own, and nor does
    // a group holding one of them without the others. The shared geometry
    // in defs moves instead, taking every copy with it.
    let is_within = |mut x: usize, a: usize| loop {
        if x == a {
            return true;
        }
        match elems[x].parent {
            Some(p) => x = p,
            None => return false,
        }
    };
    let uses: Vec<(usize, usize)> = elems.iter().enumerate().filter(|(_, e)| e.name == "use").filter_map(|(i, e)| href(e, &text).and_then(|id| ids.get(&id).copied()).map(|t| (i, t))).collect();
    let mut partners: Vec<Vec<usize>> = vec![Vec::new(); elems.len()];
    for (k, &(u1, t1)) in uses.iter().enumerate() {
        for &(u2, t2) in &uses[k + 1..] {
            let same = abs[u1].iter().zip(&abs[u2]).all(|(a, b)| (a - b).abs() <= 1e-6 * (1.0 + a.abs()));
            if same && (is_within(t1, t2) || is_within(t2, t1)) {
                partners[u1].push(u2);
                partners[u2].push(u1);
            }
        }
    }
    let splits_stack = |g: usize| uses.iter().any(|&(u, _)| is_within(u, g) && partners[u].iter().any(|&o| !is_within(o, g)));

    // Movers.
    let mut movers: Vec<Mover> = Vec::new();
    for (i, e) in elems.iter().enumerate().skip(1) {
        if e.in_filter || !PIECES.contains(&e.name.as_str()) {
            continue;
        }
        let Some(bb) = boxes[i] else { continue };
        // Nor does a piece of defs whose insides are also drawn on their own:
        // moving it would move only some copies.
        if splits_stack(i) || (e.in_defs && uses.iter().any(|&(_, t)| t != i && is_within(t, i))) {
            continue;
        }
        let parent_abs = mul(&abs[i], &{
            // inverse of own
            let m = &own[i];
            let det = m[0] * m[3] - m[1] * m[2];
            if det.abs() < 1e-12 {
                ID
            } else {
                let (a, b, c, d) = (m[3] / det, -m[1] / det, -m[2] / det, m[0] / det);
                [a, b, c, d, -(a * m[4] + c * m[5]), -(b * m[4] + d * m[5])]
            }
        });
        let ppu = scale_of(&parent_abs);
        let c = apply(&own[i], centre(&bb));
        let half = ppu * scale_of(&own[i]) * half_diag(&bb);
        if half < 1.0 {
            continue;
        }
        let sk = shape_of[i];
        let bake = sk.is_some_and(|k| shapes[k].complete) && e.attr_at("transform").is_none();
        let form = sk.map(|k| shapes[k].form);
        let prefix = if bake {
            None
        } else {
            Some(match e.attr_at("transform") {
                Some(a) => (a.vs, false),
                None => (e.name_end, true),
            })
        };
        let round = matches!(form, Some(Form::Circle));
        let rot = !round && (!bake || matches!(form, Some(Form::Path | Form::Line | Form::Poly)) && !sk.is_some_and(|k| shapes[k].hv));
        let asp = !round;
        let mi = movers.len();
        if bake {
            shapes[sk.unwrap()].mover = Some(mi);
        }
        // Glows (filtered, or inside a filtered group) are paired up below.
        movers.push(Mover { elem: i, prefix, shape: if bake { sk } else { None }, c, ppu, half, rot, asp, tight: filtered[i] || frozen[i], follows: None, dec: decimals(ppu), at: apply(&parent_abs, c) });
    }

    // Shapes in one group that share two or more points (two halves of a
    // disc, pieces meeting along an edge) move as one, so no seam opens.
    {
        let mut by_pt: std::collections::HashMap<(usize, i64, i64), Vec<usize>> = std::collections::HashMap::new();
        for (m, mo) in movers.iter().enumerate() {
            let Some(k) = mo.shape else { continue };
            if mo.tight || !matches!(shapes[k].form, Form::Path | Form::Poly | Form::Line) {
                continue;
            }
            let par = elems[mo.elem].parent.unwrap_or(0);
            let mut seen = std::collections::HashSet::new();
            for p in &shapes[k].pts {
                let key = (par, (p[0] * 100.0).round() as i64, (p[1] * 100.0).round() as i64);
                if seen.insert(key) {
                    by_pt.entry(key).or_default().push(m);
                }
            }
        }
        let mut shared: std::collections::HashMap<(usize, usize), usize> = std::collections::HashMap::new();
        for ms in by_pt.values() {
            for (a, &x) in ms.iter().enumerate() {
                for &y in &ms[a + 1..] {
                    *shared.entry((x.min(y), x.max(y))).or_default() += 1;
                }
            }
        }
        let mut lead: Vec<usize> = (0..movers.len()).collect();
        fn top(l: &mut [usize], mut i: usize) -> usize {
            while l[i] != i {
                l[i] = l[l[i]];
                i = l[i];
            }
            i
        }
        let mut pairs: Vec<(usize, usize)> = shared.into_iter().filter(|(_, n)| *n >= 2).map(|(k, _)| k).collect();
        pairs.sort();
        for (x, y) in pairs {
            let (a, b) = (top(&mut lead, x), top(&mut lead, y));
            if a != b {
                lead[a.max(b)] = a.min(b);
            }
        }
        for m in 0..movers.len() {
            let l = top(&mut lead, m);
            if l != m {
                let (c, rot, asp) = (movers[l].c, movers[m].rot, movers[m].asp);
                movers[l].rot &= rot;
                movers[l].asp &= asp;
                movers[m].c = c;
                movers[m].follows = Some((Lead::Twin(l), 1.0));
            }
        }
    }

    // Outlines of paths with several, and their points, for shapes outside filters.
    let mut subs: Vec<Sub> = Vec::new();
    let mut classes: Vec<Class> = Vec::new();
    let long = w.max(h) as f64;
    for sh in shapes.iter_mut() {
        if frozen[sh.elem] || filtered[sh.elem] {
            continue;
        }
        // Line art: a stroke drawn along the shape with no fill. Moving one of
        // its points or outlines bends the whole stroke (a letter's bowl
        // closes up, a hump grows a hook), so its outlines move half as far
        // and its points not at all.
        let line = inherited(sh.elem, "fill") == Some("none") && inherited(sh.elem, "stroke").is_some_and(|v| v != "none");
        let sub_boxes: Vec<Option<BBox>> = (0..sh.nsub).map(|s| bbox_of(sh.pts.iter().zip(&sh.pt_sub).filter(|(_, &q)| q == s).map(|(p, _)| *p))).collect();
        if sh.nsub >= 2 && sh.form == Form::Path {
            sh.sub0 = Some(subs.len());
            for bb in sub_boxes.iter() {
                let bb = bb.unwrap_or([0.0; 4]);
                subs.push(Sub { c: centre(&bb), ppu: sh.ppu, half: sh.ppu * half_diag(&bb), at: apply(&abs[sh.elem], centre(&bb)), line });
            }
        }
        // Points. Ends of segments are nodes; handles follow their node.
        if line || matches!(sh.form, Form::Circle | Form::Ellipse | Form::Rect) {
            continue;
        }
        let n = sh.pts.len();
        // Union-find per axis.
        let mut par: [Vec<usize>; 2] = [(0..n).collect(), (0..n).collect()];
        fn root(p: &mut [usize], mut i: usize) -> usize {
            while p[i] != i {
                p[i] = p[p[i]];
                i = p[i];
            }
            i
        }
        let join = |par: &mut [Vec<usize>; 2], axis: usize, a: usize, b: usize| {
            let (ra, rb) = (root(&mut par[axis], a), root(&mut par[axis], b));
            if ra != rb {
                par[axis][ra] = rb;
            }
        };
        // A straight edge that is exactly level or upright stays so, however
        // it is written (L, a closing Z, a polygon's side), as H and V do.
        let pts = sh.pts.clone();
        let level = |par: &mut [Vec<usize>; 2], a: usize, b: usize| {
            for axis in 0..2 {
                if (pts[a][axis] - pts[b][axis]).abs() < 1e-6 {
                    let (ra, rb) = (root(&mut par[axis], a), root(&mut par[axis], b));
                    if ra != rb {
                        par[axis][ra] = rb;
                    }
                }
            }
        };
        if matches!(sh.form, Form::Poly | Form::Line) {
            for i in 0..n {
                let j = (i + 1) % n;
                if j != i && (j != 0 || elems[sh.elem].name == "polygon") {
                    level(&mut par, i, j);
                }
            }
        }
        let mut handles: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
        let mut is_node = vec![sh.form != Form::Path; n];
        if sh.form == Form::Path {
            let (mut prev, mut start): (Option<usize>, Option<usize>) = (None, None);
            for s in &sh.segs {
                let end = s.pts.last().copied();
                match s.cmd {
                    b'M' => start = end,
                    b'L' => {
                        if let Some(p) = prev {
                            level(&mut par, p, s.pts[0]);
                        }
                    }
                    b'H' => join(&mut par, 1, prev.unwrap_or(s.pts[0]), s.pts[0]),
                    b'V' => join(&mut par, 0, prev.unwrap_or(s.pts[0]), s.pts[0]),
                    b'C' => {
                        if let Some(p) = prev {
                            handles[p].push((s.pts[0], 1.0));
                        }
                        handles[s.pts[2]].push((s.pts[1], 1.0));
                    }
                    b'S' => handles[s.pts[1]].push((s.pts[0], 1.0)),
                    b'Q' => {
                        if let Some(p) = prev {
                            handles[p].push((s.pts[0], 0.5));
                        }
                        handles[s.pts[1]].push((s.pts[0], 0.5));
                    }
                    b'Z' => {
                        // A closing segment that ends on the start: one node.
                        if let (Some(p), Some(st)) = (prev, start) {
                            level(&mut par, p, st);
                            if p != st && (sh.pts[p][0] - sh.pts[st][0]).abs() < 1e-6 && (sh.pts[p][1] - sh.pts[st][1]).abs() < 1e-6 {
                                join(&mut par, 0, p, st);
                                join(&mut par, 1, p, st);
                            }
                        }
                    }
                    _ => {}
                }
                if let Some(e) = end {
                    is_node[e] = true;
                    prev = Some(e);
                } else {
                    prev = start;
                }
            }
        }
        for axis in 0..2 {
            let mut groups: std::collections::BTreeMap<usize, Vec<usize>> = std::collections::BTreeMap::new();
            for p in 0..n {
                if is_node[p] {
                    let r = root(&mut par[axis], p);
                    groups.entry(r).or_default().push(p);
                }
            }
            for (_, nodes) in groups {
                let mut members: Vec<(usize, f64)> = Vec::new();
                for &p in &nodes {
                    members.push((p, 1.0));
                    members.extend(handles[p].iter().copied());
                }
                // Merge repeated points (a handle shared by two nodes in the group).
                members.sort_by_key(|m| m.0);
                let mut merged: Vec<(usize, f64)> = Vec::new();
                for m in members {
                    match merged.last_mut() {
                        Some(l) if l.0 == m.0 => l.1 = (l.1 + m.1).min(1.0),
                        _ => merged.push(m),
                    }
                }
                let size = nodes.iter().filter_map(|&p| sub_boxes[sh.pt_sub[p]].map(|b| half_diag(&b) * 2.0)).fold(f64::INFINITY, f64::min);
                let size = if size.is_finite() { size } else { 0.0 };
                let cap = (0.01 * long / sh.ppu).min(0.03 * size);
                if cap * sh.ppu < 0.2 {
                    continue;
                }
                sh.classes.push(classes.len());
                classes.push(Class { axis, members: merged, cap, ppu: sh.ppu });
            }
        }
    }

    // Pair each glow (filtered, or inside a filtered group) with the shape it
    // sits on, so it moves with that shape. Movers are in document order, so
    // an ancestor is settled before what is inside it: inside a glow that
    // follows something, nothing follows anything else.
    let leaders: Vec<(Lead, [f64; 2], f64, f64)> = movers
        .iter()
        .enumerate()
        .filter(|(_, m)| !m.tight && m.follows.is_none())
        .map(|(i, m)| (Lead::Mover(i), m.at, m.half, m.ppu))
        .chain(subs.iter().enumerate().map(|(i, s)| (Lead::Sub(i), s.at, s.half, s.ppu)))
        .collect();
    let mover_of: std::collections::HashMap<usize, usize> = movers.iter().enumerate().map(|(i, m)| (m.elem, i)).collect();
    const STAY: Option<(Lead, f64)> = Some((Lead::Mover(usize::MAX), 0.0));
    for i in 0..movers.len() {
        if !movers[i].tight {
            continue;
        }
        let mut a = elems[movers[i].elem].parent;
        let mut inside_follower = false;
        while let Some(x) = a {
            if let Some(&m) = mover_of.get(&x) {
                if movers[m].tight && movers[m].follows.is_some_and(|f| f.1 > 0.0) {
                    inside_follower = true;
                }
            }
            a = elems[x].parent;
        }
        // A blurred group of several glows looks for a leader for each glow, not one for all.
        let many = elems[movers[i].elem].name == "g" && movers.iter().filter(|m| m.elem != movers[i].elem && is_within(m.elem, movers[i].elem)).count() >= 2;
        let mo = &movers[i];
        let best = if many { None } else { leaders
            .iter()
            .filter(|l| {
                let d = ((l.1[0] - mo.at[0]).powi(2) + (l.1[1] - mo.at[1]).powi(2)).sqrt();
                d <= (0.35 * l.2).max(1.5) && (0.4..=3.0).contains(&(mo.half / l.2))
            })
            .min_by(|a, b| {
                let d = |l: &(Lead, [f64; 2], f64, f64)| (l.1[0] - mo.at[0]).powi(2) + (l.1[1] - mo.at[1]).powi(2);
                d(a).partial_cmp(&d(b)).unwrap()
            }) };
        movers[i].follows = match best {
            _ if inside_follower => STAY,
            Some(l) => Some((l.0, l.3 / mo.ppu)),
            // A frozen piece with nothing to follow stays put; a filtered one moves a little.
            None if frozen[mo.elem] => STAY,
            None => None,
        };
    }
    // A filtered group whose glows follow shapes one by one stays put itself.
    for i in 0..movers.len() {
        if movers[i].tight && movers[i].follows.is_none() {
            let e = movers[i].elem;
            let has = movers.iter().any(|m| {
                let mut a = elems[m.elem].parent;
                while let Some(x) = a {
                    if x == e {
                        return m.follows.is_some_and(|f| f.1 > 0.0);
                    }
                    a = elems[x].parent;
                }
                false
            });
            if has {
                movers[i].follows = STAY;
            }
        }
    }
    let mut followers_m = vec![Vec::new(); movers.len()];
    let mut followers_s = vec![Vec::new(); subs.len()];
    for (i, mo) in movers.iter().enumerate() {
        match mo.follows {
            Some((Lead::Mover(l) | Lead::Twin(l), _)) if l != usize::MAX => followers_m[l].push(i),
            Some((Lead::Sub(l), _)) => followers_s[l].push(i),
            _ => {}
        }
    }

    // Paint numbers. Not inside masks and clip paths, where colour is opacity.
    let in_mask: Vec<bool> = (0..elems.len()).map(|i| (0..elems.len()).any(|m| (elems[m].name == "mask" || elems[m].name == "clipPath") && is_within(i, m))).collect();
    let mut slots = Vec::new();
    for (ei, e) in elems.iter().enumerate().skip(1) {
        if e.in_filter || in_mask[ei] {
            continue;
        }
        let grad = e.name == "linearGradient" || e.name == "radialGradient";
        let obb = grad && e.attr(&text, "gradientUnits") != Some("userSpaceOnUse");
        let gppu = scale_of(&root_abs);
        for a in &e.attrs {
            let n = a.name.as_str();
            let mut push = |s: usize, en: usize, v: [f64; 3], kind: Kind, ppu: f64| slots.push(Slot { s, e: en, kind, orig: v, ppu });
            match n {
                "fill" | "stroke" | "stop-color" | "color" | "style" => {
                    for (s, en, c) in hex_colours(&text, a.vs, a.ve) {
                        push(s, en, c, Kind::Colour, 1.0);
                    }
                }
                "opacity" | "fill-opacity" | "stroke-opacity" | "stop-opacity" | "offset" => {
                    let Some((s, en, v)) = num_at(tb, a.vs).map(|(en, v)| (a.vs, en, v)) else { continue };
                    let pct = en < a.ve && tb[en] == b'%';
                    let kind = if pct { Kind::Unit { lo: 0.0, hi: 100.0, step: 2.0, cap: 12.0 } } else { Kind::Unit { lo: 0.0, hi: 1.0, step: 0.02, cap: 0.12 } };
                    push(s, en, [v, 0.0, 0.0], kind, 1.0);
                }
                "stroke-width" if !frozen[ei] => {
                    let Some((s, en, v)) = plain_num(tb, a.vs, a.ve) else { continue };
                    push(s, en, [v, 0.0, 0.0], Kind::Width, scale_of(&abs[ei]));
                }
                "x1" | "y1" | "x2" | "y2" | "cx" | "cy" | "r" | "fx" | "fy" if grad => {
                    let Some((en, v)) = num_at(tb, a.vs) else { continue };
                    let pct = en < a.ve && tb[en] == b'%';
                    let kind = if obb || pct {
                        let step = if pct { 2.0 } else { 0.02 };
                        Kind::Unit { lo: -1e9, hi: 1e9, step, cap: 12.0 * step }
                    } else {
                        Kind::Coord
                    };
                    push(a.vs, en, [v, 0.0, 0.0], kind, gppu);
                }
                "gradientTransform" => {
                    for (s, en, v, kind) in transform_nums(tb, a.vs, a.ve) {
                        push(s, en, [v, 0.0, 0.0], kind, gppu);
                    }
                }
                _ => {}
            }
        }
    }
    slots.sort_by_key(|s| s.s);

    Ok(Problem { text, sizing, shapes, movers, subs, classes, slots, followers_m, followers_s, w, h, long })
}

// ---------------------------------------------------------------- scoring

fn linear_table() -> [f32; 256] {
    let mut t = [0f32; 256];
    for (i, v) in t.iter_mut().enumerate() {
        let c = i as f32 / 255.0;
        *v = if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
    }
    t
}

fn lab(lin: &[f32; 256], r: u8, g: u8, b: u8) -> [f32; 3] {
    let (r, g, b) = (lin[r as usize], lin[g as usize], lin[b as usize]);
    let x = (0.4124 * r + 0.3576 * g + 0.1805 * b) / 0.9505;
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let z = (0.0193 * r + 0.1192 * g + 0.9505 * b) / 1.089;
    let f = |t: f32| if t > 0.008856 { t.cbrt() } else { 7.787 * t + 16.0 / 116.0 };
    let (fx, fy, fz) = (f(x), f(y), f(z));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// The original at the raster size: Lab over black and over white, and coverage.
struct Target {
    lab_b: Vec<[f32; 3]>,
    lab_w: Vec<[f32; 3]>,
    alpha: Vec<bool>,
    lin: [f32; 256],
}

impl Target {
    /// From straight RGBA at any size, area-averaged down to w x h.
    fn new(img: &[u8], iw: usize, ih: usize, w: usize, h: usize) -> Target {
        let (fx, fy) = (iw as f64 / w as f64, ih as f64 / h as f64);
        let lin = linear_table();
        let mut t = Target { lab_b: Vec::with_capacity(w * h), lab_w: Vec::with_capacity(w * h), alpha: Vec::with_capacity(w * h), lin };
        for y in 0..h {
            let (y0, y1) = (y as f64 * fy, (y + 1) as f64 * fy);
            for x in 0..w {
                let (x0, x1) = (x as f64 * fx, (x + 1) as f64 * fx);
                let mut acc = [0f64; 4];
                let mut wsum = 0f64;
                let mut sy = y0.floor() as usize;
                while (sy as f64) < y1 && sy < ih {
                    let wy = (y1.min(sy as f64 + 1.0) - y0.max(sy as f64)).max(0.0);
                    let mut sx = x0.floor() as usize;
                    while (sx as f64) < x1 && sx < iw {
                        let wx = (x1.min(sx as f64 + 1.0) - x0.max(sx as f64)).max(0.0);
                        let i = (sy * iw + sx) * 4;
                        let a = img[i + 3] as f64 / 255.0;
                        let wgt = wx * wy;
                        for k in 0..3 {
                            acc[k] += img[i + k] as f64 * a * wgt;
                        }
                        acc[3] += img[i + 3] as f64 * wgt;
                        wsum += wgt;
                        sx += 1;
                    }
                    sy += 1;
                }
                let p: Vec<f64> = acc.iter().map(|v| v / wsum.max(1e-9)).collect();
                let q = |v: f64| v.round().clamp(0.0, 255.0) as u8;
                t.lab_b.push(lab(&lin, q(p[0]), q(p[1]), q(p[2])));
                t.lab_w.push(lab(&lin, q(p[0] + 255.0 - p[3]), q(p[1] + 255.0 - p[3]), q(p[2] + 255.0 - p[3])));
                t.alpha.push(p[3] > 0.5);
            }
        }
        t
    }

    /// One rendered pixel's ΔE (straight RGBA), or None when neither image covers it.
    fn contrib(&self, i: usize, p: [u8; 4]) -> Option<f32> {
        let a = p[3] as u32;
        if a == 0 && !self.alpha[i] {
            return None;
        }
        let d = |x: &[f32; 3], y: &[f32; 3]| ((x[0] - y[0]).powi(2) + (x[1] - y[1]).powi(2) + (x[2] - y[2]).powi(2)).sqrt();
        if a == 255 {
            let l = lab(&self.lin, p[0], p[1], p[2]);
            return Some(d(&l, &self.lab_b[i]).max(d(&l, &self.lab_w[i])));
        }
        let ob = |v: u8| ((v as u32 * a + 127) / 255) as u8;
        let ow = |v: u8| ((v as u32 * a + 255 * (255 - a) + 127) / 255) as u8;
        let lb = lab(&self.lin, ob(p[0]), ob(p[1]), ob(p[2]));
        let lw = lab(&self.lin, ow(p[0]), ow(p[1]), ow(p[2]));
        Some(d(&lb, &self.lab_b[i]).max(d(&lw, &self.lab_w[i])))
    }
}

/// The current best's render, with each pixel's ΔE (-1 where nothing is drawn).
struct Base {
    px: Vec<u32>,
    contrib: Vec<f32>,
    sum: f64,
    n: i64,
}

fn pack(c: &[u8]) -> u32 {
    u32::from_le_bytes([c[0], c[1], c[2], c[3]])
}

impl Base {
    fn new(t: &Target, img: &[u8]) -> Base {
        let px: Vec<u32> = img.chunks_exact(4).map(pack).collect();
        let contrib: Vec<f32> = img.chunks_exact(4).enumerate().map(|(i, c)| t.contrib(i, [c[0], c[1], c[2], c[3]]).unwrap_or(-1.0)).collect();
        let sum = contrib.iter().filter(|c| **c >= 0.0).map(|&c| c as f64).sum();
        let n = contrib.iter().filter(|c| **c >= 0.0).count() as i64;
        Base { px, contrib, sum, n }
    }

    fn mean(sum: f64, n: i64) -> f64 {
        if n <= 0 {
            0.0
        } else {
            sum / n as f64
        }
    }

    /// The mean ΔE of `img`, scoring only the pixels that differ from this.
    fn score(&self, t: &Target, img: &[u8]) -> f64 {
        let (mut sum, mut n) = (self.sum, self.n);
        for (i, c) in img.chunks_exact(4).enumerate() {
            if pack(c) == self.px[i] {
                continue;
            }
            let old = self.contrib[i];
            if old >= 0.0 {
                sum -= old as f64;
                n -= 1;
            }
            if let Some(v) = t.contrib(i, [c[0], c[1], c[2], c[3]]) {
                sum += v as f64;
                n += 1;
            }
        }
        Base::mean(sum, n)
    }

    fn adopt(&mut self, t: &Target, img: &[u8]) {
        for (i, c) in img.chunks_exact(4).enumerate() {
            let p = pack(c);
            if p == self.px[i] {
                continue;
            }
            let old = self.contrib[i];
            if old >= 0.0 {
                self.sum -= old as f64;
                self.n -= 1;
            }
            let v = t.contrib(i, [c[0], c[1], c[2], c[3]]).unwrap_or(-1.0);
            if v >= 0.0 {
                self.sum += v as f64;
                self.n += 1;
            }
            self.px[i] = p;
            self.contrib[i] = v;
        }
    }
}

// ---------------------------------------------------------------- search

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tier {
    Moves,
    Subs,
    Paint,
    Points,
    /// Start a second, finer cycle: every var gets another chance.
    Again,
    Revert,
}

#[derive(Clone, Copy)]
enum Item {
    Mv(usize),
    Sub(usize),
    Node(usize),
    Num(usize),
}

#[derive(Clone, Copy)]
enum Var {
    Mv(usize, usize),
    Sub(usize, usize),
    Node(usize),
    Num(usize, usize),
    Undo(Item),
}

/// Where a phase is: the var, the direction being tried, and how many steps
/// in that direction have already paid off.
#[derive(Clone, Copy)]
struct Cursor {
    i: usize,
    dir: f64,
    rep: u32,
}

struct Gen {
    tier: Tier,
    level: f64,
    vars: Vec<(Var, usize)>,
    paid: Vec<bool>,
    cur: Cursor,
    eps: f64,
    limit: f64,
}

impl Gen {
    fn at(&self, c: Cursor) -> Option<(Var, f64)> {
        self.vars.get(c.i).map(|v| (v.0, c.dir))
    }
    fn after_fail(&self, c: Cursor) -> Cursor {
        if self.tier == Tier::Revert || c.rep > 0 || c.dir < 0.0 {
            Cursor { i: c.i + 1, dir: 1.0, rep: 0 }
        } else {
            Cursor { dir: -1.0, ..c }
        }
    }
    fn after_success(&self, c: Cursor) -> Cursor {
        if self.tier == Tier::Revert || c.rep >= 6 {
            Cursor { i: c.i + 1, dir: 1.0, rep: 0 }
        } else {
            Cursor { rep: c.rep + 1, ..c }
        }
    }
}

struct Pending {
    st: Option<State>,
    before: Cursor,
}

/// A polish in progress. See the module notes.
pub struct Polish {
    p: Problem,
    t: Target,
    st: State,
    cur: f64,
    before: f64,
    base: Option<Base>,
    plan: Vec<(Tier, f64)>,
    phase: usize,
    gen: Option<Gen>,
    /// Every var with its tier, and how many phases in a row it failed in.
    vars: Vec<(Tier, Var)>,
    fails: Vec<u8>,
    pending: Vec<Pending>,
    renders: usize,
    max_renders: usize,
    done: bool,
    pub log: Vec<String>,
}

/// Tier thresholds: how much a change must lower the mean ΔE to be kept.
fn eps_of(t: Tier) -> f64 {
    match t {
        Tier::Moves => 0.001,
        Tier::Subs | Tier::Paint => 0.002,
        Tier::Points => 0.004,
        Tier::Revert | Tier::Again => 0.0,
    }
}

impl Polish {
    /// Set up a polish of `svg` against the original, straight RGBA `orig`
    /// (`ow` x `oh`), compared at `long` px on the long side. `max_renders`
    /// bounds the work; the caller can also stop at any time.
    pub fn new(svg: &str, orig: &[u8], ow: usize, oh: usize, long: usize, max_renders: usize) -> Result<Polish, String> {
        if orig.len() != ow * oh * 4 || ow == 0 || oh == 0 {
            return Err("the original's pixels don't match its size".into());
        }
        let s = (long as f64 / ow.max(oh) as f64).min(1.0);
        let (w, h) = (((ow as f64 * s).round() as usize).max(1), ((oh as f64 * s).round() as usize).max(1));
        let p = setup(svg.to_string(), w, h)?;
        let t = Target::new(orig, ow, oh, w, h);
        let st = p.state0();
        let mut vars: Vec<(Tier, Var)> = Vec::new();
        for (m, mo) in p.movers.iter().enumerate() {
            if mo.follows.is_some() {
                continue;
            }
            for k in 0..5 {
                if (k == 3 && (!mo.rot || mo.tight)) || (k == 4 && (!mo.asp || mo.tight)) {
                    continue;
                }
                vars.push((Tier::Moves, Var::Mv(m, k)));
            }
        }
        for (s, sb) in p.subs.iter().enumerate() {
            if sb.half >= 1.5 {
                for k in 0..3 {
                    vars.push((Tier::Subs, Var::Sub(s, k)));
                }
            }
        }
        for (i, sl) in p.slots.iter().enumerate() {
            for ch in 0..if sl.kind == Kind::Colour { 3 } else { 1 } {
                vars.push((Tier::Paint, Var::Num(i, ch)));
            }
        }
        for c in 0..p.classes.len() {
            vars.push((Tier::Points, Var::Node(c)));
        }
        let mut plan = Vec::new();
        for level in [4.0, 2.0, 1.0, 0.5, 0.25, -1.0, 1.0, 0.5, 0.25] {
            if level < 0.0 {
                plan.push((Tier::Again, 0.0));
                continue;
            }
            plan.push((Tier::Moves, level));
            if level <= 2.0 {
                plan.push((Tier::Subs, level));
            }
            plan.push((Tier::Paint, level));
            if level <= 1.0 {
                plan.push((Tier::Points, level));
            }
        }
        plan.push((Tier::Revert, 0.0));
        let log = vec![format!(
            "{}x{} raster, {} movers ({} follow), {} outlines, {} point groups, {} paint numbers",
            w,
            h,
            p.movers.len(),
            p.movers.iter().filter(|m| m.follows.is_some_and(|f| f.1 > 0.0)).count(),
            p.subs.len(),
            p.classes.len(),
            p.slots.len()
        )];
        let fails = vec![0; vars.len()];
        Ok(Polish { p, t, st, cur: f64::INFINITY, before: f64::INFINITY, base: None, plan, phase: 0, gen: None, vars, fails, pending: Vec::new(), renders: 0, max_renders, done: false, log })
    }

    /// The raster size renders must be drawn at.
    pub fn size(&self) -> (usize, usize) {
        (self.p.w, self.p.h)
    }
    pub fn before(&self) -> f64 {
        self.before
    }
    pub fn score(&self) -> f64 {
        self.cur
    }
    pub fn renders(&self) -> usize {
        self.renders
    }
    pub fn done(&self) -> bool {
        self.done
    }
    /// Rough share of the work done, 0 to 1.
    pub fn progress(&self) -> f64 {
        if self.done {
            return 1.0;
        }
        let by_phase = self.phase as f64 / self.plan.len() as f64;
        by_phase.max(self.renders as f64 / self.max_renders as f64).min(0.99)
    }
    /// The best SVG so far.
    pub fn svg(&self) -> String {
        self.p.build(&self.st, false)
    }

    fn step(&self, st: &State, var: Var, dir: f64, level: f64) -> Option<State> {
        let p = &self.p;
        let mut n = st.clone();
        match var {
            Var::Mv(m, k) => {
                let mo = &p.movers[m];
                let v = &mut n.mv[m];
                v[k] += dir * if k < 2 { level / mo.ppu } else { level / mo.half };
                let (tcap, scap) = if mo.tight { (0.01, 0.04) } else { (0.05, 0.12) };
                let tcap = tcap * p.long / mo.ppu;
                if v[0].abs() > tcap || v[1].abs() > tcap || v[2].abs() > scap || v[3].abs() > 0.14 || v[4].abs() > 0.06 {
                    return None;
                }
                let v = *v;
                for &f in &p.followers_m[m] {
                    n.mv[f] = match p.movers[f].follows.unwrap() {
                        (Lead::Twin(_), _) => v,
                        (_, r) => [v[0] * r, v[1] * r, v[2], 0.0, 0.0],
                    };
                }
            }
            Var::Sub(s, k) => {
                let sb = &p.subs[s];
                let v = &mut n.sub[s];
                v[k] += dir * if k < 2 { level / sb.ppu } else { level / sb.half };
                let k = if sb.line { 0.5 } else { 1.0 };
                let tcap = k * (0.03 * p.long).min(0.3 * sb.half) / sb.ppu;
                if v[0].abs() > tcap || v[1].abs() > tcap || v[2].abs() > k * 0.12 {
                    return None;
                }
                let v = *v;
                for &f in &p.followers_s[s] {
                    let r = p.movers[f].follows.unwrap().1;
                    n.mv[f] = [v[0] * r, v[1] * r, v[2], 0.0, 0.0];
                }
            }
            Var::Node(c) => {
                let cl = &p.classes[c];
                n.node[c] += dir * level / cl.ppu;
                if n.node[c].abs() > cl.cap {
                    return None;
                }
            }
            Var::Num(i, ch) => {
                let s = &p.slots[i];
                let o = s.orig[ch];
                let (d, lo, hi) = match s.kind {
                    Kind::Coord => {
                        let c = 0.04 * p.long / s.ppu;
                        (level / s.ppu, o - c, o + c)
                    }
                    Kind::Width => (level / s.ppu, o * 0.88, o * 1.12),
                    Kind::Unit { lo, hi, step, cap } => (step * level, lo.max(o - cap), hi.min(o + cap)),
                    Kind::Colour => (8.0 * level, (o - 24.0).max(0.0), (o + 24.0).min(255.0)),
                    Kind::Angle => (level, o - 10.0, o + 10.0),
                    Kind::Mat => (0.01 * level, o - 0.15, o + 0.15),
                };
                let v = n.num[i][ch] + dir * d;
                if v < lo - 1e-9 || v > hi + 1e-9 {
                    return None;
                }
                n.num[i][ch] = v;
            }
            Var::Undo(item) => match item {
                Item::Mv(m) => {
                    n.mv[m] = [0.0; 5];
                    for &f in &p.followers_m[m] {
                        n.mv[f] = [0.0; 5];
                    }
                }
                Item::Sub(s) => {
                    n.sub[s] = [0.0; 3];
                    for &f in &p.followers_s[s] {
                        n.mv[f] = [0.0; 5];
                    }
                }
                Item::Node(c) => n.node[c] = 0.0,
                Item::Num(i) => n.num[i] = p.slots[i].orig,
            },
        }
        Some(n)
    }

    fn start_phase(&mut self) -> bool {
        loop {
            let Some(&(tier, level)) = self.plan.get(self.phase) else { return false };
            if tier == Tier::Again {
                self.fails.iter_mut().for_each(|f| *f = 0);
                self.phase += 1;
                continue;
            }
            let vars: Vec<(Var, usize)> = if tier == Tier::Revert {
                let st = &self.st;
                let mut v = Vec::new();
                for (m, mo) in self.p.movers.iter().enumerate() {
                    if mo.follows.is_none() && !is_identity(&st.mv[m]) {
                        v.push((Var::Undo(Item::Mv(m)), usize::MAX));
                    }
                }
                for s in 0..self.p.subs.len() {
                    if !is_identity(&st.sub[s]) {
                        v.push((Var::Undo(Item::Sub(s)), usize::MAX));
                    }
                }
                for c in 0..self.p.classes.len() {
                    if st.node[c] != 0.0 {
                        v.push((Var::Undo(Item::Node(c)), usize::MAX));
                    }
                }
                for i in 0..self.p.slots.len() {
                    if st.num[i] != self.p.slots[i].orig {
                        v.push((Var::Undo(Item::Num(i)), usize::MAX));
                    }
                }
                v
            } else {
                // Vars that failed in three phases running are left out.
                self.vars.iter().enumerate().filter(|(i, v)| v.0 == tier && self.fails[*i] < 3).map(|(i, v)| (v.1, i)).collect()
            };
            if vars.is_empty() {
                self.phase += 1;
                continue;
            }
            let n = vars.len();
            self.gen = Some(Gen { tier, level, vars, paid: vec![false; n], cur: Cursor { i: 0, dir: 1.0, rep: 0 }, eps: eps_of(tier), limit: self.cur * 1.005 + 1e-9 });
            return true;
        }
    }

    fn end_phase(&mut self) {
        if let Some(g) = self.gen.take() {
            for (k, &(_, id)) in g.vars.iter().enumerate() {
                if id != usize::MAX {
                    self.fails[id] = if g.paid[k] { 0 } else { self.fails[id].saturating_add(1) };
                }
            }
            self.log.push(format!("{:?} at {} px: ΔE {:.3} ({} renders)", g.tier, g.level, self.cur, self.renders));
        }
        self.phase += 1;
    }

    /// Skip to the last pass, which puts back changes that bought nothing.
    pub fn wrap_up(&mut self) {
        if self.done || self.base.is_none() {
            return;
        }
        let last = self.plan.len() - 1;
        if self.phase < last {
            self.end_phase();
            self.gen = None;
            self.phase = last;
        }
    }

    /// Up to `n` SVG texts to render, at [`Polish::size`]. Empty when done.
    pub fn ask(&mut self, n: usize) -> Vec<String> {
        self.pending.clear();
        if self.done {
            return vec![];
        }
        if self.base.is_none() {
            self.pending.push(Pending { st: None, before: Cursor { i: 0, dir: 1.0, rep: 0 } });
            return vec![self.p.build(&self.st, true)];
        }
        if self.renders >= self.max_renders * 7 / 8 {
            self.wrap_up();
        }
        if self.renders >= self.max_renders {
            self.done = true;
            return vec![];
        }
        loop {
            if self.gen.is_none() && !self.start_phase() {
                self.done = true;
                self.log.push(format!("done: ΔE {:.3} -> {:.3} in {} renders", self.before, self.cur, self.renders));
                return vec![];
            }
            let g = self.gen.as_ref().unwrap();
            let (level, mut c) = (g.level, g.cur);
            let mut out = Vec::new();
            while out.len() < n {
                let g = self.gen.as_ref().unwrap();
                let Some((var, dir)) = g.at(c) else { break };
                let before = c;
                c = g.after_fail(c);
                match self.step(&self.st, var, dir, level) {
                    Some(st) => {
                        out.push(self.p.build(&st, true));
                        self.pending.push(Pending { st: Some(st), before });
                    }
                    None if out.is_empty() => self.gen.as_mut().unwrap().cur = c,
                    None => {}
                }
            }
            if !out.is_empty() {
                return out;
            }
            self.end_phase();
        }
    }

    /// The renders of the texts from the last `ask`, in order, as straight
    /// RGBA at [`Polish::size`]. Fewer than asked is fine; a buffer of the
    /// wrong size counts as a failed render.
    pub fn tell(&mut self, renders: &[&[u8]]) {
        let len = self.p.w * self.p.h * 4;
        let pending = std::mem::take(&mut self.pending);
        for (pd, img) in pending.into_iter().zip(renders) {
            self.renders += 1;
            let ok = img.len() == len;
            let Some(st) = pd.st else {
                // The drawing as it came.
                if ok {
                    let b = Base::new(&self.t, img);
                    self.cur = Base::mean(b.sum, b.n);
                    self.before = self.cur;
                    self.base = Some(b);
                    self.log.push(format!("before: ΔE {:.3}", self.cur));
                } else {
                    self.done = true;
                }
                return;
            };
            let Some(g) = self.gen.as_mut() else { return };
            let s = if ok { self.base.as_ref().unwrap().score(&self.t, img) } else { f64::INFINITY };
            let (var, _) = g.at(pd.before).unwrap();
            let win = match var {
                Var::Undo(_) => s <= self.cur + 0.0005 && s <= g.limit,
                _ => s < self.cur - g.eps,
            };
            if win {
                self.base.as_mut().unwrap().adopt(&self.t, img);
                self.cur = s;
                self.st = st;
                g.paid[pd.before.i] = true;
                g.cur = g.after_success(pd.before);
                return;
            }
            g.cur = g.after_fail(pd.before);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problem(svg: &str) -> Problem {
        setup(svg.to_string(), 100, 100).unwrap()
    }

    #[test]
    fn untouched_state_writes_the_text_back() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><path d="M10-5a5 5 0 015 5L.5.5Zm20 20h10v10h-10z" fill="#f00"/><circle cx="50" cy="50" r="10"/></svg>"##;
        let p = problem(svg);
        assert_eq!(p.build(&p.state0(), false), svg);
        assert!(p.build(&p.state0(), true).starts_with(r#"<svg width="100" height="100" xmlns"#));
    }

    #[test]
    fn a_moved_point_keeps_later_relative_points_in_place() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><path d="M10 10l20 0l2 20l-20 0z"/></svg>"#;
        let p = problem(svg);
        let mut st = p.state0();
        // The second point (30,10) moved right by 2: its own relative x grows,
        // and the next segment's shrinks so (32,30) stays.
        let c = p.classes.iter().position(|c| c.axis == 0 && c.members.iter().any(|m| p.shapes[0].pts[m.0] == [30.0, 10.0])).unwrap();
        st.node[c] = 2.0;
        assert!(p.build(&st, false).contains(r#"d="M10 10l22 0l0 20l-20 0z""#), "{}", p.build(&st, false));
    }

    #[test]
    fn an_upright_edge_stays_upright_however_it_is_written() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><path d="M10 10L30 10L30 30L10 30Z"/></svg>"#;
        let p = problem(svg);
        let mut st = p.state0();
        // (10,30) closes back to (10,10) with Z: both move together.
        let c = p.classes.iter().position(|c| c.axis == 0 && c.members.iter().any(|m| p.shapes[0].pts[m.0] == [10.0, 30.0])).unwrap();
        st.node[c] = -1.0;
        assert!(p.build(&st, false).contains(r#"d="M9 10L30 10L30 30L9 30Z""#), "{}", p.build(&st, false));
    }

    #[test]
    fn level_lines_stay_level_and_numbers_keep_separators() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><path d="M10 10H30V30H10Z"/></svg>"#;
        let p = problem(svg);
        let mut st = p.state0();
        // Moving (30,10) down moves the start too: they share a level line.
        let c = p.classes.iter().position(|c| c.axis == 1 && c.members.iter().any(|m| p.shapes[0].pts[m.0] == [30.0, 10.0])).unwrap();
        st.node[c] = -1.5;
        let out = p.build(&st, false);
        assert!(out.contains(r#"d="M10 8.5H30V30H10Z""#), "{out}");
    }

    #[test]
    fn whole_moves_are_written_into_plain_shapes_and_as_transforms_elsewhere() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><circle cx="50" cy="50" r="10"/><g><rect x="1" y="1" width="9" height="9"/></g></svg>"#;
        let p = problem(svg);
        let mut st = p.state0();
        st.mv[0] = [2.0, -1.0, (1.1f64).ln(), 0.0, 0.0];
        let g = p.movers.iter().position(|m| m.prefix.is_some()).unwrap();
        st.mv[g][0] = 3.0;
        let out = p.build(&st, false);
        assert!(out.contains(r#"<circle cx="52" cy="49" r="11"/>"#), "{out}");
        assert!(out.contains(r#"<g transform="translate(3 0)">"#), "{out}");
    }

    #[test]
    fn a_glow_follows_the_shape_it_sits_on() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><defs><filter id="b"><feGaussianBlur stdDeviation="3"/></filter></defs><path d="M0 0H100V100H0ZM40 40h20v20h-20z" fill-rule="evenodd"/><g filter="url(#b)"><circle cx="50" cy="50" r="12"/></g></svg>"#;
        let p = problem(svg);
        // The blurred group follows the square hole; the circle inside it stays put within it.
        let follow: Vec<bool> = p.movers.iter().map(|m| m.follows.is_some_and(|f| f.1 > 0.0)).collect();
        assert_eq!(follow.iter().filter(|f| **f).count(), 1);
        let g = follow.iter().position(|f| *f).unwrap();
        assert!(p.movers[g].prefix.is_some_and(|x| x.1));
        assert!(matches!(p.movers[g].follows, Some((Lead::Sub(1), _))));
    }

    #[test]
    fn polishing_moves_a_misplaced_square_back() {
        let (w, h) = (64usize, 64usize);
        let mut img = vec![255u8; w * h * 4];
        for y in 20..40 {
            for x in 24..44 {
                let i = (y * w + x) * 4;
                img[i..i + 3].copy_from_slice(&[0, 0, 0]);
            }
        }
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" fill="#fff"/><rect x="21" y="22" width="20" height="20"/></svg>"##;
        let mut pol = Polish::new(svg, &img, w, h, 64, 3000).unwrap();
        // Render by hand: the second rect, at whole pixels after rounding.
        let render = |text: &str| -> Vec<u8> {
            let num = |name: &str| -> f64 {
                let at = text.rfind("<rect").unwrap();
                let t = &text[at..];
                let k = t.find(&format!(" {name}=\"")).unwrap() + name.len() + 3;
                t[k..k + t[k..].find('"').unwrap()].parse().unwrap()
            };
            let (x, y, rw, rh) = (num("x"), num("y"), num("width"), num("height"));
            let mut out = vec![255u8; w * h * 4];
            for py in 0..h {
                for px in 0..w {
                    let cov = |a: f64, b: f64, p: usize| ((b.min(p as f64 + 1.0) - a.max(p as f64)).max(0.0)).min(1.0);
                    let c = cov(x, x + rw, px) * cov(y, y + rh, py);
                    let v = (255.0 * (1.0 - c)).round() as u8;
                    let i = (py * w + px) * 4;
                    out[i..i + 3].copy_from_slice(&[v, v, v]);
                }
            }
            out
        };
        loop {
            let asks = pol.ask(4);
            if asks.is_empty() {
                break;
            }
            let r: Vec<Vec<u8>> = asks.iter().map(|t| render(t)).collect();
            let refs: Vec<&[u8]> = r.iter().map(|v| v.as_slice()).collect();
            pol.tell(&refs);
        }
        assert!(pol.score() < 0.2 * pol.before(), "{} -> {}", pol.before(), pol.score());
        assert!(pol.svg().contains(r#"x="24" y="20""#), "{}", pol.svg());
    }
}
