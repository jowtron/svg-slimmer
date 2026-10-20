//! Keeping gradients (`Opts::gradients`).
//!
//! The palette gives every pixel one flat colour, so a smooth ramp comes out as
//! bands. Here the bands that are really one ramp are found and merged into one
//! shape, filled with an SVG gradient fitted to the original pixels. Soft glows
//! inside such an area become blurred shapes over the gradient, and a wide,
//! evenly fading halo around the artwork becomes a blurred copy of its outline
//! underneath. Each area's fit is checked against the bands it replaces, region
//! by region, and the bands stay wherever they are closer to the original.
//!
//! Finding the ramps: the index map is split into regions (connected patches of
//! one colour). Two touching regions belong to one ramp when the original:
//! - doesn't jump across their border (a real edge does), and has no step at it
//!   either, however small (a flat edge between close colours, a highlight's
//!   hard rim), measured wide enough to catch an upscaled edge;
//! - and either keeps changing further out on at least one side (a ramp does,
//!   on one side only next to its crest; a flat edge stops), or barely changes
//!   at all compared with how far apart the two palette colours are (the flat
//!   top of a ramp).
//!
//! Locked colours never join a ramp. Slivers of a ramp's colours cut off by a
//! sharp edge (the blended rim round a shape inside it) and small islands of
//! them join it too.
//!
//! Fitting: flat, linear or radial with 2 to 4 stops, or linear with a radial
//! sheen over it, each fitted robustly; the simplest that is nearly as good as
//! the best wins. Glows are what the gradient leaves over: one colour at one
//! opacity, blurred, round each shape the glow surrounds, so neighbouring glows
//! overlap the way separate glows do. The gradient is fitted again without the
//! glowing pixels, then the glows again over it.

#![cfg_attr(target_arch = "wasm32", allow(unused_variables))]

use crate::palette::Pal;
use crate::trace::{coarse_d, layer_d, ranks, trace_index, Traced};
use crate::{js_round, to_hex, Image, Opts};
use std::collections::HashMap;

/// Diagnostics on stderr when SLIMMER_DEBUG is set (native builds only).
macro_rules! dbg_line {
    ($($t:tt)*) => {{
        #[cfg(not(target_arch = "wasm32"))]
        if std::env::var_os("SLIMMER_DEBUG").is_some() {
            eprintln!($($t)*);
        }

    }};
}

type C3 = [f64; 3];

const NONE: u32 = u32::MAX;

/// A change across a few pixels at least this big (summed over RGBA) is an edge,
/// as in the edge snapping and the comparison.
const SHARP: i32 = 24;

fn rgb(d: &[u8], j: usize) -> C3 {
    [d[j * 4] as f64, d[j * 4 + 1] as f64, d[j * 4 + 2] as f64]
}

fn l1(d: &[u8], i: usize, k: usize) -> i32 {
    (0..4).map(|c| (d[i * 4 + c] as i32 - d[k * 4 + c] as i32).abs()).sum()
}

fn norm(v: C3) -> f64 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

fn sub(a: C3, b: C3) -> C3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: C3, b: C3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn clamp3(c: C3) -> C3 {
    c.map(|v| v.clamp(0.0, 255.0))
}

fn hex(c: C3) -> String {
    to_hex(c.map(|v| js_round(v.clamp(0.0, 255.0)) as i32))
}

/// A number for the SVG: `dec` decimals, no leading zero, no trailing zeros.
fn num(v: f64, dec: u32) -> String {
    let f = 10f64.powi(dec as i32);
    let v = js_round(v * f) / f;
    let s = if v == 0.0 { "0".to_string() } else { format!("{}", v) };
    if let Some(rest) = s.strip_prefix("0.") {
        format!(".{rest}")
    } else if let Some(rest) = s.strip_prefix("-0.") {
        format!("-.{rest}")
    } else {
        s
    }
}

fn percentile(v: &mut [f64], q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let k = ((v.len() - 1) as f64 * q).round() as usize;
    let (_, x, _) = v.select_nth_unstable_by(k, |a, b| a.partial_cmp(b).unwrap());
    *x
}

/// Solve `a x = b` (Gaussian elimination with partial pivoting).
fn solve(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Option<Vec<f64>> {
    let n = b.len();
    for c in 0..n {
        let p = (c..n).max_by(|&i, &j| a[i][c].abs().partial_cmp(&a[j][c].abs()).unwrap())?;
        if a[p][c].abs() < 1e-12 {
            return None;
        }
        a.swap(c, p);
        b.swap(c, p);
        for r in c + 1..n {
            let f = a[r][c] / a[c][c];
            for k in c..n {
                a[r][k] -= f * a[c][k];
            }
            b[r] -= f * b[c];
        }
    }
    let mut x = vec![0.0; n];
    for r in (0..n).rev() {
        let s: f64 = (r + 1..n).map(|k| a[r][k] * x[k]).sum();
        x[r] = (b[r] - s) / a[r][r];
    }
    Some(x)
}

// ---------- regions and their borders ----------

/// Connected patches of one colour (4-connected). `lab` is NONE for background.
struct Regions {
    lab: Vec<u32>,
    col: Vec<i16>,
    n: Vec<usize>,
}

fn regions(idx: &[i16], w: usize, h: usize) -> Regions {
    let total = w * h;
    let mut lab = vec![NONE; total];
    let (mut col, mut n, mut stack) = (Vec::new(), Vec::new(), Vec::new());
    for j0 in 0..total {
        if idx[j0] < 0 || lab[j0] != NONE {
            continue;
        }
        let (id, c) = (col.len() as u32, idx[j0]);
        lab[j0] = id;
        stack.push(j0);
        let mut count = 0;
        while let Some(j) = stack.pop() {
            count += 1;
            let x = j % w;
            for (ok, k) in [(x > 0, j.wrapping_sub(1)), (x + 1 < w, j + 1), (j >= w, j.wrapping_sub(w)), (j + w < total, j + w)] {
                if ok && idx[k] == c && lab[k] == NONE {
                    lab[k] = id;
                    stack.push(k);
                }
            }
        }
        col.push(c);
        n.push(count);
    }
    Regions { lab, col, n }
}

/// What the original does across the border between two regions, summed over
/// every pixel step that crosses it, oriented from the lower region id to the
/// higher and measured along the difference of their palette colours.
#[derive(Default, Clone, Copy)]
struct Border {
    n: u32,
    /// Steps where the original jumps: a real edge.
    sharp: u32,
    /// Change from `k` px on one side to `k` px on the other.
    inner: f64,
    /// Change from `k` to `3k` px out on each side. A ramp keeps changing
    /// (outer ≈ inner) on at least one side: on both in the middle of a ramp,
    /// on one side only next to its crest. A flat edge stops on both.
    outer: [f64; 2],
    /// Change across the border's middle (`k/2` px either side, wide enough
    /// for an upscaled edge), less half of `inner`: zero on a ramp, half the
    /// step on a step at the border, however small, as at a soft highlight's
    /// hard edge.
    step: f64,
}

struct Borders {
    pairs: HashMap<(u32, u32), Border>,
    /// Every border step of each region, including those to the background and
    /// the canvas edge: used to find regions enclosed by an area.
    contacts: Vec<u32>,
}

fn borders(r: &Image, reg: &Regions, p: &Pal) -> Borders {
    let (w, h, d) = (r.w, r.h, r.data);
    let long = w.max(h) as f64;
    let st = (js_round(long / 2048.0) as usize).max(1);
    let k = (js_round(long / 256.0) as usize).max(2);
    let mut pairs: HashMap<(u32, u32), Border> = HashMap::new();
    let mut contacts = vec![0u32; reg.col.len()];
    let lab = &reg.lab;
    for y in 0..h {
        for x in 0..w {
            let j = y * w + x;
            let la = lab[j];
            if la != NONE && (x == 0 || y == 0 || x + 1 == w || y + 1 == h) {
                contacts[la as usize] += 1;
            }
            for (step, ok) in [(1usize, x + 1 < w), (w, y + 1 < h)] {
                if !ok {
                    continue;
                }
                let q = j + step;
                let lb = lab[q];
                if la == lb {
                    continue;
                }
                if la != NONE {
                    contacts[la as usize] += 1;
                }
                if lb != NONE {
                    contacts[lb as usize] += 1;
                }
                if la == NONE || lb == NONE {
                    continue;
                }
                // `a` is the lower id's side, walking away from the border.
                let (lo, hi, a0, b0, back) = if la < lb { (la, lb, j, q, true) } else { (lb, la, q, j, false) };
                let (pos, len) = if step == 1 { (x, w) } else { (y, h) };
                let far = 3 * k;
                // Pixel `s` steps out on side a (towards lower coordinates if back).
                let side = |start: usize, s: usize, towards_low: bool| -> Option<usize> {
                    let p0 = if start == j { pos } else { pos + 1 };
                    if towards_low {
                        (p0 >= s).then(|| start - s * step)
                    } else {
                        (p0 + s < len).then(|| start + s * step)
                    }
                };
                let e = pairs.entry((lo, hi)).or_default();
                e.n += 1;
                let (Some(ta), Some(tb)) = (side(a0, st, back), side(b0, st, !back)) else { continue };
                // Next to the artwork's own edge, the step says nothing about this border.
                if d[ta * 4 + 3] < 255 || d[tb * 4 + 3] < 255 {
                    e.n -= 1;
                    continue;
                }
                if l1(d, ta, tb) >= SHARP {
                    e.sharp += 1;
                    continue;
                }
                let pts = [side(a0, far, back), side(a0, k, back), side(b0, k, !back), side(b0, far, !back)];
                let [Some(a3), Some(a1), Some(b1), Some(b3)] = pts else { continue };
                if [a3, a1, b1, b3].iter().any(|&i| d[i * 4 + 3] < 255) {
                    continue;
                }
                let dir = sub(cf(p.pal[reg.col[hi as usize] as usize]), cf(p.pal[reg.col[lo as usize] as usize]));
                let len_dir = norm(dir);
                if len_dir == 0.0 {
                    continue;
                }
                let u = dir.map(|v| v / len_dir);
                let inner = dot(sub(rgb(d, b1), rgb(d, a1)), u);
                e.inner += inner;
                if let (Some(ah), Some(bh)) = (side(a0, k / 2, back), side(b0, k / 2, !back)) {
                    e.step += dot(sub(rgb(d, bh), rgb(d, ah)), u) - inner / 2.0;
                }
                e.outer[0] += dot(sub(rgb(d, a1), rgb(d, a3)), u);
                e.outer[1] += dot(sub(rgb(d, b3), rgb(d, b1)), u);
            }
        }
    }
    Borders { pairs, contacts }
}

fn cf(c: crate::Rgb) -> C3 {
    [c[0] as f64, c[1] as f64, c[2] as f64]
}

impl Border {
    /// `gap`: how far apart the two regions' palette colours are.
    fn is_ramp(&self, gap: f64) -> bool {
        let n = self.n as f64;
        self.n >= 8
            && (self.sharp as f64) < 0.2 * n
            // no step at the border, beyond 8-bit noise
            && self.step < 2.0 * n
            && (
                // a change clearly bigger than 8-bit noise, which keeps going
                (self.inner > 2.0 * n.sqrt() && self.outer[0].max(self.outer[1]) >= 0.5 * self.inner)
                // or hardly any change at all: an edge between two flat colours
                // changes by their whole difference, a band boundary on the
                // flat top of a ramp by next to nothing
                || self.inner.abs() < 0.35 * gap * n
            )
    }
}

fn gap(p: &Pal, reg: &Regions, a: u32, b: u32) -> f64 {
    norm(sub(cf(p.pal[reg.col[a as usize] as usize]), cf(p.pal[reg.col[b as usize] as usize])))
}

fn find(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        parent[x as usize] = parent[parent[x as usize] as usize];
        x = parent[x as usize];
    }
    x
}

// ---------- gradient models ----------

