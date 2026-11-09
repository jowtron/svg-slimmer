//! `--polish`: nudge the numbers of a hand-made (AI) SVG towards a bitmap.
//!
//! Prototype. The SVG's structure is left alone: every number that draws
//! something (coordinates, sizes, path data, transform values, gradient
//! offsets, opacities, hex colours) becomes a slot in the text, and only those
//! spans are rewritten. Scored as the AI redraw harness scores: CIE76 ΔE per
//! pixel, the worse of over white and over black, averaged over pixels either
//! image covers. Rendering is resvg at a modest size.
//!
//! Search is a coarse-to-fine pattern search: at each step size, first whole
//! elements (translate, scale about their centre), then single numbers. Each
//! sweep tries +step and -step for every variable in parallel against the
//! current best, then applies the winners one at a time, re-scoring each, and
//! keeps only real improvements. Numbers inside `<filter>` are held fixed
//! (blurs are slow and easy to overfit).

use rayon::prelude::*;
use resvg::{tiny_skia, usvg};
use std::time::Instant;

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

fn inv(m: &Aff) -> Aff {
    let det = m[0] * m[3] - m[1] * m[2];
    if det.abs() < 1e-12 {
        return ID;
    }
    let (a, b, c, d) = (m[3] / det, -m[1] / det, -m[2] / det, m[0] / det);
    [a, b, c, d, -(a * m[4] + c * m[5]), -(b * m[4] + d * m[5])]
}

fn apply(m: &Aff, x: f64, y: f64) -> (f64, f64) {
    (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
}

fn scale_of(m: &Aff) -> f64 {
    (m[0] * m[3] - m[1] * m[2]).abs().sqrt().max(1e-9)
}

fn from_usvg(t: usvg::Transform) -> Aff {
    [t.sx as f64, t.ky as f64, t.kx as f64, t.sy as f64, t.tx as f64, t.ty as f64]
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
    attrs: Vec<Attr>,
    /// Inside defs, gradients, masks, clip paths, symbols: not drawn where it stands.
    in_defs: bool,
    in_filter: bool,
}

impl Elem {
    fn attr<'a>(&self, text: &'a str, name: &str) -> Option<&'a str> {
        self.attrs.iter().find(|a| a.name == name).map(|a| &text[a.vs..a.ve])
    }
}

const DEFS: [&str; 9] = ["defs", "linearGradient", "radialGradient", "mask", "clipPath", "pattern", "symbol", "marker", "filter"];

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
                }
            }
            let parent = stack.last().copied();
            let pd = parent.map(|p| out[p].in_defs).unwrap_or(false);
            let pf = parent.map(|p| out[p].in_filter).unwrap_or(false);
            let in_defs = pd || DEFS.contains(&name.as_str());
            let in_filter = pf || name == "filter";
            let idx = out.len();
            let skip_body = name == "style" || name == "script";
            out.push(Elem { name, name_end, parent, attrs, in_defs, in_filter });
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

// ---------------------------------------------------------------- slots

#[derive(Clone, Copy, PartialEq)]
enum Axis {
    X,
    Y,
    Len,
    No,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// User-space length; step and decimals follow `ppu`.
    Coord,
    Angle,
    /// Linear part of scale()/matrix().
    Mat,
    /// Bounded plain number (offsets, opacities, bounding-box units).
    Unit { lo: f64, hi: f64, step: f64 },
    /// Hex colour, three channels.
    Colour,
    /// Whole-element move written as a transform prefix: [tx, ty, s].
    Prefix { cx: f64, cy: f64, insert: bool },
}

struct Slot {
    start: usize,
    end: usize,
    kind: Kind,
    axis: Axis,
    elem: usize,
    orig: [f64; 3],
    /// Optimisation-raster pixels per user unit where this number lives.
    ppu: f64,
    /// 1/2: arc rx/ry, 3/4: rx/ry attributes (equal pairs move together).
    role: u8,
}

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

