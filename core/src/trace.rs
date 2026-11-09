//! Tracing the index map colour by colour, and writing compact SVG paths.

use crate::index::index_map;
use crate::palette::Pal;
use crate::potrace::{self, Curve, Params, Seg};
use crate::{js_round, to_hex, Image, Mode, Opts};

/// A path command with Potrace's coordinates rounded to three decimals, as the
/// JS version read them back from Potrace's SVG text.
#[derive(Clone, Copy)]
pub(crate) enum Cmd {
    M([f64; 2]),
    C([f64; 6]),
    L([f64; 4]),
    /// A circular arc, only from [`crate::snap`]: radius, large-arc and sweep
    /// flags (0 or 1), end point, and the arc's true midpoint, which the
    /// writer uses to choose how many decimals the radius needs.
    A([f64; 7]),
}

impl Cmd {
    /// Every coordinate scaled by `s`.
    fn scaled(self, s: f64) -> Cmd {
        match self {
            Cmd::M(p) => Cmd::M(p.map(|v| v * s)),
            Cmd::C(p) => Cmd::C(p.map(|v| v * s)),
            Cmd::L(p) => Cmd::L(p.map(|v| v * s)),
            Cmd::A(p) => Cmd::A([p[0] * s, p[1], p[2], p[3] * s, p[4] * s, p[5] * s, p[6] * s]),
        }
    }
}

/// One closed path of a layer.
pub(crate) type Sub = Vec<Cmd>;

/// `parseFloat(v.toFixed(3))`: rounded to three decimals, exact halves away from zero.
fn fixed3(v: f64) -> f64 {
    let a = v.abs();
    let y = a * 1000.0;
    let f = y.floor();
    let n = if (y - f - 0.5).abs() > 1e-6 {
        if y - f > 0.5 {
            f + 1.0
        } else {
            f
        }
    } else {
        // Close to a half: decide on the exact value. It is exactly a half only when
        // `a` is an odd number of sixteenths, and toFixed rounds that up.
        let s = a * 16.0;
        if s.fract() == 0.0 && s % 2.0 == 1.0 {
            f + 1.0
        } else {
            format!("{:.3}", a).parse::<f64>().unwrap() * 1000.0
        }
    };
    let r = js_round(n) / 1000.0;
    if v < 0.0 {
        -r
    } else {
        r
    }
}

fn subpaths(curves: &[Curve]) -> Vec<Sub> {
    curves
        .iter()
        .filter(|c| !c.is_empty())
        .map(|c| {
            let start = c[c.len() - 1].end();
            let mut sub = vec![Cmd::M([fixed3(start.x), fixed3(start.y)])];
            for s in c {
                sub.push(match s {
                    Seg::Curve(p) => Cmd::C([
                        fixed3(p[0].x),
                        fixed3(p[0].y),
                        fixed3(p[1].x),
                        fixed3(p[1].y),
                        fixed3(p[2].x),
                        fixed3(p[2].y),
                    ]),
                    Seg::Corner(p) => Cmd::L([fixed3(p[0].x), fixed3(p[0].y), fixed3(p[1].x), fixed3(p[1].y)]),
                });
            }
            sub
        })
        .collect()
}

/// The larger of a path's width and height, control points included.
fn span(sub: &Sub) -> f64 {
    let (mut x0, mut x1, mut y0, mut y1) = (f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY);
    let mut pt = |x: f64, y: f64| {
        x0 = x0.min(x);
        x1 = x1.max(x);
        y0 = y0.min(y);
        y1 = y1.max(y);
    };
    for c in sub {
        let v: &[f64] = match c {
            Cmd::M(v) => v,
            Cmd::C(v) => v,
            Cmd::L(v) => v,
            Cmd::A(v) => &v[3..],
        };
        for p in v.chunks_exact(2) {
            pt(p[0], p[1]);
        }
    }
    (x1 - x0).max(y1 - y0)
}