#[derive(Clone, Debug)]
enum Geo {
    Flat,
    /// x1 y1 x2 y2
    Linear([f64; 4]),
    /// cx cy r
    Radial([f64; 3]),
}

impl Geo {
    fn t(&self, x: f64, y: f64) -> f64 {
        match *self {
            Geo::Flat => 0.0,
            Geo::Linear([x1, y1, x2, y2]) => {
                let (dx, dy) = (x2 - x1, y2 - y1);
                (((x - x1) * dx + (y - y1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0)
            }
            Geo::Radial([cx, cy, r]) => (((x - cx).powi(2) + (y - cy).powi(2)).sqrt() / r).clamp(0.0, 1.0),
        }
    }
}

#[derive(Clone, Debug)]
struct Grad {
    geo: Geo,
    stops: Vec<(f64, C3)>,
}

fn interp(stops: &[(f64, C3)], t: f64) -> C3 {
    if t <= stops[0].0 {
        return stops[0].1;
    }
    for w in stops.windows(2) {
        let ((t0, c0), (t1, c1)) = (w[0], w[1]);
        if t <= t1 {
            let f = if t1 > t0 { (t - t0) / (t1 - t0) } else { 1.0 };
            return [c0[0] + f * (c1[0] - c0[0]), c0[1] + f * (c1[1] - c0[1]), c0[2] + f * (c1[2] - c0[2])];
        }
    }
    stops[stops.len() - 1].1
}

impl Grad {
    fn at(&self, x: f64, y: f64) -> C3 {
        interp(&self.stops, self.geo.t(x, y))
    }
}

/// A radial highlight over the gradient: `col` at opacity `a` in the centre,
/// fading linearly to nothing at radius `r`.
#[derive(Clone, Debug)]
struct Sheen {
    c: [f64; 3],
    col: C3,
    a: f64,
}

impl Sheen {
    fn alpha(&self, x: f64, y: f64) -> f64 {
        let d = ((x - self.c[0]).powi(2) + (y - self.c[1]).powi(2)).sqrt();
        self.a * (1.0 - d / self.c[2]).max(0.0)
    }
}

#[derive(Clone, Debug)]
struct Base {
    grad: Grad,
    sheen: Option<Sheen>,
}

impl Base {
    fn at(&self, x: f64, y: f64) -> C3 {
        let b = self.grad.at(x, y);
        match &self.sheen {
            None => b,
            Some(s) => {
                let a = s.alpha(x, y);
                [b[0] + a * (s.col[0] - b[0]), b[1] + a * (s.col[1] - b[1]), b[2] + a * (s.col[2] - b[2])]
            }
        }
    }
}

/// Pixels a model is fitted to: position and colour.
struct Samples {
    xy: Vec<(f64, f64)>,
    c: Vec<C3>,
}

/// Robust loss: mean colour error over the 70% of pixels the model fits best,
/// so glows and other details it isn't meant to explain don't decide between
/// models.
fn loss(s: &Samples, f: &dyn Fn(f64, f64) -> C3) -> f64 {
    let mut e: Vec<f64> = s.xy.iter().zip(&s.c).map(|(&(x, y), &c)| norm(sub(c, f(x, y)))).collect();
    let keep = (e.len() * 7 / 10).max(1);
    e.select_nth_unstable_by(keep - 1, |a, b| a.partial_cmp(b).unwrap());
    e[..keep].iter().sum::<f64>() / keep as f64
}

/// Which samples the next fit uses: those the last one explains.
fn inliers(s: &Samples, f: &dyn Fn(f64, f64) -> C3) -> Vec<bool> {
    let mut e: Vec<f64> = s.xy.iter().zip(&s.c).map(|(&(x, y), &c)| norm(sub(c, f(x, y)))).collect();
    let med = percentile(&mut e.clone(), 0.5);
    let thr = (3.0 * med).max(8.0);
    for v in e.iter_mut() {
        *v = if *v < thr { 1.0 } else { 0.0 };
    }
    e.into_iter().map(|v| v > 0.5).collect()
}

/// Stop colours for `n` stops spread evenly over [t0, 1], least squares on hat functions.
fn fit_stops(ts: &[f64], cs: &[C3], inl: &[bool], n: usize, t0: f64) -> Option<Vec<(f64, C3)>> {
    let offs: Vec<f64> = (0..n).map(|i| t0 + (1.0 - t0) * i as f64 / (n - 1) as f64).collect();
    let mut a = vec![vec![0.0; n]; n];
    let mut b = vec![[0.0; 3]; n];
    let mut wts = vec![0.0; n];
    for ((&t, c), _) in ts.iter().zip(cs).zip(inl).filter(|(_, &k)| k) {
        wts.iter_mut().for_each(|v| *v = 0.0);
        let t = t.clamp(offs[0], 1.0);
        let mut i = 0;
        while i + 2 < n && t > offs[i + 1] {
            i += 1;
        }
        let f = if offs[i + 1] > offs[i] { (t - offs[i]) / (offs[i + 1] - offs[i]) } else { 0.0 };
        wts[i] = 1.0 - f;
        wts[i + 1] = f;
        for p in i..=i + 1 {
            for q in i..=i + 1 {
                a[p][q] += wts[p] * wts[q];
            }
            for ch in 0..3 {
                b[p][ch] += wts[p] * c[ch];
            }
        }
    }
    // A little pull between neighbouring stops, so a stop no sample reaches
    // takes its neighbours' colour.
    for p in 0..n - 1 {
        a[p][p] += 1e-3;
        a[p + 1][p + 1] += 1e-3;
        a[p][p + 1] -= 1e-3;
        a[p + 1][p] -= 1e-3;
    }
    let mut cols = vec![[0.0; 3]; n];
    for ch in 0..3 {
        let x = solve(a.clone(), b.iter().map(|v| v[ch]).collect())?;
        for p in 0..n {
            cols[p][ch] = x[p];
        }
    }
    Some(offs.into_iter().zip(cols.into_iter().map(clamp3)).collect())
}

/// A gradient of `n` stops along the best direction, or around the best centre.
fn fit_grad(s: &Samples, inl: &[bool], n: usize, radial: bool) -> Option<Grad> {
    let pts = s.xy.iter().zip(inl).filter(|(_, &k)| k).map(|(p, _)| *p);
    let geo = if radial {
        let c = radial_centre(s, inl)?;
        let mut ds: Vec<f64> = pts.map(|(x, y)| ((x - c.0).powi(2) + (y - c.1).powi(2)).sqrt()).collect();
        let r = percentile(&mut ds, 0.995).max(1.0);
        Geo::Radial([c.0, c.1, r])
    } else {
        // Each channel as a plane c = a + bx + cy; the ramp runs along the
        // direction the three planes rise fastest in together.
        let mut m = vec![vec![0.0; 3]; 3];
        let mut rhs = [[0.0; 3]; 3];
        let (mut cx, mut cy, mut cnt) = (0.0, 0.0, 0.0);
        for ((&(x, y), c), _) in s.xy.iter().zip(&s.c).zip(inl).filter(|(_, &k)| k) {
            cx += x;
            cy += y;
            cnt += 1.0;
            let v = [1.0, x, y];
            for p in 0..3 {
                for q in 0..3 {
                    m[p][q] += v[p] * v[q];
                }
                for ch in 0..3 {
                    rhs[ch][p] += v[p] * c[ch];
                }
            }
        }
        if cnt < 10.0 {
            return None;
        }
        let (cx, cy) = (cx / cnt, cy / cnt);
        let (mut sxx, mut sxy, mut syy) = (0.0, 0.0, 0.0);
        for ch in 0..3 {
            let k = solve(m.clone(), rhs[ch].to_vec())?;
            sxx += k[1] * k[1];
            sxy += k[1] * k[2];
            syy += k[2] * k[2];
        }
        let th = 0.5 * (2.0 * sxy).atan2(sxx - syy);
        let (ux, uy) = (th.cos(), th.sin());
        let mut ts: Vec<f64> = pts.map(|(x, y)| x * ux + y * uy).collect();
        let lo = percentile(&mut ts, 0.005);
        let hi = percentile(&mut ts, 0.995);
        if hi - lo < 1.0 {
            return None;
        }
        let c0 = cx * ux + cy * uy;
        Geo::Linear([cx + (lo - c0) * ux, cy + (lo - c0) * uy, cx + (hi - c0) * ux, cy + (hi - c0) * uy])
    };
    let ts: Vec<f64> = s.xy.iter().map(|&(x, y)| geo.t(x, y)).collect();
    let t0 = if radial {
        let mut v: Vec<f64> = ts.iter().zip(inl).filter(|(_, &k)| k).map(|(t, _)| *t).collect();
        percentile(&mut v, 0.005).min(0.9)
    } else {
        0.0
    };
    let stops = fit_stops(&ts, &s.c, inl, n, t0)?;
    Some(Grad { geo, stops })
}

/// The centre a radial ramp fits best around: colour as a straight line of the
/// distance, searched on a coarse grid around the samples and refined.
fn radial_centre(s: &Samples, inl: &[bool]) -> Option<(f64, f64)> {
    let pick: Vec<usize> = (0..s.xy.len()).filter(|&i| inl[i]).collect();
    let step = (pick.len() / 3000).max(1);
    let pick: Vec<usize> = pick.into_iter().step_by(step).collect();
    if pick.len() < 10 {
        return None;
    }
    let (mut x0, mut x1, mut y0, mut y1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for &i in &pick {
        let (x, y) = s.xy[i];
        x0 = x0.min(x);
        x1 = x1.max(x);
        y0 = y0.min(y);
        y1 = y1.max(y);
    }
    let err = |cx: f64, cy: f64| {
        let (mut sd, mut sdd, mut n) = (0.0, 0.0, 0.0);
        let mut sc = [0.0; 3];
        let mut sdc = [0.0; 3];
        let mut scc = 0.0;
        for &i in &pick {
            let (x, y) = s.xy[i];
            let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
            let c = s.c[i];
            sd += d;
            sdd += d * d;
            n += 1.0;
            for ch in 0..3 {
                sc[ch] += c[ch];
                sdc[ch] += d * c[ch];
            }
            scc += dot(c, c);
        }
        // residual of the best line c = a + b·d, per channel, summed
        let det = n * sdd - sd * sd;
        if det <= 1e-9 {
            return f64::MAX;
        }
        let mut e = scc;
        for ch in 0..3 {
            let b = (n * sdc[ch] - sd * sc[ch]) / det;
            let a = (sc[ch] - b * sd) / n;
            e -= a * sc[ch] + b * sdc[ch];
        }
        e
    };
    let (sw, sh) = (x1 - x0, y1 - y0);
    let (mut bx, mut by, mut be) = (0.0, 0.0, f64::MAX);
    for gy in 0..9 {
        for gx in 0..9 {
            let cx = x0 - 0.5 * sw + 2.0 * sw * gx as f64 / 8.0;
            let cy = y0 - 0.5 * sh + 2.0 * sh * gy as f64 / 8.0;
            let e = err(cx, cy);
            if e < be {
                (bx, by, be) = (cx, cy, e);
            }
        }
    }
    let (mut dx, mut dy) = (sw / 4.0, sh / 4.0);
    for _ in 0..5 {
        let (cx0, cy0) = (bx, by);
        for gy in -2..=2 {
            for gx in -2..=2 {
                let (cx, cy) = (cx0 + dx * gx as f64 / 2.0, cy0 + dy * gy as f64 / 2.0);
                let e = err(cx, cy);
                if e < be {
                    (bx, by, be) = (cx, cy, e);
                }
            }
        }
        dx /= 2.5;
        dy /= 2.5;
    }
    Some((bx, by))
}

/// A two-stop gradient along `geo` with a sheen over it, fitted together. With
/// the sheen's centre, radius and opacity fixed, the stop colours and the sheen
/// colour are one linear least squares per channel:
/// obs = (c0·(1−u) + c1·u)·(1 − a·w) + q·w, with q = a·S and w the sheen's falloff.
/// The centre, radius and opacity are searched on a grid, then refined.
fn fit_sheen(s: &Samples, inl: &[bool], geo: &Geo) -> Option<Base> {
    let pick: Vec<usize> = (0..s.xy.len()).filter(|&i| inl[i]).collect();
    let step = (pick.len() / 3000).max(1);
    let pick: Vec<usize> = pick.into_iter().step_by(step).collect();
    if pick.len() < 20 {
        return None;
    }
    let t0 = match geo {
        Geo::Radial(_) => {
            let mut v: Vec<f64> = pick.iter().map(|&i| geo.t(s.xy[i].0, s.xy[i].1)).collect();
            percentile(&mut v, 0.005).min(0.9)
        }
        _ => 0.0,
    };
    let us: Vec<f64> = pick.iter().map(|&i| ((geo.t(s.xy[i].0, s.xy[i].1) - t0) / (1.0 - t0)).clamp(0.0, 1.0)).collect();
    let (mut x0, mut x1, mut y0, mut y1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for &i in &pick {
        let (x, y) = s.xy[i];
        x0 = x0.min(x);
        x1 = x1.max(x);
        y0 = y0.min(y);
        y1 = y1.max(y);
    }
    let diag = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
    // (stops, sheen colour, error) for one sheen placement
    let fit = |cx: f64, cy: f64, r: f64, a: f64| -> Option<([C3; 2], C3, f64)> {
        let mut m = vec![vec![0.0; 3]; 3];
        let mut b = [[0.0; 3]; 3];
        let rows: Vec<[f64; 3]> = pick
            .iter()
            .zip(&us)
            .map(|(&i, &u)| {
                let (x, y) = s.xy[i];
                let w = (1.0 - ((x - cx).powi(2) + (y - cy).powi(2)).sqrt() / r).max(0.0);
                [(1.0 - u) * (1.0 - a * w), u * (1.0 - a * w), w]
            })
            .collect();
        for (k, row) in rows.iter().enumerate() {
            let c = s.c[pick[k]];
            for p in 0..3 {
                for q in 0..3 {
                    m[p][q] += row[p] * row[q];
                }
                for ch in 0..3 {
                    b[ch][p] += row[p] * c[ch];
                }
            }
        }
        let mut z = [[0.0; 3]; 3];
        for ch in 0..3 {
            let x = solve(m.clone(), b[ch].to_vec())?;
            for p in 0..3 {
                z[p][ch] = x[p];
            }
        }
        let (c0, c1, col) = (clamp3(z[0]), clamp3(z[1]), clamp3(z[2].map(|v| v / a)));
        let e: f64 = rows
            .iter()
            .enumerate()
            .map(|(k, row)| {
                let w = row[2];
                let u = if w * a < 1.0 { row[1] / (1.0 - a * w) } else { 0.0 };
                let p = [0, 1, 2].map(|ch| (c0[ch] * (1.0 - u) + c1[ch] * u) * (1.0 - a * w) + col[ch] * a * w);
                norm(sub(s.c[pick[k]], p)).min(30.0)
            })
            .sum();
        Some(([c0, c1], col, e))
    };
    let mut best: Option<([f64; 4], f64)> = None;
    for gy in 0..7 {
        for gx in 0..7 {
            for rf in [0.2, 0.3, 0.45, 0.65, 0.9, 1.2] {
                for a in [0.2, 0.45, 0.75] {
                    let c = [x0 + (x1 - x0) * gx as f64 / 6.0, y0 + (y1 - y0) * gy as f64 / 6.0, rf * diag, a];
                    if let Some((_, _, e)) = fit(c[0], c[1], c[2], c[3]) {
                        if best.is_none_or(|b| e < b.1) {
                            best = Some((c, e));
                        }
                    }
                }
            }
        }
    }
    let (mut c, mut be) = best?;
    let mut sp = [(x1 - x0) / 6.0, (y1 - y0) / 6.0, 0.15 * diag, 0.15];
    for _ in 0..6 {
        for k in 0..4 {
            for dir in [-1.0, -0.5, 0.5, 1.0] {
                let mut t = c;
                t[k] += dir * sp[k];
                if t[2] <= 1.0 || !(0.02..=1.0).contains(&t[3]) {
                    continue;
                }
                if let Some((_, _, e)) = fit(t[0], t[1], t[2], t[3]) {
                    if e < be {
                        (c, be) = (t, e);
                    }
                }
            }
        }
        sp = sp.map(|v| v / 2.0);
    }
    let (stops, col, _) = fit(c[0], c[1], c[2], c[3])?;
    Some(Base { grad: Grad { geo: geo.clone(), stops: vec![(t0, stops[0]), (1.0, stops[1])] }, sheen: Some(Sheen { c: [c[0], c[1], c[2]], col, a: c[3] }) })
}

/// Fit a gradient robustly: refit on the pixels the last fit explains.
fn robust(s: &Samples, n: usize, radial: bool, sheen: bool) -> Option<Base> {
    let mut inl = vec![true; s.xy.len()];
    let mut base: Option<Base> = None;
    let mut cs = Samples { xy: s.xy.clone(), c: s.c.clone() };
    for _ in 0..3 {
        let grad = fit_grad(&cs, &inl, n, radial)?;
        let b = if sheen { fit_sheen(s, &inl, &grad.geo)? } else { Base { grad, sheen: None } };
        inl = inliers(s, &|x, y| b.at(x, y));
        // Take the sheen out of the colours before refitting the gradient under it.
        if let Some(sh) = &b.sheen {
            for (i, &(x, y)) in s.xy.iter().enumerate() {
                let a = sh.alpha(x, y).min(0.95);
                cs.c[i] = [0, 1, 2].map(|ch| (s.c[i][ch] - a * sh.col[ch]) / (1.0 - a));
            }
        }
        base = Some(b);
    }
    base
}

fn flat(s: &Samples) -> Base {
    let mut c = [0.0; 3];
    for ch in 0..3 {
        let mut v: Vec<f64> = s.c.iter().map(|c| c[ch]).collect();
        c[ch] = percentile(&mut v, 0.5);
    }
    Base { grad: Grad { geo: Geo::Flat, stops: vec![(0.0, c)] }, sheen: None }
}

/// The simplest model that is nearly as good as the best one.
fn best_base(s: &Samples) -> (Base, f64) {
    let mut cands: Vec<(f64, Base)> = vec![(0.0, flat(s))];
    for (n, cost) in [(2, 1.0), (3, 1.5), (4, 2.0)] {
        if let Some(b) = robust(s, n, false, false) {
            cands.push((cost, b));
        }
        if let Some(b) = robust(s, n, true, false) {
            cands.push((cost + 0.2, b));
        }
    }
    if let Some(b) = robust(s, 2, false, true) {
        cands.push((2.5, b));
    }
    let scored: Vec<(f64, f64, Base)> = cands.into_iter().map(|(cost, b)| (cost, loss(s, &|x, y| b.at(x, y)), b)).collect();
    for (cost, l, b) in &scored {
        dbg_line!("    model cost {cost}: loss {l:.2} {}{}", match b.grad.geo { Geo::Flat => "flat", Geo::Linear(_) => "linear", Geo::Radial(_) => "radial" }, if b.sheen.is_some() { " + sheen" } else { "" });
    }
    let best = scored.iter().map(|c| c.1).fold(f64::MAX, f64::min);
    let mut ok: Vec<&(f64, f64, Base)> = scored.iter().filter(|c| c.1 <= best * 1.1 + 0.3).collect();
    ok.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    (ok[0].2.clone(), ok[0].1)
}

// ---------- blur ----------

/// Gaussian blur of a grid with zero outside, approximated by three box blurs.
fn blur(src: &[f32], w: usize, h: usize, sigma: f64) -> Vec<f32> {
    let n = 3.0;
    let wi = ((12.0 * sigma * sigma / n + 1.0).sqrt()).floor();
    let wl = if (wi as i64) % 2 == 0 { wi - 1.0 } else { wi };
    let wu = wl + 2.0;
    let m = ((12.0 * sigma * sigma - n * wl * wl - 4.0 * n * wl - 3.0 * n) / (-4.0 * wl - 4.0)).round();
    let mut a = src.to_vec();
    let mut b = vec![0f32; a.len()];
    for i in 0..3 {
        let size = if (i as f64) < m { wl } else { wu };
        let r = ((size - 1.0) / 2.0).max(0.0) as usize;
        if r == 0 {
            continue;
        }
        box_pass(&a, &mut b, w, h, r, true);
        box_pass(&b, &mut a, w, h, r, false);
    }
    a
}

/// A blurred patch of a grid: `v` is `w`×`h` cells placed at (`x`, `y`).
struct Crop {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    v: Vec<f32>,
}

/// Blur only around what is set in `m`: as far as the blur reaches.
fn blur_crop(m: &[f32], gw: usize, gh: usize, sigma: f64) -> Crop {
    let (mut x0, mut x1, mut y0, mut y1) = (gw, 0, gh, 0);
    for (k, &v) in m.iter().enumerate() {
        if v != 0.0 {
            let (x, y) = (k % gw, k / gw);
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
    }
    if x1 < x0 {
        return Crop { x: 0, y: 0, w: 0, h: 0, v: vec![] };
    }
    let pad = (3.5 * sigma).ceil() as usize + 2;
    let (x0, y0, x1, y1) = (x0.saturating_sub(pad), y0.saturating_sub(pad), (x1 + pad).min(gw - 1), (y1 + pad).min(gh - 1));
    let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
    let mut v = vec![0f32; w * h];
    for y in 0..h {
        v[y * w..(y + 1) * w].copy_from_slice(&m[(y0 + y) * gw + x0..(y0 + y) * gw + x0 + w]);
    }
    Crop { x: x0, y: y0, w, h, v: blur(&v, w, h, sigma) }
}

fn box_pass(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize, horiz: bool) {
    let (lines, len) = if horiz { (h, w) } else { (w, h) };
    let at = |l: usize, i: usize| if horiz { l * w + i } else { i * w + l };
    let norm = 1.0 / (2 * r + 1) as f32;
    for l in 0..lines {
        let mut acc = 0f32;
        for i in 0..=r.min(len - 1) {
            acc += src[at(l, i)];
        }
        for i in 0..len {
            dst[at(l, i)] = acc * norm;
            if i + r + 1 < len {
                acc += src[at(l, i + r + 1)];
            }
            if i >= r {
                acc -= src[at(l, i - r)];
            }
        }
    }
}

// ---------- areas ----------

/// A soft glow inside an area: `mask` blurred by `sigma`, in colour `col` at opacity `a`.
struct Glow {
    mask: Vec<u8>,
    col: C3,
    a: f64,
    sigma: f64,
}

struct Area {
    regions: Vec<u32>,
    base: Base,
    glows: Vec<Glow>,
    /// Regions inside the area that its glows run under.
    enclosed: Vec<u32>,
}

/// A glow's colour change over the base, on its own grid of cells.
type GlowPred = (Bbox, Vec<[f32; 3]>);

struct Bbox {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
}

/// Fit one area: base gradient, glows, then the check against its bands.
#[allow(clippy::too_many_arguments)]
/// Err holds the regions to try again with, without those the fit does worse
/// on than their bands (empty: give up).
fn fit_area(r: &Image, p: &Pal, idx: &[i16], reg: &Regions, member: &[bool], hidden: &[bool], bb: &Bbox, enclosed: Vec<u32>, regions: Vec<u32>) -> Result<Area, Vec<u32>> {
    let (w, h, d) = (r.w, r.h, r.data);
    let long = w.max(h) as f64;
    let st = (js_round(long / 2048.0) as usize).max(1);
    let (bw, bh) = (bb.x1 - bb.x0 + 1, bb.y1 - bb.y0 + 1);
    let in_area = |j: usize| reg.lab[j] != NONE && member[reg.lab[j] as usize];
    let total: usize = regions.iter().map(|&g| reg.n[g as usize]).sum();
    let stride = ((total as f64 / 60000.0).sqrt().ceil() as usize).max(1);
    // Fit on solid pixels: anti-aliased edge pixels belong to neither side.
    let solid = |x: usize, y: usize| {
        let j = y * w + x;
        d[j * 4 + 3] == 255
            && (x < st || l1(d, j, j - st) < SHARP)
            && (x + st >= w || l1(d, j, j + st) < SHARP)
            && (y < st || l1(d, j, j - st * w) < SHARP)
            && (y + st >= h || l1(d, j, j + st * w) < SHARP)
    };
    let mut s = Samples { xy: vec![], c: vec![] };
    for y in (bb.y0..=bb.y1).step_by(stride) {
        for x in (bb.x0..=bb.x1).step_by(stride) {
            let j = y * w + x;
            if in_area(j) && solid(x, y) {
                s.xy.push((x as f64 + 0.5, y as f64 + 0.5));
                s.c.push(rgb(d, j));
            }
        }
    }
    if s.xy.len() < 50 {
        return Err(vec![]);
    }
    let ds = (js_round(long / 256.0) as usize).max(1);
    // Glows: what the base leaves over, in blobs. Kinds: 0 outside, 1 solid
    // area pixel, 2 enclosed region (hidden under a sharp shape), 3 area pixel
    // on a sharp edge (part of a glow's shape, but its colour is a blend with
    // the shape beside it, so the fit doesn't go by it).
    let mut kind = vec![0u8; bw * bh];
    for y in 0..bh {
        for x in 0..bw {
            let j = (bb.y0 + y) * w + bb.x0 + x;
            let c = y * bw + x;
            if in_area(j) {
                if d[j * 4 + 3] == 255 {
                    kind[c] = if solid(bb.x0 + x, bb.y0 + y) { 1 } else { 3 };
                }
            } else if reg.lab[j] != NONE && hidden[reg.lab[j] as usize] {
                kind[c] = 2;
            }
        }
    }
    // `quick`: a rough pass, only to see where the glows are.
    let find_glows = |base: &Base, fit_loss: f64, quick: bool| -> (Vec<Glow>, Vec<GlowPred>) {
        let mut res = vec![[0f32; 3]; bw * bh];
        for y in 0..bh {
            for x in 0..bw {
                let c = y * bw + x;
                if kind[c] == 1 || kind[c] == 3 {
                    let j = (bb.y0 + y) * w + bb.x0 + x;
                    let e = sub(rgb(d, j), base.at((bb.x0 + x) as f64 + 0.5, (bb.y0 + y) as f64 + 0.5));
                    res[c] = e.map(|v| v as f32);
                }
            }
        }
        let tg = (3.0 * fit_loss).max(10.0) as f32;
        let mag = |c: usize| (res[c][0].powi(2) + res[c][1].powi(2) + res[c][2].powi(2)).sqrt();
        let blob: Vec<bool> = (0..bw * bh).map(|c| (kind[c] == 1 || kind[c] == 3) && mag(c) > tg).collect();
        // An enclosed region joins the glows when they surround most of it.
        let mut touch: HashMap<u32, (u32, u32)> = HashMap::new();
        for y in 0..bh {
            for x in 0..bw {
                let c = y * bw + x;
                if kind[c] != 2 {
                    continue;
                }
                let g = reg.lab[(bb.y0 + y) * w + bb.x0 + x];
                for (ok, k) in [(x > 0, c.wrapping_sub(1)), (x + 1 < bw, c + 1), (y > 0, c.wrapping_sub(bw)), (y + 1 < bh, c + bw)] {
                    if ok && (kind[k] == 1 || kind[k] == 3) {
                        let e = touch.entry(g).or_default();
                        e.1 += 1;
                        if blob[k] {
                            e.0 += 1;
                        }
                    }
                }
            }
        }
        let joins = |c: usize| {
            let g = reg.lab[(bb.y0 + c / bw) * w + bb.x0 + c % bw];
            touch.get(&g).is_some_and(|&(b, t)| b * 2 >= t)
        };
        let cand: Vec<bool> = (0..bw * bh).map(|c| blob[c] || (kind[c] == 2 && joins(c))).collect();
        // 8-connected components of the candidates
        let mut comp = vec![NONE; bw * bh];
        let mut comps: Vec<(Bbox, usize)> = vec![];
        let mut stack = vec![];
        for c0 in 0..bw * bh {
            if !cand[c0] || comp[c0] != NONE {
                continue;
            }
            let id = comps.len() as u32;
            comp[c0] = id;
            stack.push(c0);
            let mut b = Bbox { x0: usize::MAX, y0: usize::MAX, x1: 0, y1: 0 };
            let mut nb = 0;
            while let Some(c) = stack.pop() {
                let (x, y) = (c % bw, c / bw);
                b.x0 = b.x0.min(x);
                b.x1 = b.x1.max(x);
                b.y0 = b.y0.min(y);
                b.y1 = b.y1.max(y);
                if blob[c] {
                    nb += 1;
                }
                for dy in -1i64..=1 {
                    for dx in -1i64..=1 {
                        let (xx, yy) = (x as i64 + dx, y as i64 + dy);
                        if xx < 0 || yy < 0 || xx >= bw as i64 || yy >= bh as i64 {
                            continue;
                        }
                        let k = yy as usize * bw + xx as usize;
                        if cand[k] && comp[k] == NONE {
                            comp[k] = id;
                            stack.push(k);
                        }
                    }
                }
            }
            comps.push((b, nb));
        }
        let min_blob = ((long / 256.0).powi(2)) as usize;
        let mut glows = vec![];
        // Each glow's prediction on its own grid, for the check below.
        let mut preds: Vec<GlowPred> = vec![];
        let one = |id: usize, nb: usize| -> Option<(GlowPred, Vec<Glow>)> {
            if nb < min_blob.max(8) {
                return None;
            }
            let id = id as u32;
            let (cb, nb) = (&comps[id as usize].0, nb);
            let mut v = [0.0; 3];
            for c in 0..bw * bh {
                if comp[c] == id && blob[c] {
                    for ch in 0..3 {
                        v[ch] += res[c][ch] as f64;
                    }
                }
            }
            let lv = norm(v);
            if lv == 0.0 {
                return None;
            }
            let v = v.map(|x| x / lv);
            // Room for the glow's tail past the blob, within reason.
            let m = ((cb.x1 - cb.x0).max(cb.y1 - cb.y0) / 2).min((long / 8.0) as usize) + 4 * ds;
            let g = Bbox { x0: cb.x0.saturating_sub(m), y0: cb.y0.saturating_sub(m), x1: (cb.x1 + m).min(bw - 1), y1: (cb.y1 + m).min(bh - 1) };
            let (gw, gh) = ((g.x1 - g.x0 + 1).div_ceil(ds), (g.y1 - g.y0 + 1).div_ceil(ds));
            // Cells: mean residual over the pixels the glow is judged on (solid,
            // not another glow's), the base at the cell, and how much of the
            // cell this glow may cover.
            let mut rsum = vec![[0f64; 3]; gw * gh];
            let mut ev = vec![0u32; gw * gh];
            let mut own = vec![0u32; gw * gh];
            let mut hid = vec![0u32; gw * gh];
            let mut cnt = vec![0u32; gw * gh];
            // The shapes it runs under. With several (a row of bulbs), each gets
            // a glow of its own, and they overlap as separate glows do: brighter
            // where they meet than one merged shape could be.
            let mut seeds: Vec<u32> = vec![];
            let mut seed_of = vec![u16::MAX; gw * gh];
            for y in g.y0..=g.y1 {
                for x in g.x0..=g.x1 {
                    let c = y * bw + x;
                    let cell = ((y - g.y0) / ds) * gw + (x - g.x0) / ds;
                    cnt[cell] += 1;
                    if kind[c] == 1 && (comp[c] == NONE || comp[c] == id) {
                        ev[cell] += 1;
                        for ch in 0..3 {
                            rsum[cell][ch] += res[c][ch] as f64;
                        }
                    }
                    if comp[c] == id {
                        if kind[c] == 2 {
                            hid[cell] += 1;
                            let rg = reg.lab[(bb.y0 + y) * w + bb.x0 + x];
                            let si = seeds.iter().position(|&q| q == rg).unwrap_or_else(|| {
                                seeds.push(rg);
                                seeds.len() - 1
                            });
                            seed_of[cell] = si as u16;
                        } else {
                            own[cell] += 1;
                        }
                    }
                }
            }
            if seeds.len() > 64 {
                seeds.clear();
            }
            let ns = seeds.len().max(1);
            // Every cell belongs to its nearest shape (breadth first over the grid).
            if seeds.len() > 1 {
                let mut q: std::collections::VecDeque<usize> = (0..gw * gh).filter(|&k| seed_of[k] != u16::MAX).collect();
                while let Some(k) = q.pop_front() {
                    let (x, y) = (k % gw, k / gw);
                    for (ok, n) in [(x > 0, k.wrapping_sub(1)), (x + 1 < gw, k + 1), (y > 0, k.wrapping_sub(gw)), (y + 1 < gh, k + gw)] {
                        if ok && seed_of[n] == u16::MAX {
                            seed_of[n] = seed_of[k];
                            q.push_back(n);
                        }
                    }
                }
            }
            let seed_at = |k: usize| if seeds.len() > 1 { seed_of[k] as usize } else { 0 };
            let rbar: Vec<C3> = (0..gw * gh).map(|k| if ev[k] > 0 { rsum[k].map(|v| v / ev[k] as f64) } else { [0.0; 3] }).collect();
            let bcell: Vec<C3> = (0..gw * gh)
                .map(|k| base.at((bb.x0 + g.x0 + (k % gw) * ds) as f64 + ds as f64 / 2.0, (bb.y0 + g.y0 + (k / gw) * ds) as f64 + ds as f64 / 2.0))
                .collect();
            let judged: Vec<bool> = (0..gw * gh).map(|k| ev[k] * 2 >= cnt[k].max(1)).collect();
            let sbar: Vec<f64> = rbar.iter().map(|r| dot(*r, v)).collect();
            let smax = (0..gw * gh).filter(|&k| own[k] > 0 && judged[k]).map(|k| sbar[k]).fold(0.0, f64::max);
            if smax <= tg as f64 {
                return None;
            }
            // The glow's shape on the cell grid: cells marked inside, the shapes
            // it runs under, and the unjudged rim between them; one per seed.
            let shape = |inside: &dyn Fn(usize) -> bool| -> Vec<Vec<f32>> {
                let mut m: Vec<f32> = (0..gw * gh).map(|k| ((own[k] > 0 && judged[k] && inside(k)) || hid[k] * 2 >= cnt[k].max(1)) as u8 as f32).collect();
                let rim: Vec<usize> = (0..gw * gh)
                    .filter(|&k| m[k] == 0.0 && own[k] > 0 && !judged[k])
                    .filter(|&k| {
                        let (x, y) = (k % gw, k / gw);
                        (x > 0 && m[k - 1] > 0.0) || (x + 1 < gw && m[k + 1] > 0.0) || (y > 0 && m[k - gw] > 0.0) || (y + 1 < gh && m[k + gw] > 0.0)
                    })
                    .collect();
                for k in rim {
                    m[k] = 1.0;
                }
                (0..ns).map(|si| (0..gw * gh).map(|k| if seed_at(k) == si { m[k] } else { 0.0 }).collect()).collect()
            };
            // A glow is colour G at opacity a over the base. With the blurred
            // shapes m_i, the original is covered by 1 − Π(1 − a·m_i), so
            // res = cover·(G − B). The base changes too little under one glow to
            // tell a from G reliably, so a is tried at a few values; for each,
            // every channel of G is its own least squares, and clamping it to
            // the colour range keeps it optimal.
            let fit = |ms: &[Vec<f32>], sg: f64| -> Option<(C3, f64, f64, Vec<f32>)> {
                let prs: Vec<Crop> = ms.iter().map(|m| blur_crop(m, gw, gh, sg)).collect();
                let cells: Vec<usize> = (0..gw * gh).filter(|&k| judged[k]).collect();
                let mut best: Option<(C3, f64, f64, Vec<f32>)> = None;
                for a in [0.25, 0.4, 0.55, 0.7, 0.85, 1.0] {
                    let mut cover = vec![1f32; gw * gh];
                    for pr in &prs {
                        for yy in 0..pr.h {
                            for xx in 0..pr.w {
                                cover[(pr.y + yy) * gw + pr.x + xx] *= 1.0 - a as f32 * pr.v[yy * pr.w + xx];
                            }
                        }
                    }
                    cover.iter_mut().for_each(|c| *c = 1.0 - *c);
                    let mut col = [0.0; 3];
                    for ch in 0..3 {
                        let (mut num_, mut den) = (0.0, 0.0);
                        for &k in &cells {
                            let mk = cover[k] as f64;
                            num_ += mk * (rbar[k][ch] + mk * bcell[k][ch]);
                            den += mk * mk;
                        }
                        if den == 0.0 {
                            return None;
                        }
                        col[ch] = (num_ / den).clamp(0.0, 255.0);
                    }
                    let e: f64 = cells
                        .iter()
                        .map(|&k| {
                            let mk = cover[k] as f64;
                            (0..3).map(|ch| (rbar[k][ch] - mk * (col[ch] - bcell[k][ch])).powi(2)).sum::<f64>()
                        })
                        .sum();
                    if best.as_ref().is_none_or(|b| e < b.2) {
                        best = Some((col, a, e, cover));
                    }
                }
                best
            };
            let none: f64 = (0..gw * gh).filter(|&k| judged[k]).map(|k| dot(rbar[k], rbar[k])).sum();
            let mut sigmas = vec![];
            let mut sg = 0.6;
            while sg <= (gw.max(gh) as f64 / 4.0).max(4.0) {
                sigmas.push(sg);
                sg *= 1.4;
            }
            // First shape: the residual along its mean direction past a threshold.
            let mut best: Option<(C3, f64, f64, Vec<f32>, f64)> = None;
            let starts: &[f64] = if quick { &[0.6, 1.0, 1.6] } else { &[0.4, 0.6, 0.8, 1.0, 1.25, 1.6, 2.0] };
            for &af in starts {
                let m = shape(&|k| sbar[k] >= smax * af / 2.0);
                for &sg in &sigmas {
                    if let Some((col, a, e, cover)) = fit(&m, sg) {
                        if best.as_ref().is_none_or(|b| e < b.2) {
                            best = Some((col, a, e, cover, sg));
                        }
                    }
                }
            }
            let mut bst = best?;
            // Then the shape from the fit itself: where the original is at least
            // half covered by the glow's colour. Refit the blur around the last one.
            let cover_of = |col: C3, a: f64, r: C3, b: C3| {
                let dg = sub(col, b);
                let l = dot(dg, dg) * a;
                if l > 0.0 {
                    dot(r, dg) / l
                } else {
                    0.0
                }
            };
            let mut cut = 0.5;
            let (rounds, cuts, steps): (usize, &[f64], &[f64]) =
                if quick { (1, &[0.5], &[0.8, 1.0, 1.25]) } else { (2, &[0.3, 0.45, 0.6], &[0.75, 0.87, 0.94, 1.0, 1.07, 1.15, 1.3]) };
            for _ in 0..rounds {
                let (col, a, s0) = (bst.0, bst.1, bst.4);
                for &c in cuts {
                    let m = shape(&|k| cover_of(col, a, rbar[k], bcell[k]) >= c);
                    for &sf in steps {
                        if let Some((col, a, e, cover)) = fit(&m, s0 * sf) {
                            if e < bst.2 {
                                bst = (col, a, e, cover, s0 * sf);
                                cut = c;
                            }
                        }
                    }
                }
            }
            let covered = |col: C3, a: f64, r: C3, b: C3| cover_of(col, a, r, b) >= cut;
            let (col, a, be, cover, bs) = bst;
            dbg_line!("    glow {id}: blob {nb} px, bbox {}x{}, {ns} shapes → {} at {a:.2}, σ {:.1} px, error {:.2}", cb.x1 - cb.x0 + 1, cb.y1 - cb.y0 + 1, hex(col), bs * ds as f64, be / none);
            // A glow explains its blob well and is a part of the area, not most
            // of it: a leftover that big means the gradient under it is wrong.
            if be > 0.2 * none || nb * 3 > total {
                return None;
            }
            // Each glow's own shape at full size.
            let mut masks = vec![vec![0u8; 0]; ns];
            for y in g.y0..=g.y1 {
                for x in g.x0..=g.x1 {
                    let c = y * bw + x;
                    if comp[c] != id {
                        continue;
                    }
                    let inside = kind[c] == 2 || {
                        let bx = base.at((bb.x0 + x) as f64 + 0.5, (bb.y0 + y) as f64 + 0.5);
                        covered(col, a, res[c].map(|v| v as f64), bx)
                    };
                    if inside {
                        let cell = ((y - g.y0) / ds) * gw + (x - g.x0) / ds;
                        let si = if kind[c] == 2 && seeds.len() > 1 {
                            let rg = reg.lab[(bb.y0 + y) * w + bb.x0 + x];
                            seeds.iter().position(|&q| q == rg).unwrap_or(0)
                        } else {
                            seed_at(cell)
                        };
                        if masks[si].is_empty() {
                            masks[si] = vec![0u8; w * h];
                        }
                        masks[si][(bb.y0 + y) * w + bb.x0 + x] = 1;
                    }
                }
            }
            let pred = (g, (0..gw * gh).map(|k| sub(col, bcell[k]).map(|v| (v * cover[k] as f64) as f32)).collect());
            Some((pred, masks.into_iter().filter(|m| !m.is_empty()).map(|mask| Glow { mask, col, a, sigma: bs * ds as f64 }).collect::<Vec<_>>()))
        };
        #[cfg(feature = "parallel")]
        let found: Vec<_> = {
            use rayon::prelude::*;
            comps.par_iter().enumerate().map(|(id, c)| one(id, c.1)).collect()
        };
        #[cfg(not(feature = "parallel"))]
        let found: Vec<_> = comps.iter().enumerate().map(|(id, c)| one(id, c.1)).collect();
        for (pred, gs) in found.into_iter().flatten() {
            preds.push(pred);
            glows.extend(gs);
        }
        (glows, preds)
    };
    #[cfg(not(target_arch = "wasm32"))]
    let t0 = std::time::Instant::now();
    #[cfg(not(target_arch = "wasm32"))]
    let lap = |what: &str| dbg_line!("    {what}: {} ms", t0.elapsed().as_millis());
    #[cfg(target_arch = "wasm32")]
    let lap = |_: &str| {};
    let (base, fit_loss) = best_base(&s);
    lap("base");
    let (_, preds) = find_glows(&base, fit_loss, true);
    lap("glows");
    // Refit the gradient on the pixels no glow reaches, then find the glows
    // again over that cleaner base.
    let glowing = |x: f64, y: f64| {
        let (x, y) = (x as usize - bb.x0, y as usize - bb.y0);
        preds.iter().any(|(g, pr)| {
            x >= g.x0 && x <= g.x1 && y >= g.y0 && y <= g.y1 && {
                let gw = (g.x1 - g.x0 + 1).div_ceil(ds);
                let v = pr[((y - g.y0) / ds) * gw + (x - g.x0) / ds];
                v[0].abs() + v[1].abs() + v[2].abs() > 3.0
            }
        })
    };
    let keep: Vec<usize> = (0..s.xy.len()).filter(|&i| !glowing(s.xy[i].0, s.xy[i].1)).collect();
    let (base, fit_loss) = if !preds.is_empty() && keep.len() >= 50 {
        best_base(&Samples { xy: keep.iter().map(|&i| s.xy[i]).collect(), c: keep.iter().map(|&i| s.c[i]).collect() })
    } else {
        (base, fit_loss)
    };
    lap("base again");
    let (glows, preds) = find_glows(&base, fit_loss, false);
    lap("glows again");
    // Keep the gradient only where it is closer to the original than the bands:
    // over the whole area, and region by region.
    let (mut em, mut eb, mut n) = (0.0, 0.0, 0f64);
    let mut per: HashMap<u32, (f64, f64)> = HashMap::new();
    for y in (0..bh).step_by(stride) {
        for x in (0..bw).step_by(stride) {
            let c = y * bw + x;
            if kind[c] != 1 {
                continue;
            }
            let j = (bb.y0 + y) * w + bb.x0 + x;
            let o = rgb(d, j);
            let mut pm = base.at((bb.x0 + x) as f64 + 0.5, (bb.y0 + y) as f64 + 0.5);
            for (g, pr) in &preds {
                if x >= g.x0 && x <= g.x1 && y >= g.y0 && y <= g.y1 {
                    let gw = (g.x1 - g.x0 + 1).div_ceil(ds);
                    let v = pr[((y - g.y0) / ds) * gw + (x - g.x0) / ds];
                    for ch in 0..3 {
                        pm[ch] += v[ch] as f64;
                    }
                }
            }
            let (e1, e2) = (norm(sub(o, clamp3(pm))), norm(sub(o, cf(p.pal[idx[j] as usize]))));
            em += e1;
            eb += e2;
            n += 1.0;
            let t = per.entry(reg.lab[j]).or_default();
            t.0 += e1;
            t.1 += e2;
        }
    }
    dbg_line!("    check: gradient {:.2} against bands {:.2} per pixel", em / n.max(1.0), eb / n.max(1.0));
    // Regions noticeably worse than their bands (by 2 per pixel) go back to them.
    let worse: Vec<u32> = regions.iter().copied().filter(|g| per.get(g).is_some_and(|&(e1, e2)| e1 > e2 + 2.0 * reg.n[*g as usize] as f64 / (stride * stride) as f64)).collect();
    if n == 0.0 || em >= eb || !worse.is_empty() {
        return Err(regions.into_iter().filter(|g| !worse.contains(g)).collect());
    }
    Ok(Area { regions, base, glows, enclosed })
}

// ---------- halo ----------

/// A wide, evenly fading halo around the artwork: `mask` in `col` at opacity
/// `a`, blurred by `sigma`, kept out of the holes in the artwork by `clip`.
struct Halo {
    mask: Vec<u8>,
    clip: Option<Vec<u8>>,
    /// The clip's edge lies this far under the artwork, so it can be traced
    /// this coarsely.
    clip_f: usize,
    col: C3,
    a: f64,
    sigma: f64,
}

/// Chamfer distance (in pixels, 3-4 weights) from the pixels where `seed` is set.
fn distance(seed: &[bool], w: usize, h: usize) -> Vec<f32> {
    let big = 1e9f32;
    let mut d: Vec<f32> = seed.iter().map(|&s| if s { 0.0 } else { big }).collect();
    for y in 0..h {
        for x in 0..w {
            let j = y * w + x;
            let mut v = d[j];
            if x > 0 {
                v = v.min(d[j - 1] + 3.0);
            }
            if y > 0 {
                v = v.min(d[j - w] + 3.0);
                if x > 0 {
                    v = v.min(d[j - w - 1] + 4.0);
                }
                if x + 1 < w {
                    v = v.min(d[j - w + 1] + 4.0);
                }
            }
            d[j] = v;
        }
    }
    for y in (0..h).rev() {
        for x in (0..w).rev() {
            let j = y * w + x;
            let mut v = d[j];
            if x + 1 < w {
                v = v.min(d[j + 1] + 3.0);
            }
            if y + 1 < h {
                v = v.min(d[j + w] + 3.0);
                if x + 1 < w {
                    v = v.min(d[j + w + 1] + 4.0);
                }
                if x > 0 {
                    v = v.min(d[j + w - 1] + 4.0);
                }
            }
            d[j] = v;
        }
    }
    d.iter_mut().for_each(|v| *v /= 3.0);
    d
}

/// erfc, Abramowitz and Stegun 7.1.26 (error below 1.5e-7).
fn erfc(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.3275911 * z);
    let y = t * (0.254829592 + t * (-0.284496736 + t * (1.421413741 + t * (-1.453152027 + t * 1.061405429)))) * (-z * z).exp();
    if x >= 0.0 {
        y
    } else {
        2.0 - y
    }
}

/// A halo is wide by definition, so it is found and fitted on the image at
/// about 512 px and scaled back up.
fn halo(r: &Image) -> Option<Halo> {
    let f = (r.w.max(r.h) / 512).max(1);
    if f == 1 {
        return halo_at(r);
    }
    let (sw, sh) = (r.w.div_ceil(f), r.h.div_ceil(f));
    let mut acc = vec![[0u32; 5]; sw * sh];
    for y in 0..r.h {
        for x in 0..r.w {
            let i = (y * r.w + x) * 4;
            let a = r.data[i + 3] as u32;
            let c = &mut acc[(y / f) * sw + x / f];
            for k in 0..3 {
                c[k] += r.data[i + k] as u32 * a;
            }
            c[3] += a;
            c[4] += 1;
        }
    }
    let data: Vec<u8> = acc
        .iter()
        .flat_map(|c| {
            let a = c[3];
            let px = |k: usize| if a > 0 { ((c[k] + a / 2) / a) as u8 } else { 0 };
            [px(0), px(1), px(2), ((a + c[4] / 2) / c[4]) as u8]
        })
        .collect();
    let mut hl = halo_at(&Image { w: sw, h: sh, data: &data })?;
    let up = |m: &[u8]| -> Vec<u8> { (0..r.w * r.h).map(|j| m[(j / r.w / f) * sw + (j % r.w) / f]).collect() };
    hl.mask = up(&hl.mask);
    hl.clip = hl.clip.as_deref().map(up);
    hl.sigma *= f as f64;
    hl.clip_f *= f;
    Some(hl)
}

/// Only a band of partial transparency that is wide (well past anti-aliasing
/// and upscaler halos), fades evenly with the distance from the artwork, and
/// runs right round a large opaque shape counts as a halo.
fn halo_at(r: &Image) -> Option<Halo> {
    let (w, h, d) = (r.w, r.h, r.data);
    let n = w * h;
    let long = w.max(h) as f64;
    let alpha = |j: usize| d[j * 4 + 3];
    let opaque: Vec<bool> = (0..n).map(|j| alpha(j) >= 248).collect();
    let n_opaque = opaque.iter().filter(|&&o| o).count();
    let partial = (0..n).filter(|&j| (8..248).contains(&alpha(j))).count();
    dbg_line!("halo: opaque {n_opaque}, partial {partial}");
    if (n_opaque as f64) < 0.01 * n as f64 || (partial as f64) < 0.005 * n as f64 {
        return None;
    }
    // Components of what isn't opaque: the outside, and each hole.
    let mut comp = vec![NONE; n];
    let mut sizes = vec![];
    let mut stack = vec![];
    for j0 in 0..n {
        if opaque[j0] || comp[j0] != NONE {
            continue;
        }
        let id = sizes.len() as u32;
        comp[j0] = id;
        stack.push(j0);
        let mut part = 0usize;
        while let Some(j) = stack.pop() {
            if alpha(j) >= 8 {
                part += 1;
            }
            let x = j % w;
            for (ok, k) in [(x > 0, j.wrapping_sub(1)), (x + 1 < w, j + 1), (j >= w, j.wrapping_sub(w)), (j + w < n, j + w)] {
                if ok && !opaque[k] && comp[k] == NONE {
                    comp[k] = id;
                    stack.push(k);
                }
            }
        }
        sizes.push(part);
    }
    let (cid, &cpart) = sizes.iter().enumerate().max_by_key(|(_, &p)| p)?;
    if (cpart as f64) < 0.005 * n as f64 {
        return None;
    }
    let cid = cid as u32;
    let dist = distance(&opaque, w, h);
    let dmax = (long / 4.0) as usize;
    let mut sum = vec![0f64; dmax + 2];
    let mut sq = vec![0f64; dmax + 2];
    let mut cnt = vec![0f64; dmax + 2];
    let mut cpt = vec![0f64; dmax + 2];
    let mut cols: [Vec<f64>; 3] = [vec![], vec![], vec![]];
    // The first few pixels out are the shape's own anti-aliased edge (wider in
    // an upscale); the halo is what carries on past them.
    let skip = (js_round(long / 256.0) as usize).max(2);
    for j in 0..n {
        if comp[j] != cid {
            continue;
        }
        let b = dist[j].round() as usize;
        if b == 0 || b > dmax {
            continue;
        }
        let a = alpha(j) as f64 / 255.0;
        sum[b] += a;
        sq[b] += a * a;
        cnt[b] += 1.0;
        if alpha(j) >= 8 {
            cpt[b] += 1.0;
            if b >= skip && j % 7 == 0 {
                for ch in 0..3 {
                    cols[ch].push(d[j * 4 + ch] as f64);
                }
            }
        }
    }
    let mean = |b: usize| if cnt[b] > 0.0 { sum[b] / cnt[b] } else { 0.0 };
    let a1 = mean(skip).max(mean(skip + 1));
    dbg_line!("halo: a1 {a1:.3}, profile {:?}", (1..40).step_by(3).map(|b| (mean(b) * 100.0).round() / 100.0).collect::<Vec<_>>());
    if a1 < 0.04 {
        return None;
    }
    let wide = (skip..=dmax).find(|&b| cnt[b] > 0.0 && mean(b) < 0.05 * a1).unwrap_or(dmax);
    dbg_line!("halo: wide {wide}, coverage {:?}", (skip..skip + 3).map(|b| cpt[b] / cnt[b]).collect::<Vec<_>>());
    if (wide as f64) < (long / 128.0).max(8.0) {
        return None;
    }
    // right round the shape
    if (skip..skip + 3).any(|b| cnt[b] == 0.0 || cpt[b] / cnt[b] < 0.7) {
        return None;
    }
    // evenly: falls with distance, and depends on little else
    let rises = (skip..wide).filter(|&b| mean(b + 1) > mean(b) + 2.0 / 255.0).count();
    let (mut spread, mut wsum) = (0.0, 0.0);
    for b in skip..=wide {
        if cnt[b] > 0.0 {
            let m = mean(b);
            spread += (sq[b] / cnt[b] - m * m).max(0.0).sqrt() * cnt[b];
            wsum += cnt[b];
        }
    }
    dbg_line!("halo: rises {rises}, spread {:.3} (a1 {a1:.3})", spread / wsum);
    if rises as f64 > 0.1 * (wide - skip) as f64 || spread / wsum > 0.25 * a1 {
        return None;
    }
    // α(d) = a·½erfc((d − d0)/(σ√2)): a big shape grown by d0, blurred by σ.
    let model = |s: f64, d0: f64, b: usize| 0.5 * erfc((b as f64 - d0) / (s * std::f64::consts::SQRT_2));
    let (mut best, mut be) = ((0.0, 1.0, 0.0), f64::MAX);
    let mut s = wide as f64 / 12.0;
    while s <= wide as f64 {
        for k in -6..=4 {
            let d0 = s * k as f64 / 4.0;
            let (mut fo, mut ff) = (0.0, 0.0);
            for b in skip..=wide {
                let f = model(s, d0, b);
                fo += f * mean(b) * cnt[b];
                ff += f * f * cnt[b];
            }
            if ff == 0.0 {
                continue;
            }
            let a = (fo / ff).clamp(0.0, 1.0);
            let e: f64 = (skip..=wide).map(|b| (mean(b) - a * model(s, d0, b)).powi(2) * cnt[b]).sum();
            if e < be {
                (best, be) = ((a, s, d0), e);
            }
        }
        s *= 1.1;
    }
    let total: f64 = (skip..=wide).map(|b| mean(b).powi(2) * cnt[b]).sum();
    dbg_line!("halo: fit {best:?}, error {:.3}", be / total);
    if be > 0.1 * total {
        return None;
    }
    // That profile is a guide; the fit itself is in 2D, blurring the actual
    // shape at reduced size, so its notches and corners count too. The shape is
    // everything but the outside, grown or shrunk by d0 (signed distance sd:
    // positive outside).
    let filled: Vec<bool> = (0..n).map(|j| comp[j] != cid).collect();
    let outside: Vec<bool> = filled.iter().map(|f| !f).collect();
    let din = distance(&outside, w, h);
    let sd: Vec<f32> = (0..n).map(|j| if filled[j] { -din[j] } else { dist[j] }).collect();
    let ds = (js_round(long / 256.0) as usize).max(1);
    let (gw, gh) = (w.div_ceil(ds), h.div_ceil(ds));
    let (mut sdsum, mut asum, mut cj, mut cn) = (vec![0f64; gw * gh], vec![0f64; gw * gh], vec![0u32; gw * gh], vec![0u32; gw * gh]);
    for j in 0..n {
        let k = (j / w / ds) * gw + (j % w) / ds;
        cn[k] += 1;
        sdsum[k] += sd[j] as f64;
        if comp[j] == cid && dist[j] as usize >= skip {
            cj[k] += 1;
            asum[k] += alpha(j) as f64 / 255.0;
        }
    }
    // Only cells wholly past the edge: a part cell would compare its faint far
    // pixels with the prediction for all of it.
    let mut judged: Vec<usize> = (0..gw * gh).filter(|&k| cj[k] > 0 && cj[k] == cn[k]).collect();
    let obs: Vec<f64> = (0..gw * gh).map(|k| if cj[k] > 0 { asum[k] / cj[k] as f64 } else { 0.0 }).collect();
    let sdbar: Vec<f64> = (0..gw * gh).map(|k| sdsum[k] / cn[k].max(1) as f64).collect();
    let fit2 = |judged: &[usize], s: f64, d0: f64| -> (f64, f64) {
        let m: Vec<f32> = sdbar.iter().map(|&v| (0.5 + (d0 - v) / ds as f64).clamp(0.0, 1.0) as f32).collect();
        let pr = blur(&m, gw, gh, s / ds as f64);
        let (mut fo, mut ff) = (0.0, 0.0);
        for &k in judged {
            fo += pr[k] as f64 * obs[k];
            ff += (pr[k] as f64).powi(2);
        }
        let a = if ff > 0.0 { (fo / ff).clamp(0.0, 1.0) } else { 0.0 };
        (a, judged.iter().map(|&k| (obs[k] - a * pr[k] as f64).powi(2)).sum())
    };
    let (_, s1, _) = best;
    let (mut a, mut sigma, mut d0, mut be2) = (0.0, s1, 0.0, f64::MAX);
    for round in 0..2 {
        dbg_line!("halo: 2D round {round}: a {a:.2} σ {sigma:.1} d0 {d0:.1}, {} cells", judged.len());
        if round == 1 {
            // Glows reaching past the artwork only ever add to the halo: drop the
            // cells the first fit can't explain, and fit again.
            let m: Vec<f32> = sdbar.iter().map(|&v| (0.5 + (d0 - v) / ds as f64).clamp(0.0, 1.0) as f32).collect();
            let pr = blur(&m, gw, gh, sigma / ds as f64);
            let mut r: Vec<f64> = judged.iter().map(|&k| (obs[k] - a * pr[k] as f64).abs()).collect();
            let lim = (3.0 * percentile(&mut r, 0.5)).max(0.02);
            judged.retain(|&k| obs[k] - a * pr[k] as f64 <= lim);
            be2 = f64::MAX;
        }
        for sf in [0.6, 0.7, 0.8, 0.9, 1.0, 1.1, 1.25, 1.4] {
            for k in -6..=4 {
                let (s, dd) = (s1 * sf, s1 * sf * k as f64 / 4.0);
                let (aa, e) = fit2(&judged, s, dd);
                if e < be2 {
                    (a, sigma, d0, be2) = (aa, s, dd, e);
                }
            }
        }
    }
    let total2: f64 = judged.iter().map(|&k| obs[k].powi(2)).sum();
    dbg_line!("halo: 2D fit a {a:.2} σ {sigma:.1} d0 {d0:.1}, error {:.3}", be2 / total2);
    if be2 > 0.1 * total2 {
        return None;
    }
    let mask: Vec<u8> = (0..n).map(|j| (sd[j] as f64 <= d0) as u8).collect();
    // Holes in the artwork stay clear of the halo, as do their blended rims:
    // the clip leaves out each hole grown by a few pixels, so its edge lies
    // under the artwork and can be traced coarsely.
    let holes: Vec<bool> = (0..n).map(|j| !opaque[j] && comp[j] != cid).collect();
    let clip_f = (js_round(long / 700.0) as usize).max(1);
    let clip = if holes.iter().any(|&b| b) {
        let dh = distance(&holes, w, h);
        Some((0..n).map(|j| (dh[j] as f64 > 2.0 * clip_f as f64) as u8).collect())
    } else {
        None
    };
    // Glows spilling past the artwork tint some of the halo, so take the median.
    let col = [0, 1, 2].map(|ch| percentile(&mut cols[ch], 0.5));
    Some(Halo { mask, clip, clip_f, col, a, sigma })
}

// ---------- putting it together ----------

/// Width and height of the set pixels' bounding box.
fn bbox(bits: &[u8], w: usize, h: usize) -> (usize, usize) {
    let (mut x0, mut x1, mut y0, mut y1) = (w, 0, h, 0);
    for (j, &b) in bits.iter().enumerate() {
        if b != 0 {
            let (x, y) = (j % w, j / w);
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
    }
    if x1 < x0 {
        (1, 1)
    } else {
        (x1 - x0 + 1, y1 - y0 + 1)
    }
}

/// Trace with gradients. When nothing qualifies, the result is exactly the
/// ordinary trace.
pub fn trace(r: &Image, o: &Opts, p: &Pal, idx: &[i16]) -> Traced {
    #[cfg(not(target_arch = "wasm32"))]
    let t0 = std::time::Instant::now();
    #[cfg(not(target_arch = "wasm32"))]
    let lap = |what: &str| dbg_line!("  {what}: {} ms", t0.elapsed().as_millis());
    #[cfg(target_arch = "wasm32")]
    let lap = |_: &str| {};
    let (w, h) = (r.w, r.h);
    let reg = regions(idx, w, h);
    let bd = borders(r, &reg, p);
    let nreg = reg.col.len();
    lap("regions and borders");
    let mut parent: Vec<u32> = (0..nreg as u32).collect();
    let locked = |g: u32| p.pinned.get(reg.col[g as usize] as usize).copied().unwrap_or(false);
    for (&(a, b), e) in &bd.pairs {
        if e.is_ramp(gap(p, &reg, a, b)) && !locked(a) && !locked(b) {
            let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
            if ra != rb {
                parent[ra.max(rb) as usize] = ra.min(rb);
            }
        }
    }
    dbg_line!("regions {}, borders {}, ramp borders {}", nreg, bd.pairs.len(), bd.pairs.iter().filter(|(&(a, b), e)| e.is_ramp(gap(p, &reg, a, b))).count());
    if std::env::var_os("SLIMMER_DEBUG_BORDERS").is_some() {
        let mut v: Vec<_> = bd.pairs.iter().filter(|(_, e)| e.n >= 40).collect();
        v.sort_by_key(|(_, e)| std::cmp::Reverse(e.n));
        for ((a, b), e) in v.iter().take(60) {
            dbg_line!("  {:>5} {} {:>5} {}  n {:>6} sharp {:>5.2} inner/n {:>6.2} outer/inner {:>5.2} {:>5.2} step/n {:>5.2} ramp {}", a, to_hex(p.pal[reg.col[*a as usize] as usize]), b, to_hex(p.pal[reg.col[*b as usize] as usize]), e.n, e.sharp as f64 / e.n as f64, e.inner / e.n as f64, e.outer[0] / e.inner, e.outer[1] / e.inner, e.step / e.n as f64, e.is_ramp(gap(p, &reg, *a, *b)));
        }
    }
    let mut groups: HashMap<u32, Vec<u32>> = HashMap::new();
    for g in 0..nreg as u32 {
        let root = find(&mut parent, g);
        groups.entry(root).or_default().push(g);
    }
    let long = w.max(h) as f64;
    let min_area = (long / 64.0).powi(2) as usize;
    let mut cands: Vec<Vec<u32>> = groups
        .into_values()
        .filter(|g| {
            let mut cols: Vec<i16> = g.iter().map(|&x| reg.col[x as usize]).collect();
            cols.sort();
            cols.dedup();
            g.len() >= 2 && cols.len() >= 2 && g.iter().map(|&x| reg.n[x as usize]).sum::<usize>() >= min_area
        })
        .collect();
    cands.sort_by_key(|g| g[0]);
    // Region bounding boxes
    let mut rb: Vec<Bbox> = (0..nreg).map(|_| Bbox { x0: usize::MAX, y0: usize::MAX, x1: 0, y1: 0 }).collect();
    for (j, &g) in reg.lab.iter().enumerate() {
        if g != NONE {
            let (x, y) = (j % w, j / w);
            let b = &mut rb[g as usize];
            b.x0 = b.x0.min(x);
            b.x1 = b.x1.max(x);
            b.y0 = b.y0.min(y);
            b.y1 = b.y1.max(y);
        }
    }
    let mut area_of = vec![usize::MAX; nreg];
    for (i, g) in cands.iter().enumerate() {
        for &x in g {
            area_of[x as usize] = i;
        }
    }
    // Slivers of a ramp's own colours cut off by a sharp edge are the blended
    // rim round a shape inside it, not shapes of their own: they join the ramp,
    // or they would be painted over its glows. Thin lines in other colours stay.
    let st = (js_round(long / 2048.0) as usize).max(1);
    let d = r.data;
    let mut edgy = vec![0usize; nreg];
    for y in 0..h {
        for x in 0..w {
            let j = y * w + x;
            let g = reg.lab[j];
            if g == NONE {
                continue;
            }
            let solid = (x < st || l1(d, j, j - st) < SHARP)
                && (x + st >= w || l1(d, j, j + st) < SHARP)
                && (y < st || l1(d, j, j - st * w) < SHARP)
                && (y + st >= h || l1(d, j, j + st * w) < SHARP);
            if !solid {
                edgy[g as usize] += 1;
            }
        }
    }
    // So do islands of a ramp's colours inside it with no sharp edge: noise in
    // the banding, not shapes.
    let mut touching: Vec<HashMap<usize, (u32, u32)>> = vec![HashMap::new(); nreg];
    for (&(a, b), e) in &bd.pairs {
        for (x, y) in [(a, b), (b, a)] {
            if area_of[y as usize] != usize::MAX && area_of[x as usize] == usize::MAX {
                let t = touching[x as usize].entry(area_of[y as usize]).or_default();
                t.0 += e.n;
                t.1 += e.sharp;
            }
        }
    }
    let area_cols: Vec<Vec<i16>> = cands.iter().map(|g| g.iter().map(|&x| reg.col[x as usize]).collect()).collect();
    for g in 0..nreg {
        if area_of[g] != usize::MAX || locked(g as u32) {
            continue;
        }
        // and crumbs too small to be shapes of their own
        let crumb = (reg.n[g] as f64) < (long / 128.0).powi(2);
        let joins = touching[g].iter().filter(|(&i, _)| area_cols[i].contains(&reg.col[g])).max_by_key(|(_, t)| t.0).filter(|(_, &(n, sharp))| {
            crumb || edgy[g] * 2 >= reg.n[g] || ((sharp as f64) < 0.2 * n as f64 && n as f64 >= 0.8 * bd.contacts[g] as f64)
        });
        if let Some((&i, _)) = joins {
            area_of[g] = i;
            cands[i].push(g as u32);
        }
    }
    // A region is enclosed by an area when nearly all its border is with it.
    let mut with_area: Vec<HashMap<usize, u32>> = vec![HashMap::new(); nreg];
    for (&(a, b), e) in &bd.pairs {
        if area_of[b as usize] != usize::MAX && area_of[a as usize] == usize::MAX {
            *with_area[a as usize].entry(area_of[b as usize]).or_default() += e.n;
        }
        if area_of[a as usize] != usize::MAX && area_of[b as usize] == usize::MAX {
            *with_area[b as usize].entry(area_of[a as usize]).or_default() += e.n;
        }
    }
    let enclosed_in = |i: usize| -> Vec<u32> {
        (0..nreg as u32)
            .filter(|&x| area_of[x as usize] == usize::MAX && with_area[x as usize].get(&i).is_some_and(|&c| c as f64 >= 0.9 * bd.contacts[x as usize] as f64))
            .collect()
    };
    // Fit an area; where some of its regions fit worse than their bands, try
    // again without them, a few times.
    let work = |i: usize| -> Option<Area> {
        let mut g = cands[i].clone();
        for _ in 0..6 {
            if g.len() < 2 || g.iter().map(|&x| reg.n[x as usize]).sum::<usize>() < min_area {
                return None;
            }
            let mut member = vec![false; nreg];
            let mut bb = Bbox { x0: usize::MAX, y0: usize::MAX, x1: 0, y1: 0 };
            for &x in &g {
                member[x as usize] = true;
                let b = &rb[x as usize];
                bb.x0 = bb.x0.min(b.x0);
                bb.y0 = bb.y0.min(b.y0);
                bb.x1 = bb.x1.max(b.x1);
                bb.y1 = bb.y1.max(b.y1);
            }
            let enclosed = enclosed_in(i);
            let mut hidden = vec![false; nreg];
            for &x in &enclosed {
                hidden[x as usize] = true;
            }
            match fit_area(r, p, idx, &reg, &member, &hidden, &bb, enclosed, g.clone()) {
                Ok(a) => return Some(a),
                Err(keep) if keep.is_empty() || keep.len() == g.len() => return None,
                Err(keep) => {
                    dbg_line!("  area {i}: {} of {} regions fit worse than their bands; again without them", g.len() - keep.len(), g.len());
                    g = keep;
                }
            }
        }
        None
    };
    #[cfg(feature = "parallel")]
    let areas: Vec<Area> = {
        use rayon::prelude::*;
        (0..cands.len()).into_par_iter().filter_map(work).collect()
    };
    #[cfg(not(feature = "parallel"))]
    let areas: Vec<Area> = (0..cands.len()).filter_map(work).collect();
    lap("areas fitted");
    for (i, g) in cands.iter().enumerate() {
        dbg_line!("area cand {i}: {} regions, {} px", g.len(), g.iter().map(|&x| reg.n[x as usize]).sum::<usize>());
    }
    for a in &areas {
        dbg_line!("area: {} regions, base {:?}, {} glows", a.regions.len(), a.base, a.glows.len());
    }
    let halo = halo(r);
    lap("halo");
    if areas.is_empty() && halo.is_none() {
        return trace_index(idx, w, h, o, p);
    }

    // Each area becomes one colour of its own, painted no higher than any of its
    // bands or the shapes inside it, so those stay on top of it and its glows.
    let mut p2 = p.clone();
    let mut idx2 = idx.to_vec();
    let rank = ranks(p);
    let mut virt = vec![];
    for a in &areas {
        let v = p2.pal.len();
        let c = a.base.at(0.0, 0.0);
        p2.pal.push(c.map(|x| js_round(x) as i32));
        p2.share.push(0.0);
        p2.pinned.push(false);
        let lowest = a.regions.iter().chain(&a.enclosed).map(|&g| rank[reg.col[g as usize] as usize]).filter(|&k| k >= 0).min().unwrap_or(0);
        virt.push((v, lowest));
        let mut member = vec![false; nreg];
        for &g in &a.regions {
            member[g as usize] = true;
        }
        for (j, &g) in reg.lab.iter().enumerate() {
            if g != NONE && member[g as usize] {
                idx2[j] = v as i16;
            }
        }
    }
    // New order: the old one with each area slotted in at its lowest rank (ties
    // keep area order), and colours no pixel uses any more dropped.
    let mut used = vec![false; p2.pal.len()];
    for &v in &idx2 {
        if v >= 0 {
            used[v as usize] = true;
        }
    }
    let mut order = vec![];
    for (k, &ci) in p.order.iter().enumerate() {
        for &(v, lo) in &virt {
            if lo == k as i32 {
                order.push(v);
            }
        }
        if used[ci] {
            order.push(ci);
        }
    }
    p2.order = order;
    let rank2 = ranks(&p2);

    // Trace everything: the layers, then each glow and halo shape.
    // Blurred shapes are traced at a scale their blur hides (a fifth of it).
    enum Job<'a> {
        Layer(usize),
        Mask(&'a [u8], usize),
    }
    let coarse = |sigma: f64| ((sigma / 5.0) as usize).clamp(1, 8);
    let mut jobs: Vec<Job> = (0..p2.order.len()).map(Job::Layer).collect();
    for a in &areas {
        for g in &a.glows {
            jobs.push(Job::Mask(&g.mask, coarse(g.sigma)));
        }
    }
    if let Some(hl) = &halo {
        jobs.push(Job::Mask(&hl.mask, coarse(hl.sigma)));
        if let Some(c) = &hl.clip {
            jobs.push(Job::Mask(c, hl.clip_f));
        }
    }
    let run = |job: &Job| match job {
        Job::Layer(k) => layer_d(&idx2, w, h, o, &rank2, *k),
        Job::Mask(bits, f) => coarse_d(bits, w, h, o, *f),
    };
    #[cfg(feature = "parallel")]
    let ds: Vec<String> = {
        use rayon::prelude::*;
        jobs.par_iter().map(run).collect()
    };
    #[cfg(not(feature = "parallel"))]
    let ds: Vec<String> = jobs.iter().map(run).collect();
    lap("traced");
    let mut dq = ds.into_iter();
    let layer_ds: Vec<String> = (0..p2.order.len()).map(|_| dq.next().unwrap()).collect();

    let dec = o.dec;
    let mut defs = String::new();
    let mut body = String::new();
    let mut filters: Vec<String> = vec![];
    // Filter regions reach three blurs past the shape: the default (10% round
    // its bounding box) would square off a wide blur.
    let mut filter = |defs: &mut String, sigma: f64, (bw, bh): (usize, usize)| -> usize {
        let sd = num(sigma, 1);
        let k = (3.0 * sigma / bw.min(bh).max(1) as f64 / 0.25).ceil() * 0.25;
        let key = format!("{sd} {k}");
        if let Some(i) = filters.iter().position(|s| *s == key) {
            return i;
        }
        defs.push_str(&format!(
            "<filter id=\"f{}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"><feGaussianBlur stdDeviation=\"{sd}\"/></filter>",
            filters.len(),
            num(-k, 2),
            num(-k, 2),
            num(1.0 + 2.0 * k, 2),
            num(1.0 + 2.0 * k, 2)
        ));
        filters.push(key);
        filters.len() - 1
    };
    let mut paths = 0;
    let mut glow_ds: Vec<Vec<String>> = areas.iter().map(|a| a.glows.iter().map(|_| dq.next().unwrap()).collect()).collect();
    if let Some(hl) = &halo {
        let md = dq.next().unwrap();
        let clip = hl.clip.as_ref().map(|_| dq.next().unwrap());
        let f = filter(&mut defs, hl.sigma, bbox(&hl.mask, w, h));
        let mut attrs = format!(" fill=\"{}\" opacity=\"{}\" filter=\"url(#f{f})\"", hex(hl.col), num(hl.a, 2));
        if let Some(cd) = clip {
            defs.push_str(&format!("<clipPath id=\"c0\"><path d=\"{cd}\"/></clipPath>"));
            attrs.push_str(" clip-path=\"url(#c0)\"");
        }
        body.push_str(&format!("<path{attrs} d=\"{md}\"/>"));
        paths += 1;
    }
    let stops = |stops: &[(f64, C3)], op: Option<(f64, f64)>| -> String {
        stops
            .iter()
            .enumerate()
            .map(|(i, (t, c))| {
                let opacity = op.map_or(String::new(), |(a0, a1)| format!(" stop-opacity=\"{}\"", num(if i == 0 { a0 } else { a1 }, 2)));
                format!("<stop offset=\"{}\" stop-color=\"{}\"{opacity}/>", num(*t, 3), hex(*c))
            })
            .collect()
    };
    for (k, &ci) in p2.order.iter().enumerate() {
        let d = &layer_ds[k];
        if d.is_empty() {
            continue;
        }
        let Some(ai) = virt.iter().position(|&(v, _)| v == ci) else {
            let hx = to_hex(p2.pal[ci]);
            let fill = if hx == "#000000" { String::new() } else { format!(" fill=\"{hx}\"") };
            body.push_str(&format!("<path{fill} d=\"{d}\"/>"));
            paths += 1;
            continue;
        };
        let a = &areas[ai];
        let g = &a.base.grad;
        let fill = match g.geo {
            Geo::Flat => hex(g.stops[0].1),
            Geo::Linear([x1, y1, x2, y2]) => {
                defs.push_str(&format!(
                    "<linearGradient id=\"g{ai}\" gradientUnits=\"userSpaceOnUse\" x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\">{}</linearGradient>",
                    num(x1, dec),
                    num(y1, dec),
                    num(x2, dec),
                    num(y2, dec),
                    stops(&g.stops, None)
                ));
                format!("url(#g{ai})")
            }
            Geo::Radial([cx, cy, rr]) => {
                defs.push_str(&format!(
                    "<radialGradient id=\"g{ai}\" gradientUnits=\"userSpaceOnUse\" cx=\"{}\" cy=\"{}\" r=\"{}\">{}</radialGradient>",
                    num(cx, dec),
                    num(cy, dec),
                    num(rr, dec),
                    stops(&g.stops, None)
                ));
                format!("url(#g{ai})")
            }
        };
        match &a.base.sheen {
            None => body.push_str(&format!("<path fill=\"{fill}\" d=\"{d}\"/>")),
            Some(sh) => {
                defs.push_str(&format!(
                    "<radialGradient id=\"s{ai}\" gradientUnits=\"userSpaceOnUse\" cx=\"{}\" cy=\"{}\" r=\"{}\">{}</radialGradient><path id=\"a{ai}\" d=\"{d}\"/>",
                    num(sh.c[0], dec),
                    num(sh.c[1], dec),
                    num(sh.c[2], dec),
                    stops(&[(0.0, sh.col), (1.0, sh.col)], Some((sh.a, 0.0)))
                ));
                body.push_str(&format!("<use href=\"#a{ai}\" fill=\"{fill}\"/><use href=\"#a{ai}\" fill=\"url(#s{ai})\"/>"));
            }
        }
        paths += 1;
        // Each glow is a path of its own, so where two overlap they add up as
        // separate glows do. Runs of one colour share their fill.
        let mut run: Vec<(String, String)> = vec![];
        let flush = |body: &mut String, run: &mut Vec<(String, String)>, paths: &mut usize| {
            if run.is_empty() {
                return;
            }
            if run.len() == 1 {
                let (fill, p) = run.pop().unwrap();
                body.push_str(&format!("<path fill=\"{fill}\"{p}"));
            } else {
                body.push_str(&format!("<g fill=\"{}\">", run[0].0));
                for (_, p) in run.drain(..) {
                    body.push_str(&format!("<path{p}"));
                }
                body.push_str("</g>");
            }
            *paths += 1;
        };
        for (gl, gd) in a.glows.iter().zip(std::mem::take(&mut glow_ds[ai])) {
            if gd.is_empty() {
                continue;
            }
            let f = filter(&mut defs, gl.sigma, bbox(&gl.mask, w, h));
            let op = num(gl.a, 2);
            let op = if op == "1" { String::new() } else { format!(" opacity=\"{op}\"") };
            let fill = hex(gl.col);
            if run.last().is_some_and(|r| r.0 != fill) {
                flush(&mut body, &mut run, &mut paths);
            }
            run.push((fill, format!("{op} filter=\"url(#f{f})\" d=\"{gd}\"/>")));
        }
        flush(&mut body, &mut run, &mut paths);
    }
    let svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {w} {h}\" fill-rule=\"evenodd\">{}{body}</svg>\n",
        if defs.is_empty() { String::new() } else { format!("<defs>{defs}</defs>") }
    );
    Traced { svg, layers: paths }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{palette, trace};

    /// A square on transparency, coloured by `f(x)` across it.
    fn square(f: impl Fn(f64) -> [u8; 3]) -> Vec<u8> {
        let n = 512;
        let mut d = vec![0u8; n * n * 4];
        for y in 64..448 {
            for x in 64..448 {
                let c = f((x - 64) as f64 / 383.0);
                d[(y * n + x) * 4..(y * n + x) * 4 + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
            }
        }
        d
    }

    fn both(d: &[u8]) -> (String, String) {
        let r = Image { w: 512, h: 512, data: d };
        let o = Opts { grid: 512, ..Opts::default() };
        let p = palette::palette_for(&r, &o, &[]).unwrap();
        let plain = trace::trace(&r, &o, &p).svg;
        let grad = trace::trace(&r, &Opts { gradients: true, ..o }, &p).svg;
        (plain, grad)
    }

    #[test]
    fn a_ramp_becomes_a_gradient() {
        let lerp = |a: f64, b: f64, t: f64| (a + (b - a) * t).round() as u8;
        let (plain, grad) = both(&square(|t| [lerp(255.0, 40.0, t), lerp(140.0, 60.0, t), lerp(30.0, 220.0, t)]));
        assert!(!plain.contains("Gradient") && plain.matches("<path").count() > 3, "{plain}");
        assert!(grad.contains("<linearGradient") && grad.matches("<path").count() == 1, "{grad}");
    }

    /// Two close flat colours meeting at a soft edge step within a pixel or two:
    /// not a ramp, so nothing changes.
    #[test]
    fn close_flat_colours_stay_bands() {
        let (plain, grad) = both(&square(|t| {
            let e = ((t - 0.5) * 383.0 / 2.0 + 0.5).clamp(0.0, 1.0);
            [(200.0 + 22.0 * e).round() as u8, 60, 60]
        }));
        assert!(plain.matches("<path").count() == 2, "{plain}");
        assert_eq!(plain, grad);
    }

    /// The three box blurs spread a point about as far as a Gaussian of that sigma.
    #[test]
    fn blur_matches_sigma() {
        for sigma in [1.5, 3.0, 5.0, 8.0, 20.0] {
            let w = 201;
            let mut m = vec![0f32; w * w];
            m[100 * w + 100] = 1.0;
            let b = blur(&m, w, w, sigma);
            let total: f64 = b.iter().map(|&v| v as f64).sum();
            let var: f64 = b.iter().enumerate().map(|(i, &v)| v as f64 * ((i % w) as f64 - 100.0).powi(2)).sum::<f64>() / total;
            assert!((total - 1.0).abs() < 1e-3, "sigma {sigma}: total {total}");
            // three boxes come out slightly narrower than the Gaussian
            assert!(var.sqrt() < sigma && var.sqrt() > sigma - 0.6, "sigma {sigma}: spread {}", var.sqrt());
        }
    }
}