/// Numbers in path data with their axis; arc flags are skipped.
fn path_nums(b: &[u8], vs: usize, ve: usize) -> Vec<(usize, usize, f64, Axis, Kind, u8)> {
    let mut out = Vec::new();
    let (mut i, mut cmd, mut k, mut first) = (vs, b'M', 0usize, true);
    while i < ve {
        let c = b[i];
        if c.is_ascii_whitespace() || c == b',' {
            i += 1;
            continue;
        }
        if c.is_ascii_alphabetic() && c != b'e' && c != b'E' {
            if !(cmd == b'M' || cmd == b'm') || k > 0 {
                first = false;
            }
            cmd = c;
            k = 0;
            i += 1;
            continue;
        }
        let up = cmd.to_ascii_uppercase();
        let rel = cmd.is_ascii_lowercase() && !(first && cmd == b'm' && k < 2);
        if up == b'A' && (k % 7 == 3 || k % 7 == 4) {
            // flag: a single 0 or 1
            k += 1;
            i += 1;
            continue;
        }
        let Some((e, v)) = num_at(b, i) else {
            i += 1;
            continue;
        };
        let (axis, kind) = match up {
            b'H' => (if rel { Axis::Len } else { Axis::X }, Kind::Coord),
            b'V' => (if rel { Axis::Len } else { Axis::Y }, Kind::Coord),
            b'A' => match k % 7 {
                0 | 1 => (Axis::Len, Kind::Coord),
                2 => (Axis::No, Kind::Angle),
                5 => (if rel { Axis::Len } else { Axis::X }, Kind::Coord),
                _ => (if rel { Axis::Len } else { Axis::Y }, Kind::Coord),
            },
            _ => (
                if rel {
                    Axis::Len
                } else if k % 2 == 0 {
                    Axis::X
                } else {
                    Axis::Y
                },
                Kind::Coord,
            ),
        };
        let role = if up == b'A' && k % 7 < 2 { 1 + (k % 7) as u8 } else { 0 };
        out.push((i, e, v, axis, kind, role));
        k += 1;
        i = e;
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
                ("translate", _) => Kind::Coord,
                ("scale", _) => Kind::Mat,
                ("rotate", 0) | ("skewX", _) | ("skewY", _) => Kind::Angle,
                ("rotate", _) => Kind::Coord,
                ("matrix", 0..=3) => Kind::Mat,
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

// ---------------------------------------------------------------- the problem

struct Problem {
    text: String,
    slots: Vec<Slot>,
    /// Elements that move as a whole: (element, bakeable, centre in parent space,
    /// px per parent unit, half-size in px).
    movers: Vec<Mover>,
    w: u32,
    h: u32,
    /// Original at the optimisation size: Lab over black, Lab over white, alpha.
    lab_b: Vec<[f32; 3]>,
    lab_w: Vec<[f32; 3]>,
    alpha: Vec<u8>,
    lin: [f32; 256],
    long_px: f64,
    /// Furthest a single number may drift, as a fraction of the raster's long side.
    num_cap: f64,
    followers: Vec<Vec<usize>>,
    follows: Vec<bool>,
}

struct Mover {
    /// Slot index of its transform prefix, or None when the move is baked into its own numbers.
    prefix: Option<usize>,
    own: Vec<usize>,
    cx: f64,
    cy: f64,
    ppu: f64,
    half: f64,
}

#[derive(Clone)]
struct State {
    vals: Vec<[f64; 3]>,
    /// Per mover: cumulative tx, ty (px), scale.
    mv: Vec<[f64; 3]>,
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

fn decimals(ppu: f64) -> u32 {
    // keep rounding under ~0.1 optimisation px
    let u = 0.1 / ppu.max(1e-9);
    if u >= 1.0 {
        0
    } else {
        ((-u.log10()).ceil() as u32).min(4)
    }
}

impl Problem {
    fn slot_text(&self, i: usize, v: &[f64; 3]) -> Option<String> {
        let s = &self.slots[i];
        if *v == s.orig {
            return None;
        }
        Some(match s.kind {
            Kind::Coord => fmt_num(v[0], decimals(s.ppu)),
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
            Kind::Prefix { cx, cy, insert } => {
                let (tx, ty, sc) = (v[0], v[1], v[2]);
                let d = decimals(s.ppu);
                let a = tx + cx * (1.0 - sc);
                let b = ty + cy * (1.0 - sc);
                let mut t = format!("translate({} {})", fmt_num(a, d), fmt_num(b, d));
                if (sc - 1.0).abs() > 1e-6 {
                    t += &format!(" scale({})", fmt_num(sc, 4));
                }
                if t == "translate(0 0)" {
                    return Some(String::new());
                }
                if insert {
                    format!(" transform=\"{t}\"")
                } else {
                    t + " "
                }
            }
        })
    }

    fn build(&self, st: &State) -> String {
        let b = self.text.as_bytes();
        let mut out = String::with_capacity(self.text.len() + 64);
        let mut prev = 0;
        for (i, s) in self.slots.iter().enumerate() {
            let Some(t) = self.slot_text(i, &st.vals[i]) else { continue };
            out.push_str(&self.text[prev..s.start]);
            let numeric = !matches!(s.kind, Kind::Colour | Kind::Prefix { .. });
            if numeric && !t.starts_with('-') && out.as_bytes().last().is_some_and(|c| c.is_ascii_digit() || *c == b'.') {
                out.push(' ');
            }
            out.push_str(&t);
            if numeric && s.end < b.len() && (b[s.end].is_ascii_digit() || b[s.end] == b'.') {
                out.push(' ');
            }
            prev = s.end;
        }
        out.push_str(&self.text[prev..]);
        out
    }

    fn render(&self, svg: &str, opt: &usvg::Options) -> Option<tiny_skia::Pixmap> {
        let tree = usvg::Tree::from_str(svg, opt).ok()?;
        let size = tree.size();
        let mut pm = tiny_skia::Pixmap::new(self.w, self.h)?;
        resvg::render(&tree, tiny_skia::Transform::from_scale(self.w as f32 / size.width(), self.h as f32 / size.height()), &mut pm.as_mut());
        Some(pm)
    }

    fn lab(&self, r: u8, g: u8, b: u8) -> [f32; 3] {
        lab(&self.lin, r, g, b)
    }

    fn score_pixmap(&self, pm: &tiny_skia::Pixmap) -> f64 {
        let mut sum = 0f64;
        let mut n = 0usize;
        for (i, p) in pm.pixels().iter().enumerate() {
            let a = p.alpha();
            if a == 0 && self.alpha[i] == 0 {
                continue;
            }
            let (r, g, b) = (p.red(), p.green(), p.blue());
            let lb = self.lab(r, g, b);
            let lw = self.lab(r.saturating_add(255 - a), g.saturating_add(255 - a), b.saturating_add(255 - a));
            let d = |x: &[f32; 3], y: &[f32; 3]| ((x[0] - y[0]).powi(2) + (x[1] - y[1]).powi(2) + (x[2] - y[2]).powi(2)).sqrt();
            sum += d(&lb, &self.lab_b[i]).max(d(&lw, &self.lab_w[i])) as f64;
            n += 1;
        }
        if n == 0 {
            0.0
        } else {
            sum / n as f64
        }
    }

    fn score(&self, st: &State, opt: &usvg::Options) -> f64 {
        match self.render(&self.build(st), opt) {
            Some(pm) => self.score_pixmap(&pm),
            None => f64::INFINITY,
        }
    }

    /// One pixel's ΔE, or None when neither image covers it.
    fn px_contrib(&self, i: usize, p: [u8; 4]) -> Option<f32> {
        let [r, g, b, a] = p;
        if a == 0 && self.alpha[i] == 0 {
            return None;
        }
        let lb = self.lab(r, g, b);
        let lw = self.lab(r.saturating_add(255 - a), g.saturating_add(255 - a), b.saturating_add(255 - a));
        let d = |x: &[f32; 3], y: &[f32; 3]| ((x[0] - y[0]).powi(2) + (x[1] - y[1]).powi(2) + (x[2] - y[2]).powi(2)).sqrt();
        Some(d(&lb, &self.lab_b[i]).max(d(&lw, &self.lab_w[i])))
    }

    /// Render of a state with its per-pixel errors, so trials only re-score changed pixels.
    fn base(&self, st: &State, opt: &usvg::Options) -> Base {
        let pm = self.render(&self.build(st), opt).expect("the current state renders");
        let px: Vec<[u8; 4]> = pm.data().chunks_exact(4).map(|c| [c[0], c[1], c[2], c[3]]).collect();
        let contrib: Vec<f32> = px.iter().enumerate().map(|(i, &p)| self.px_contrib(i, p).unwrap_or(-1.0)).collect();
        let sum = contrib.iter().filter(|c| **c >= 0.0).map(|&c| c as f64).sum();
        let n = contrib.iter().filter(|c| **c >= 0.0).count();
        Base { px, contrib, sum, n }
    }

    /// Score a state against `b`, re-scoring only pixels that differ. Returns the render too.
    fn score_vs(&self, b: &Base, st: &State, opt: &usvg::Options) -> (f64, Option<tiny_skia::Pixmap>) {
        let Some(pm) = self.render(&self.build(st), opt) else { return (f64::INFINITY, None) };
        let (mut sum, mut n) = (b.sum, b.n as i64);
        for (i, c) in pm.data().chunks_exact(4).enumerate() {
            let p = [c[0], c[1], c[2], c[3]];
            if p == b.px[i] {
                continue;
            }
            let old = b.contrib[i];
            if old >= 0.0 {
                sum -= old as f64;
                n -= 1;
            }
            let new = self.px_contrib(i, p).unwrap_or(-1.0);
            if new >= 0.0 {
                sum += new as f64;
                n += 1;
            }
        }
        (if n <= 0 { 0.0 } else { sum / n as f64 }, Some(pm))
    }

    /// Make `b` the base for render `pm`.
    fn adopt(&self, b: &mut Base, pm: &tiny_skia::Pixmap) {
        for (i, c) in pm.data().chunks_exact(4).enumerate() {
            let p = [c[0], c[1], c[2], c[3]];
            if p == b.px[i] {
                continue;
            }
            let old = b.contrib[i];
            if old >= 0.0 {
                b.sum -= old as f64;
                b.n -= 1;
            }
            let new = self.px_contrib(i, p).unwrap_or(-1.0);
            if new >= 0.0 {
                b.sum += new as f64;
                b.n += 1;
            }
            b.px[i] = p;
            b.contrib[i] = new;
        }
    }
}

#[derive(Clone)]
struct Base {
    px: Vec<[u8; 4]>,
    contrib: Vec<f32>,
    sum: f64,
    n: usize,
}

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

// ---------------------------------------------------------------- setup

/// The original, premultiplied, area-averaged down to w x h.
fn shrink(img: &image::RgbaImage, w: u32, h: u32) -> Vec<[f32; 4]> {
    let (sw, sh) = (img.width() as f64, img.height() as f64);
    let (fx, fy) = (sw / w as f64, sh / h as f64);
    let mut out = vec![[0f32; 4]; (w * h) as usize];
    for y in 0..h {
        let (y0, y1) = (y as f64 * fy, (y + 1) as f64 * fy);
        for x in 0..w {
            let (x0, x1) = (x as f64 * fx, (x + 1) as f64 * fx);
            let mut acc = [0f64; 4];
            let mut wsum = 0f64;
            let mut sy = y0.floor() as u32;
            while (sy as f64) < y1 && sy < img.height() {
                let wy = (y1.min(sy as f64 + 1.0) - y0.max(sy as f64)).max(0.0);
                let mut sx = x0.floor() as u32;
                while (sx as f64) < x1 && sx < img.width() {
                    let wx = (x1.min(sx as f64 + 1.0) - x0.max(sx as f64)).max(0.0);
                    let p = img.get_pixel(sx, sy);
                    let a = p[3] as f64 / 255.0;
                    let wgt = wx * wy;
                    for k in 0..3 {
                        acc[k] += p[k] as f64 * a * wgt;
                    }
                    acc[3] += p[3] as f64 * wgt;
                    wsum += wgt;
                    sx += 1;
                }
                sy += 1;
            }
            let i = (y * w + x) as usize;
            for k in 0..4 {
                out[i][k] = (acc[k] / wsum.max(1e-9)) as f32;
            }
        }
    }
    out
}

fn collect_nodes(g: &usvg::Group, out: &mut std::collections::HashMap<String, (Aff, Option<(f64, f64, f64, f64)>)>) {
    for n in g.children() {
        let id = n.id();
        if !id.is_empty() {
            let bb = n.abs_bounding_box();
            let r = (bb.left() as f64, bb.top() as f64, bb.right() as f64, bb.bottom() as f64);
            let e = out.entry(id.to_string()).or_insert((from_usvg(n.abs_transform()), None));
            e.1 = Some(match e.1 {
                None => r,
                Some(o) => (o.0.min(r.0), o.1.min(r.1), o.2.max(r.2), o.3.max(r.3)),
            });
        }
        if let usvg::Node::Group(c) = n {
            collect_nodes(c, out);
        }
    }
}

const SHAPES: [&str; 9] = ["path", "circle", "ellipse", "rect", "line", "polygon", "polyline", "use", "g"];

fn setup(text: String, img: &image::RgbaImage, long: u32, opt: &usvg::Options) -> Result<(Problem, State), String> {
    let elems = scan(&text);
    let tree = usvg::Tree::from_str(&text, opt).map_err(|e| format!("the SVG doesn't parse ({e})"))?;
    let size = tree.size();
    let (iw, ih) = (img.width() as f64, img.height() as f64);
    let s = (long as f64 / iw.max(ih)).min(1.0);
    let (w, h) = (((iw * s).round() as u32).max(1), ((ih * s).round() as u32).max(1));
    let render_scale = ((w as f64 / size.width() as f64) * (h as f64 / size.height() as f64)).sqrt();

    // Probe copy with an id on every element so usvg tells us transforms and boxes.
    let mut probe = String::new();
    let mut prev = 0;
    let mut ids = Vec::new();
    for (i, e) in elems.iter().enumerate() {
        let id = match e.attr(&text, "id") {
            Some(v) => v.to_string(),
            None => {
                let id = format!("zz-polish-{i}");
                probe.push_str(&text[prev..e.name_end]);
                probe.push_str(&format!(" id=\"{id}\""));
                prev = e.name_end;
                id
            }
        };
        ids.push(id);
    }
    probe.push_str(&text[prev..]);
    let ptree = usvg::Tree::from_str(&probe, opt).map_err(|e| format!("probe parse failed ({e})"))?;
    let mut nodes = std::collections::HashMap::new();
    collect_nodes(ptree.root(), &mut nodes);
    let root_abs = from_usvg(ptree.root().abs_transform());

    // Absolute transform of each element (own transform included); falls back up the tree.
    let mut abs: Vec<Aff> = Vec::with_capacity(elems.len());
    for (i, e) in elems.iter().enumerate() {
        let a = match nodes.get(&ids[i]) {
            Some((t, _)) => *t,
            None => {
                let base = e.parent.map(|p| abs[p]).unwrap_or(root_abs);
                match e.attr(&text, "transform").and_then(|t| t.parse::<svgtypes::Transform>().ok()) {
                    Some(t) if !e.in_defs => mul(&base, &[t.a, t.b, t.c, t.d, t.e, t.f]),
                    _ => base,
                }
            }
        };
        abs.push(a);
    }
    let own_t = |e: &Elem| -> Aff {
        e.attr(&text, "transform").and_then(|t| t.parse::<svgtypes::Transform>().ok()).map(|t| [t.a, t.b, t.c, t.d, t.e, t.f]).unwrap_or(ID)
    };

    let tb = text.as_bytes();
    let mut slots = Vec::new();
    for (ei, e) in elems.iter().enumerate() {
        if ei == 0 || e.in_filter || e.parent.is_none() {
            continue;
        }
        let ppu = render_scale * scale_of(&abs[ei]);
        let parent_ppu = render_scale * scale_of(&mul(&abs[ei], &inv(&own_t(e))));
        let grad = e.name == "linearGradient" || e.name == "radialGradient";
        let obb = grad && e.attr(&text, "gradientUnits") != Some("userSpaceOnUse");
        for a in &e.attrs {
            let n = a.name.as_str();
            let role = match n {
                "rx" => 3,
                "ry" => 4,
                _ => 0,
            };
            let mut push = |st: usize, en: usize, v: f64, kind: Kind, axis: Axis, ppu: f64, role: u8| {
                slots.push(Slot { start: st, end: en, kind, axis, elem: ei, orig: [v, 0.0, 0.0], ppu, role })
            };
            match n {
                "d" => {
                    for (st, en, v, axis, kind, role) in path_nums(tb, a.vs, a.ve) {
                        push(st, en, v, kind, axis, ppu, role);
                    }
                }
                "points" => {
                    let mut k = 0;
                    let mut i = a.vs;
                    while i < a.ve {
                        if let Some((en, v)) = num_at(tb, i) {
                            push(i, en, v, Kind::Coord, if k % 2 == 0 { Axis::X } else { Axis::Y }, ppu, 0);
                            k += 1;
                            i = en;
                        } else {
                            i += 1;
                        }
                    }
                }
                "transform" | "gradientTransform" => {
                    let p = if n == "transform" { parent_ppu } else { render_scale * scale_of(&root_abs) };
                    for (st, en, v, kind) in transform_nums(tb, a.vs, a.ve) {
                        push(st, en, v, kind, Axis::No, p, 0);
                    }
                }
                "x" | "y" | "cx" | "cy" | "r" | "rx" | "ry" | "x1" | "y1" | "x2" | "y2" | "fx" | "fy" | "width" | "height" | "stroke-width" | "offset" | "opacity"
                | "fill-opacity" | "stroke-opacity" | "stop-opacity" => {
                    let Some((en, v)) = num_at(tb, a.vs) else { continue };
                    if en > a.ve || e.name == "svg" {
                        continue;
                    }
                    let pct = en < a.ve && tb[en] == b'%';
                    let (kind, axis) = match n {
                        "offset" | "opacity" | "fill-opacity" | "stroke-opacity" | "stop-opacity" => {
                            if pct {
                                (Kind::Unit { lo: 0.0, hi: 100.0, step: 2.0 }, Axis::No)
                            } else {
                                (Kind::Unit { lo: 0.0, hi: 1.0, step: 0.02 }, Axis::No)
                            }
                        }
                        _ if obb || pct => (Kind::Unit { lo: -1e9, hi: 1e9, step: if pct { 2.0 } else { 0.02 } }, Axis::No),
                        "x" | "cx" | "x1" | "x2" | "fx" => (Kind::Coord, Axis::X),
                        "y" | "cy" | "y1" | "y2" | "fy" => (Kind::Coord, Axis::Y),
                        _ => (Kind::Coord, Axis::Len),
                    };
                    let p = if grad { render_scale * scale_of(&root_abs) } else { ppu };
                    push(a.vs, en, v, kind, axis, p, role);
                }
                "fill" | "stroke" | "stop-color" | "flood-color" | "color" | "style" => {
                    for (st, en, c) in hex_colours(&text, a.vs, a.ve) {
                        slots.push(Slot { start: st, end: en, kind: Kind::Colour, axis: Axis::No, elem: ei, orig: c, ppu, role: 0 });
                    }
                }
                _ => {}
            }
        }
    }

    // Movers: drawn elements, children of the root or of a top-level group.
    let mut movers = Vec::new();
    for (ei, e) in elems.iter().enumerate() {
        let Some(p) = e.parent else { continue };
        if e.in_defs || !SHAPES.contains(&e.name.as_str()) {
            continue;
        }
        let depth_ok = p == 0 || (elems[p].parent == Some(0) && elems[p].name == "g");
        if !depth_ok {
            continue;
        }
        // Box: union of this element's and its descendants' nodes.
        let mut bb: Option<(f64, f64, f64, f64)> = None;
        for (dj, _) in elems.iter().enumerate() {
            let mut k = Some(dj);
            let mut inside = false;
            while let Some(x) = k {
                if x == ei {
                    inside = true;
                    break;
                }
                k = elems[x].parent;
            }
            if !inside {
                continue;
            }
            if let Some((_, Some(r))) = nodes.get(&ids[dj]) {
                bb = Some(match bb {
                    None => *r,
                    Some(o) => (o.0.min(r.0), o.1.min(r.1), o.2.max(r.2), o.3.max(r.3)),
                });
            }
        }
        let Some(bb) = bb else { continue };
        let pt = mul(&abs[ei], &inv(&own_t(e)));
        let (cx, cy) = apply(&inv(&pt), (bb.0 + bb.2) / 2.0, (bb.1 + bb.3) / 2.0);
        let ppu = render_scale * scale_of(&pt);
        let half = render_scale * ((bb.2 - bb.0).powi(2) + (bb.3 - bb.1).powi(2)).sqrt() / 2.0;
        let own: Vec<usize> = slots.iter().enumerate().filter(|(_, s)| s.elem == ei && s.axis != Axis::No).map(|(i, _)| i).collect();
        let leaf = matches!(e.name.as_str(), "path" | "circle" | "ellipse" | "rect" | "line" | "polygon" | "polyline");
        let bake = leaf && e.attr(&text, "transform").is_none() && !own.is_empty();
        let prefix = if bake {
            None
        } else {
            let (start, insert) = match e.attrs.iter().find(|a| a.name == "transform") {
                Some(a) => (a.vs, false),
                None => (e.name_end, true),
            };
            slots.push(Slot { start, end: start, kind: Kind::Prefix { cx, cy, insert }, axis: Axis::No, elem: ei, orig: [0.0, 0.0, 1.0], ppu, role: 0 });
            Some(slots.len() - 1)
        };
        movers.push(Mover { prefix, own, cx, cy, ppu, half: half.max(2.0) });
    }
    // Text order; a prefix goes before numbers starting at the same place.
    let mut order: Vec<usize> = (0..slots.len()).collect();
    order.sort_by_key(|&i| (slots[i].start, !matches!(slots[i].kind, Kind::Prefix { .. })));
    let remap: Vec<usize> = {
        let mut r = vec![0; order.len()];
        for (new, &old) in order.iter().enumerate() {
            r[old] = new;
        }
        r
    };
    let mut sorted: Vec<Slot> = Vec::with_capacity(slots.len());
    let mut tmp: Vec<Option<Slot>> = slots.into_iter().map(Some).collect();
    for &old in &order {
        sorted.push(tmp[old].take().unwrap());
    }
    for m in &mut movers {
        m.prefix = m.prefix.map(|p| remap[p]);
        for o in &mut m.own {
            *o = remap[*o];
        }
    }

    // Equal rx/ry pairs (round arcs, circles drawn as ellipses, square-cornered
    // rounding) stay equal: the second follows the first.
    let mut followers: Vec<Vec<usize>> = vec![Vec::new(); sorted.len()];
    let mut follows = vec![false; sorted.len()];
    for i in 0..sorted.len() {
        let a = &sorted[i];
        let partner = match a.role {
            1 => (i + 1 < sorted.len() && sorted[i + 1].role == 2).then_some(i + 1),
            3 => (0..sorted.len()).find(|&j| sorted[j].role == 4 && sorted[j].elem == a.elem),
            _ => None,
        };
        if let Some(j) = partner {
            if sorted[j].orig[0] == a.orig[0] && sorted[j].elem == a.elem {
                followers[i].push(j);
                follows[j] = true;
            }
        }
    }
    let small = shrink(img, w, h);
    let lin = linear_table();
    let q = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    let lab_b = small.iter().map(|p| lab(&lin, q(p[0]), q(p[1]), q(p[2]))).collect();
    let lab_w = small.iter().map(|p| lab(&lin, q(p[0] + 255.0 - p[3]), q(p[1] + 255.0 - p[3]), q(p[2] + 255.0 - p[3]))).collect();
    let alpha = small.iter().map(|p| if p[3] > 0.5 { 255 } else { 0 }).collect();
    let st = State { vals: sorted.iter().map(|s| s.orig).collect(), mv: vec![[0.0, 0.0, 1.0]; movers.len()] };
    Ok((Problem { text, slots: sorted, movers, w, h, lab_b, lab_w, alpha, lin, long_px: w.max(h) as f64, num_cap: 0.04, followers, follows }, st))
}

// ---------------------------------------------------------------- search

#[derive(Clone, Copy)]
enum Var {
    Num { slot: usize, ch: usize },
    Move { m: usize, what: usize },
}

impl Problem {
    /// Apply one step of `var` (signed multiple `dir` of the level's step). None if out of bounds.
    fn step(&self, st: &State, var: Var, level: f64, dir: f64) -> Option<State> {
        let mut n = st.clone();
        let cap_px = 0.04 * self.long_px;
        match var {
            Var::Num { slot, ch } => {
                let s = &self.slots[slot];
                let v = &mut n.vals[slot][ch];
                let (d, lo, hi) = match s.kind {
                    Kind::Coord => {
                        let d = level / s.ppu;
                        let c = self.num_cap * self.long_px / s.ppu;
                        (d, s.orig[0] - c, s.orig[0] + c)
                    }
                    Kind::Angle => (level, s.orig[0] - 15.0, s.orig[0] + 15.0),
                    Kind::Mat => (0.01 * level, s.orig[0] - 0.2, s.orig[0] + 0.2),
                    Kind::Unit { lo, hi, step } => (step * level, lo.max(s.orig[0] - 30.0 * step), hi.min(s.orig[0] + 30.0 * step)),
                    Kind::Colour => (8.0 * level, 0.0, 255.0),
                    Kind::Prefix { .. } => return None,
                };
                let nv = *v + dir * d;
                if nv < lo - 1e-9 || nv > hi + 1e-9 {
                    return None;
                }
                *v = nv;
                for &f in &self.followers[slot] {
                    n.vals[f][0] = nv;
                }
            }
            Var::Move { m, what } => {
                let mo = &self.movers[m];
                let mv = &mut n.mv[m];
                let (dx, dy, k) = match what {
                    0 => (dir * level / mo.ppu, 0.0, 1.0),
                    1 => (0.0, dir * level / mo.ppu, 1.0),
                    _ => (0.0, 0.0, 1.0 + dir * level / mo.half),
                };
                mv[0] += dx;
                mv[1] += dy;
                mv[2] *= k;
                if (mv[0] * mo.ppu).abs() > 1.5 * cap_px || (mv[1] * mo.ppu).abs() > 1.5 * cap_px || !(0.8..=1.25).contains(&mv[2]) {
                    return None;
                }
                match mo.prefix {
                    Some(p) => n.vals[p] = *mv,
                    None => {
                        let (cx, cy) = (mo.cx + mv[0] - dx, mo.cy + mv[1] - dy);
                        for &i in &mo.own {
                            let v = &mut n.vals[i][0];
                            match self.slots[i].axis {
                                Axis::X => *v = cx + (*v - cx) * k + dx,
                                Axis::Y => *v = cy + (*v - cy) * k + dy,
                                Axis::Len => *v *= k,
                                Axis::No => {}
                            }
                        }
                    }
                }
            }
        }
        Some(n)
    }

    fn sweep(&self, st: &mut State, cur: &mut f64, vars: &[Var], level: f64, deadline: Instant, opt: &usvg::Options, evals: &mut usize) -> bool {
        let eps = 0.002;
        let mut any = false;
        loop {
            if Instant::now() > deadline {
                return any;
            }
            let base = st.clone();
            let b0 = self.base(st, opt);
            let mut cb = b0.clone();
            let trials: Vec<(Var, f64)> = vars.iter().flat_map(|&v| [(v, 1.0), (v, -1.0)]).collect();
            let mut gains: Vec<(f64, Var, f64)> = trials
                .par_iter()
                .filter_map(|&(v, d)| {
                    if Instant::now() > deadline {
                        return None;
                    }
                    let n = self.step(&base, v, level, d)?;
                    let s = self.score_vs(&b0, &n, opt).0;
                    (s < *cur - eps).then_some((*cur - s, v, d))
                })
                .collect();
            *evals += trials.len();
            if gains.is_empty() {
                return any;
            }
            gains.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
            let mut took = 0;
            for (_, v, d) in gains {
                let Some(n) = self.step(st, v, level, d) else { continue };
                let (s, pm) = self.score_vs(&cb, &n, opt);
                *evals += 1;
                if s < *cur - eps {
                    self.adopt(&mut cb, pm.as_ref().unwrap());
                    *st = n;
                    *cur = s;
                    took += 1;
                }
            }
            if took == 0 {
                return any;
            }
            any = true;
        }
    }
}

/// The SVG drawn by resvg at w x h, as PNG (for checking against other renderers).
pub fn render_png(svg: &str, w: u32, h: u32) -> Option<Vec<u8>> {
    let mut opt = usvg::Options::default();
    opt.fontdb_mut().load_system_fonts();
    let tree = usvg::Tree::from_str(svg, &opt).ok()?;
    let size = tree.size();
    let mut pm = tiny_skia::Pixmap::new(w, h)?;
    resvg::render(&tree, tiny_skia::Transform::from_scale(w as f32 / size.width(), h as f32 / size.height()), &mut pm.as_mut());
    pm.encode_png().ok()
}

impl Problem {
    /// Put changed numbers back where that costs (almost) nothing: drops
    /// changes that bought nothing, such as numbers inside a shape faded out.
    fn revert(&self, st: &mut State, cur: &mut f64, deadline: Instant, opt: &usvg::Options, evals: &mut usize) -> usize {
        let limit = *cur * 1.005;
        let cands: Vec<usize> = (0..self.slots.len())
            .filter(|&i| !self.follows[i] && !matches!(self.slots[i].kind, Kind::Prefix { .. }) && st.vals[i] != self.slots[i].orig)
            .collect();
        let undo = |s: &State, i: usize| {
            let mut n = s.clone();
            n.vals[i] = self.slots[i].orig;
            for &f in &self.followers[i] {
                n.vals[f] = self.slots[f].orig;
            }
            n
        };
        let base = st.clone();
        let b0 = self.base(st, opt);
        let mut cb = b0.clone();
        let c0 = *cur;
        let mut ok: Vec<(f64, usize)> = cands
            .par_iter()
            .filter_map(|&i| {
                if Instant::now() > deadline {
                    return None;
                }
                let s = self.score_vs(&b0, &undo(&base, i), opt).0;
                (s <= c0 + 0.0005).then_some((s, i))
            })
            .collect();
        *evals += cands.len();
        ok.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let mut n = 0;
        for (_, i) in ok {
            let t = undo(st, i);
            let (s, pm) = self.score_vs(&cb, &t, opt);
            *evals += 1;
            if s <= *cur + 0.0005 && s <= limit {
                self.adopt(&mut cb, pm.as_ref().unwrap());
                *st = t;
                *cur = s;
                n += 1;
            }
        }
        n
    }
}

pub struct Report {
    pub svg: String,
    pub before: f64,
    pub after: f64,
    pub evals: usize,
    pub secs: f64,
    pub slots: usize,
    pub movers: usize,
    pub log: Vec<String>,
}

pub fn polish(svg: String, img: &image::RgbaImage, long: u32, seconds: f64, colours: bool, moves_only: bool, num_cap: f64) -> Result<Report, String> {
    let t0 = Instant::now();
    let deadline = t0 + std::time::Duration::from_secs_f64(seconds * 0.85);
    let mut opt = usvg::Options::default();
    opt.fontdb_mut().load_system_fonts();
    let (mut p, mut st) = setup(svg, img, long, &opt)?;
    p.num_cap = num_cap;
    let before = p.score(&st, &opt);
    if std::env::var_os("POLISH_BENCH").is_some() {
        // Where a render's time goes: build text, parse, draw, score.
        let n = 100;
        let (mut tb, mut tp, mut tr, mut ts) = (0f64, 0f64, 0f64, 0f64);
        for _ in 0..n {
            let t = Instant::now();
            let svg = p.build(&st);
            tb += t.elapsed().as_secs_f64();
            let t = Instant::now();
            let tree = usvg::Tree::from_str(&svg, &opt).unwrap();
            tp += t.elapsed().as_secs_f64();
            let t = Instant::now();
            let size = tree.size();
            let mut pm = tiny_skia::Pixmap::new(p.w, p.h).unwrap();
            resvg::render(&tree, tiny_skia::Transform::from_scale(p.w as f32 / size.width(), p.h as f32 / size.height()), &mut pm.as_mut());
            tr += t.elapsed().as_secs_f64();
            let t = Instant::now();
            std::hint::black_box(p.score_pixmap(&pm));
            ts += t.elapsed().as_secs_f64();
        }
        let ms = |x: f64| x * 1000.0 / n as f64;
        eprintln!("bench per render: build {:.2} ms, parse {:.2} ms, draw {:.2} ms, score {:.2} ms", ms(tb), ms(tp), ms(tr), ms(ts));
    }
    let mut cur = before;
    let mut log = vec![format!("setup {:.2}s, {} slots, {} movers, {}x{} raster, ΔE {:.3}", t0.elapsed().as_secs_f64(), p.slots.len(), p.movers.len(), p.w, p.h, before)];
    let moves: Vec<Var> = (0..p.movers.len()).flat_map(|m| (0..3).map(move |w| Var::Move { m, what: w })).collect();
    let nums: Vec<Var> = p
        .slots
        .iter()
        .enumerate()
        .flat_map(|(i, s)| {
            let n = match s.kind {
                _ if p.follows[i] => 0,
                Kind::Prefix { .. } => 0,
                Kind::Colour if colours => 3,
                Kind::Colour => 0,
                _ => 1,
            };
            (0..n).map(move |ch| Var::Num { slot: i, ch })
        })
        .collect();
    let mut evals = 0;
    for level in [4.0, 2.0, 1.0, 0.5, 0.25] {
        p.sweep(&mut st, &mut cur, &moves, level, deadline, &opt, &mut evals);
        log.push(format!("  step {level} px: moves ΔE {cur:.3} ({:.1}s)", t0.elapsed().as_secs_f64()));
        if !moves_only {
            p.sweep(&mut st, &mut cur, &nums, level, deadline, &opt, &mut evals);
            log.push(format!("  step {level} px: numbers ΔE {cur:.3} ({:.1}s, {evals} renders)", t0.elapsed().as_secs_f64()));
        }
    }
    let rdeadline = Instant::now() + std::time::Duration::from_secs_f64((seconds * 0.15).max(1.0));
    let n = p.revert(&mut st, &mut cur, rdeadline, &opt, &mut evals);
    log.push(format!("  revert: {n} numbers put back, ΔE {cur:.3} ({:.1}s)", t0.elapsed().as_secs_f64()));
    Ok(Report { svg: p.build(&st), before, after: cur, evals, secs: t0.elapsed().as_secs_f64(), slots: p.slots.len(), movers: p.movers.len(), log })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_numbers_skip_arc_flags() {
        let d = "M10-5a5 5 0 015 5L.5.5Z";
        let n = path_nums(d.as_bytes(), 0, d.len());
        let v: Vec<f64> = n.iter().map(|x| x.2).collect();
        assert_eq!(v, vec![10.0, -5.0, 5.0, 5.0, 0.0, 5.0, 5.0, 0.5, 0.5]);
        assert_eq!(n[2].5, 1); // arc rx
        assert_eq!(n[3].5, 2); // arc ry
        assert!(n[4].4 == Kind::Angle);
    }

    #[test]
    fn rewritten_numbers_keep_their_separators() {
        let text = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20"><path d="M1.5.5L10-5" fill="#f00"/></svg>"##.to_string();
        let img = image::RgbaImage::from_pixel(20, 20, image::Rgba([255, 0, 0, 255]));
        let opt = usvg::Options::default();
        let (p, mut st) = setup(text, &img, 20, &opt).unwrap();
        assert_eq!(p.build(&st), p.text);
        for v in st.vals.iter_mut() {
            if v[0] == 0.5 {
                v[0] = 2.0;
            }
            if v[0] == -5.0 {
                v[0] = 3.0;
            }
        }
        assert!(p.build(&st).contains(r#"d="M1.5 2L10 3""#), "{}", p.build(&st));
    }
}