/// Smoothing merges curve segments, which is invisible on a big sweeping edge
/// and obvious on an eye or a letter. So trace twice and let every shape smaller
/// than the "protect" size take the lightly smoothed version.
pub(crate) fn trace_layer(bits: &[u8], w: usize, h: usize, o: &Opts) -> Vec<Sub> {
    let s = w.max(h) as f64 / o.src as f64;
    if o.src > 0 && s >= 1.5 && !o.upscale {
        return native(bits, w, h, o, s);
    }
    let params = Params { turdsize: 2, alphamax: o.alpha, opttolerance: o.tol };
    if o.protect == 0.0 || o.tol <= 0.15 {
        return subpaths(&potrace::trace(w, h, bits, &params));
    }
    let mut both = potrace::trace_tolerances(w, h, bits, &params, &[o.tol, 0.1]);
    let fine = subpaths(&both.pop().unwrap());
    let coarse = subpaths(&both.pop().unwrap());
    if coarse.len() != fine.len() {
        return fine;
    }
    let limit = w.max(h) as f64 * o.protect / 100.0;
    coarse.into_iter().zip(fine).map(|(a, b)| if span(&b) < limit { b } else { a }).collect()
}

/// Trace a shape at the bitmap's own size, 1/`s` of the grid, where its
/// jaggies are one pixel high and the tracer smooths them away, then scale it
/// back up. Each small pixel is set when most of the grid pixels in it are.
fn native(bits: &[u8], w: usize, h: usize, o: &Opts, s: f64) -> Vec<Sub> {
    let (sw, sh) = ((w as f64 / s).ceil() as usize, (h as f64 / s).ceil() as usize);
    // Area-weighted: a grid pixel straddling two small pixels counts towards
    // both by how much of it falls in each. Binning whole grid pixels by their
    // index gave cells of uneven size when s isn't whole (at s 1.7 some held
    // one grid pixel and some two), which put a ripple of 1 px notches along
    // every diagonal and curve.
    let spans = |n: usize, m: usize| -> Vec<[(usize, f32); 2]> {
        (0..n)
            .map(|i| {
                let (a, b) = (i as f64 / s, (i + 1) as f64 / s);
                let k = (a as usize).min(m - 1);
                let edge = (k + 1) as f64;
                if b <= edge || k + 1 >= m { [(k, 1.0), (k, 0.0)] } else { [(k, ((edge - a) * s) as f32), (k + 1, ((b - edge) * s) as f32)] }
            })
            .collect()
    };
    let (xs, ys) = (spans(w, sw), spans(h, sh));
    let (mut on, mut all) = (vec![0f32; sw * sh], vec![0f32; sw * sh]);
    for y in 0..h {
        for &(ky, wy) in &ys[y] {
            if wy == 0.0 {
                continue;
            }
            let row = ky * sw;
            for x in 0..w {
                let b = bits[y * w + x] as f32;
                for &(kx, wx) in &xs[x] {
                    let a = wx * wy;
                    all[row + kx] += a;
                    on[row + kx] += a * b;
                }
            }
        }
    }
    let small: Vec<u8> = on.iter().zip(&all).map(|(&n, &a)| (a > 0.0 && n * 2.0 >= a) as u8).collect();
    // Smoothing is a distance: keep it the same on the grid.
    trace_layer(&small, sw, sh, &Opts { src: 0, tol: o.tol / s, ..o.clone() })
        .into_iter()
        .map(|sub| {
            sub.into_iter().map(|c| c.scaled(s)).collect()
        })
        .collect()
}

/// Trace a shape at 1/`f` of its size and scale the result back up: for shapes
/// that are blurred, or only clip, detail finer than `f` px never shows.
pub(crate) fn coarse_d(bits: &[u8], w: usize, h: usize, o: &Opts, f: usize) -> String {
    if f <= 1 {
        return compact_path(&trace_layer(bits, w, h, o), o.dec);
    }
    let (sw, sh) = (w.div_ceil(f), h.div_ceil(f));
    let mut cnt = vec![0usize; sw * sh];
    for y in 0..h {
        for x in 0..w {
            if bits[y * w + x] != 0 {
                cnt[(y / f) * sw + x / f] += 1;
            }
        }
    }
    let small: Vec<u8> = cnt.iter().map(|&c| (c * 2 >= f * f) as u8).collect();
    let k = f as f64;
    let subs: Vec<Sub> = trace_layer(&small, sw, sh, &Opts { protect: 0.0, src: 0, ..o.clone() })
        .into_iter()
        .map(|sub| sub.into_iter().map(|c| c.scaled(k)).collect())
        .collect();
    compact_path(&subs, o.dec)
}

