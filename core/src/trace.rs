//! Tracing the index map colour by colour, and writing compact SVG paths.

use crate::index::index_map;
use crate::palette::Pal;
use crate::potrace::{self, Curve, Params, Seg};
use crate::{js_round, to_hex, Image, Opts};

/// A path command with Potrace's coordinates rounded to three decimals, as the
/// JS version read them back from Potrace's SVG text.
#[derive(Clone, Copy)]
enum Cmd {
    M([f64; 2]),
    C([f64; 6]),
    L([f64; 4]),
}

/// One closed path of a layer.
type Sub = Vec<Cmd>;

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
fn trace_layer(bits: &[u8], w: usize, h: usize, o: &Opts) -> Vec<Sub> {
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

/// Write the paths as rounded relative commands.
fn compact_path(subs: &[Sub], dec: u32) -> String {
    let f = 10f64.powi(dec as i32);
    let r = |v: f64| js_round(v * f) / f;
    let num = |v: f64| {
        let v = r(v);
        let s = if v == 0.0 { "0".to_string() } else { format!("{}", v) };
        if let Some(rest) = s.strip_prefix("0.") {
            format!(".{}", rest)
        } else if let Some(rest) = s.strip_prefix("-0.") {
            format!("-.{}", rest)
        } else {
            s
        }
    };
    let mut out = String::new();
    let (mut last, mut digit) = ('\0', false);
    let mut emit = |out: &mut String, last: &mut char, cmd: char, vals: &[f64]| {
        if !(cmd == *last && cmd != 'M') {
            out.push(cmd);
            digit = false;
        }
        for &v in vals {
            let t = num(v);
            if digit && !t.starts_with('-') {
                out.push(' ');
            }
            out.push_str(&t);
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
                    emit(&mut out, &mut last, 'M', &[cx, cy]);
                    open = true;
                }
                Cmd::C(v) => {
                    let p = v.map(r);
                    let mut rel = [0f64; 6];
                    for k in 0..6 {
                        rel[k] = r(p[k] - if k % 2 == 1 { cy } else { cx });
                    }
                    if rel.iter().any(|&v| v != 0.0) {
                        emit(&mut out, &mut last, 'c', &rel);
                    }
                    cx = p[4];
                    cy = p[5];
                }
                Cmd::L(v) => {
                    for k in 0..2 {
                        let (x, y) = (r(v[k * 2]), r(v[k * 2 + 1]));
                        let (dx, dy) = (r(x - cx), r(y - cy));
                        if dx != 0.0 || dy != 0.0 {
                            emit(&mut out, &mut last, 'l', &[dx, dy]);
                        }
                        cx = x;
                        cy = y;
                    }
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
fn ranks(p: &Pal) -> Vec<i32> {
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
    let rank = ranks(p);
    let bits: Vec<u8> = idx.iter().map(|&v| (v >= 0 && rank[v as usize] >= k as i32) as u8).collect();
    let d = compact_path(&trace_layer(&bits, w, h, o), o.dec);
    if d.is_empty() {
        return None;
    }
    let hex = to_hex(p.pal[p.order[k]]);
    let fill = if hex == "#000000" { String::new() } else { format!(" fill=\"{}\"", hex) };
    Some(format!("<path{} d=\"{}\"/>", fill, d))
}

pub fn svg(w: usize, h: usize, paths: &[String]) -> String {
    format!("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {} {}\" fill-rule=\"evenodd\">{}</svg>\n", w, h, paths.concat())
}

/// Quantize and trace the original drawn at `o.grid` (over white in mono mode).
/// Layers are traced in parallel when the `parallel` feature is on.
pub fn trace(r: &Image, o: &Opts, p: &Pal) -> Traced {
    let idx = index_map(r, o, p);
    let run = |k: usize| layer(&idx, r.w, r.h, o, p, k);
    #[cfg(feature = "parallel")]
    let paths: Vec<Option<String>> = {
        use rayon::prelude::*;
        (0..p.order.len()).into_par_iter().map(run).collect()
    };
    #[cfg(not(feature = "parallel"))]
    let paths: Vec<Option<String>> = (0..p.order.len()).map(run).collect();
    let paths: Vec<String> = paths.into_iter().flatten().collect();
    Traced { layers: paths.len(), svg: svg(r.w, r.h, &paths) }
}

pub struct Traced {
    pub svg: String,
    pub layers: usize,
}