/// Write the paths as rounded relative commands.
pub(crate) fn compact_path(subs: &[Sub], dec: u32) -> String {
    write_path(subs, dec, false)
}

/// `v` rounded to `dec` decimals and written as briefly as JS would, without
/// a leading zero.
fn fmt_num(v: f64, dec: u32) -> String {
    let f = 10f64.powi(dec as i32);
    let v = js_round(v * f) / f;
    let s = if v == 0.0 { "0".to_string() } else { format!("{}", v) };
    if let Some(rest) = s.strip_prefix("0.") {
        format!(".{}", rest)
    } else if let Some(rest) = s.strip_prefix("-0.") {
        format!("-.{}", rest)
    } else {
        s
    }
}

/// The radius to write for an arc from `s` to `e` (both already rounded): the
/// shortest that keeps the arc's midpoint within three quarters of the last
/// decimal of the true one, else the closest. A nearly half-circle arc swings
/// a long way for a small change of radius, and rounding the end points alone
/// can push it out, so a radius just short of half the chord is tried too:
/// SVG stretches that to fit exactly, which makes a clean half circle.
fn arc_radius(s: [f64; 2], e: [f64; 2], a: &[f64; 7], dec: u32) -> String {
    let (large, sw) = (a[1] != 0.0, a[2] != 0.0);
    let err = |r: f64| crate::snap::arc_mid(s, e, r, large, sw).map_or(f64::INFINITY, |m| (m[0] - a[5]).hypot(m[1] - a[6]));
    let limit = 0.75 * 10f64.powi(-(dec as i32));
    let half = (e[0] - s[0]).hypot(e[1] - s[1]) / 2.0;
    let mut best = (f64::INFINITY, String::new());
    for p in dec..=dec + 3 {
        let f = 10f64.powi(p as i32);
        for r in [js_round(a[0] * f) / f, (a[0] * f).floor() / f, (half * f).floor() / f] {
            if r <= 0.0 {
                continue;
            }
            let x = err(r);
            if x <= limit {
                return fmt_num(r, p);
            }
            if x < best.0 {
                best = (x, fmt_num(r, p));
            }
        }
    }
    best.1
}

/// Write the paths as rounded relative commands; with `hv`, level and upright
/// lines as `h` and `v`.
pub(crate) fn write_path(subs: &[Sub], dec: u32, hv: bool) -> String {
    let f = 10f64.powi(dec as i32);
    let r = |v: f64| js_round(v * f) / f;
    let num = |v: f64| fmt_num(v, dec);
    let nums = |vals: &[f64]| -> Vec<String> { vals.iter().map(|&v| num(v)).collect() };
    let mut out = String::new();
    let (mut last, mut digit) = ('\0', false);
    let mut emit = |out: &mut String, last: &mut char, cmd: char, vals: &[String]| {
        if !(cmd == *last && cmd != 'M') {
            out.push(cmd);
            digit = false;
        }
        for t in vals {
            if digit && !t.starts_with('-') {
                out.push(' ');
            }
            out.push_str(t);
            digit = true;
        }
        *last = cmd;
    };
    let (mut cx, mut cy, mut open) = (0.0, 0.0, false);
    for sub in subs {
        for c in sub {
            match *c {
                Cmd::M(p) => {
                    if open {
                        out.push('z');
                        last = 'z';
                        // the next command is always M, which resets the digit state
                    }
                    cx = r(p[0]);
                    cy = r(p[1]);
                    emit(&mut out, &mut last, 'M', &nums(&[cx, cy]));
                    open = true;
                }
                Cmd::C(v) => {
                    let p = v.map(r);
                    let mut rel = [0f64; 6];
                    for k in 0..6 {
                        rel[k] = r(p[k] - if k % 2 == 1 { cy } else { cx });
                    }
                    if rel.iter().any(|&v| v != 0.0) {
                        emit(&mut out, &mut last, 'c', &nums(&rel));
                    }
                    cx = p[4];
                    cy = p[5];
                }
                Cmd::L(v) => {
                    for k in 0..2 {
                        let (x, y) = (r(v[k * 2]), r(v[k * 2 + 1]));
                        let (dx, dy) = (r(x - cx), r(y - cy));
                        if hv && dy == 0.0 && dx != 0.0 {
                            emit(&mut out, &mut last, 'h', &nums(&[dx]));
                        } else if hv && dx == 0.0 && dy != 0.0 {
                            emit(&mut out, &mut last, 'v', &nums(&[dy]));
                        } else if dx != 0.0 || dy != 0.0 {
                            emit(&mut out, &mut last, 'l', &nums(&[dx, dy]));
                        }
                        cx = x;
                        cy = y;
                    }
                }
                Cmd::A(a) => {
                    let (x, y) = (r(a[3]), r(a[4]));
                    let (dx, dy) = (r(x - cx), r(y - cy));
                    if dx != 0.0 || dy != 0.0 {
                        let rad = arc_radius([cx, cy], [x, y], &a, dec);
                        let flag = |v: f64| if v != 0.0 { "1" } else { "0" }.to_string();
                        let vals = [rad.clone(), rad, "0".into(), flag(a[1]), flag(a[2]), num(dx), num(dy)];
                        emit(&mut out, &mut last, 'a', &vals);
                    }
                    cx = x;
                    cy = y;
                }
            }
        }
    }
    if open {
        out.push('z');
    }
    out
}

/// The index map's rank of each palette colour: its position in the paint order.
pub(crate) fn ranks(p: &Pal) -> Vec<i32> {
    let mut rank = vec![-1i32; p.pal.len()];
    for (k, &ci) in p.order.iter().enumerate() {
        rank[ci] = k as i32;
    }
    rank
}

/// Trace layer `k` of an index map. Stacked: each layer also covers every colour
/// painted above it, so no gaps open between colours. Returns the `<path>`
/// element, or None when the layer is empty.
pub fn layer(idx: &[i16], w: usize, h: usize, o: &Opts, p: &Pal, k: usize) -> Option<String> {
    let d = layer_d(idx, w, h, o, &ranks(p), k);
    if d.is_empty() {
        return None;
    }
    let hex = to_hex(p.pal[p.order[k]]);
    let fill = if hex == "#000000" { String::new() } else { format!(" fill=\"{}\"", hex) };
    Some(format!("<path{} d=\"{}\"/>", fill, d))
}

/// The path data of layer `k`: every pixel ranked `k` or above.
pub(crate) fn layer_d(idx: &[i16], w: usize, h: usize, o: &Opts, rank: &[i32], k: usize) -> String {
    let bits: Vec<u8> = idx.iter().map(|&v| (v >= 0 && rank[v as usize] >= k as i32) as u8).collect();
    let subs = trace_layer(&bits, w, h, o);
    if !o.snap {
        return compact_path(&subs, o.dec);
    }
    // The scale a bitmap traced at its own size was enlarged by (see `native`).
    let s = if o.src > 0 && !o.upscale { w.max(h) as f64 / o.src as f64 } else { 1.0 };
    let s = if s >= 1.5 { s } else { 1.0 };
    write_path(&crate::snap::snap(subs, crate::snap::tolerance(w.max(h), s)), o.dec, true)
}

pub fn svg(w: usize, h: usize, paths: &[String]) -> String {
    format!("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {} {}\" fill-rule=\"evenodd\">{}</svg>\n", w, h, paths.concat())
}

/// Quantize and trace the original drawn at `o.grid` (over white in mono mode).
/// Layers are traced in parallel when the `parallel` feature is on.
pub fn trace(r: &Image, o: &Opts, p: &Pal) -> Traced {
    let idx = index_map(r, o, p);
    if o.gradients && o.mode == Mode::Colour {
        return crate::gradient::trace(r, o, p, &idx);
    }
    trace_index(&idx, r.w, r.h, o, p)
}

/// Trace every layer of an index map.
pub(crate) fn trace_index(idx: &[i16], w: usize, h: usize, o: &Opts, p: &Pal) -> Traced {
    let run = |k: usize| layer(idx, w, h, o, p, k);
    #[cfg(feature = "parallel")]
    let paths: Vec<Option<String>> = {
        use rayon::prelude::*;
        (0..p.order.len()).into_par_iter().map(run).collect()
    };
    #[cfg(not(feature = "parallel"))]
    let paths: Vec<Option<String>> = (0..p.order.len()).map(run).collect();
    let paths: Vec<String> = paths.into_iter().flatten().collect();
    Traced { layers: paths.len(), svg: svg(w, h, &paths) }
}

pub struct Traced {
    pub svg: String,
    pub layers: usize,
}
