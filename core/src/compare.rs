//! Comparing the traced result with the original, colour by colour.

use crate::index::{classify, mono_index};
use crate::palette::Pal;
use crate::{png, Image, Mode, Opts};

pub struct Compared {
    /// Changed pixels as a percentage of the artwork's pixels.
    pub diff_pct: f64,
    /// Changed pixels in red over a faint copy of the artwork, as a PNG.
    pub diff_png: Vec<u8>,
    /// Mean colour difference (CIE76 ΔE) over the artwork, see [`colour_error`].
    pub de_mean: f64,
    /// Percentage of the artwork's pixels that are noticeably off (ΔE over 5).
    pub de_over: f64,
}

/// sRGB 0–255 to linear light.
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

/// How far the result's colours are from the original's, as people see it: the
/// mean CIE76 ΔE over the artwork (pixels either image covers) and the share of
/// pixels over ΔE 5. See-through pixels are put over white and over black and
/// the worse counts, so a wrong opacity shows as well as a wrong colour. The
/// palette-based score above can't see colour inside a band (a ramp traced as
/// bands scores as well as a gradient); this one can. A background removed on
/// purpose (opaque in the original, but outside the artwork in its index map
/// `a_idx`) doesn't count. Samples about a million pixels.
pub fn colour_error(a_idx: &[i16], a: &Image, b: &Image) -> (f64, f64) {
    let (w, h) = (a.w.min(b.w), a.h.min(b.h));
    let st = (w.max(h) / 1024).max(1);
    let lin = linear_table();
    let over = |d: &[u8], i: usize, bg: u32| {
        let al = d[i + 3] as u32;
        let c = |k: usize| ((d[i + k] as u32 * al + bg * (255 - al) + 127) / 255) as u8;
        (c(0), c(1), c(2))
    };
    // Runs of one colour are common: remember the last pixel of each image.
    let both = |d: &[u8], i: usize| -> [[f32; 3]; 2] {
        if d[i + 3] == 255 {
            let l = lab(&lin, d[i], d[i + 1], d[i + 2]);
            return [l, l];
        }
        [0u32, 255].map(|bg| {
            let (r, g, bl) = over(d, i, bg);
            lab(&lin, r, g, bl)
        })
    };
    let (mut ka, mut la) = (u32::MAX, [[0f32; 3]; 2]);
    let (mut kb, mut lb) = (u32::MAX, [[0f32; 3]; 2]);
    let key = |d: &[u8], i: usize| u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]);
    let (mut sum, mut n, mut big) = (0f64, 0usize, 0usize);
    for y in (0..h).step_by(st) {
        for x in (0..w).step_by(st) {
            let (i, k) = ((y * a.w + x) * 4, (y * b.w + x) * 4);
            if (a.data[i + 3] == 0 && b.data[k + 3] == 0) || (a_idx[y * a.w + x] < 0 && a.data[i + 3] >= 128) {
                continue;
            }
            if key(a.data, i) != ka {
                ka = key(a.data, i);
                la = both(a.data, i);
            }
            if key(b.data, k) != kb {
                kb = key(b.data, k);
                lb = both(b.data, k);
            }
            let de = |p: [f32; 3], q: [f32; 3]| ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2)).sqrt();
            let e = de(la[0], lb[0]).max(de(la[1], lb[1])) as f64;
            sum += e;
            n += 1;
            if e > 5.0 {
                big += 1;
            }
        }
    }
    if n == 0 {
        (0.0, 0.0)
    } else {
        (sum / n as f64, 100.0 * big as f64 / n as f64)
    }
}

/// `a_idx` is the original's index map at `CMP` (see `index::source_index`) and
/// `a` the original drawn at `CMP` (over white in mono mode). `b` is the traced
/// SVG drawn at the same size (also over white in mono mode).
pub fn compare(a_idx: &[i16], a: &Image, b: &Image, o: &Opts, p: &Pal) -> Compared {
    let b_idx = match o.mode {
        Mode::Mono => mono_index(b, 128.0),
        Mode::Colour => classify(b, &p.pal, p.bg, o.edges),
    };
    let (w, h) = (a.w.min(b.w), a.h.min(b.h));
    // A pixel only counts as changed if the other image has no pixel of the
    // same colour within one pixel of it. Edges that moved by less than a
    // hairline are invisible, and otherwise swamp the count on busy artwork.
    // A bitmap drawn bigger than its own size can't place an edge closer than
    // one of its own pixels, so there it's within half of one.
    let s = w.max(h) as f64 / o.src as f64;
    let r = if o.src > 0 && s >= 1.5 { (s / 2.0).ceil() as usize } else { 1 };
    let near = |idx: &[i16], iw: usize, x: usize, y: usize, v: i16| {
        for yy in y.saturating_sub(r)..=(y + r).min(h - 1) {
            for xx in x.saturating_sub(r)..=(x + r).min(w - 1) {
                if idx[yy * iw + xx] == v {
                    return true;
                }
            }
        }
        false
    };
    // Blended edge pixels can land on a third palette colour by accident, so a
    // change only counts where at least one image is solid colour. A lost
    // detail or an added blob always leaves solid colour behind in one of them.
    let solid = |d: &[u8], iw: usize, x: usize, y: usize| {
        let i = (y * iw + x) * 4;
        let same = |k: usize| {
            (d[i] as i32 - d[k] as i32).abs()
                + (d[i + 1] as i32 - d[k + 1] as i32).abs()
                + (d[i + 2] as i32 - d[k + 2] as i32).abs()
                + (d[i + 3] as i32 - d[k + 3] as i32).abs()
                < 24
        };
        (x == 0 || same(i - 4)) && (x == w - 1 || same(i + 4)) && (y == 0 || same(i - iw * 4)) && (y == h - 1 || same(i + iw * 4))
    };
    // With gradients, a smooth ramp crosses band boundaries anywhere, so a pixel
    // a hair either side of one isn't a change unless its colour really moved:
    // by half the distance at which two colours count as one.
    let close = |x: usize, y: usize| {
        let (i, k) = ((y * a.w + x) * 4, (y * b.w + x) * 4);
        let e: f64 = (0..4).map(|c| (a.data[i + c] as f64 - b.data[k + c] as f64).powi(2)).sum();
        e.sqrt() < o.merge.max(16.0) / 2.0
    };
    // 0 untouched, 1 artwork, 2 changed
    let mut marks = vec![0u8; w * h];
    let (mut diff, mut ink) = (0usize, 0usize);
    for y in 0..h {
        for x in 0..w {
            let va = a_idx[y * a.w + x];
            let vb = b_idx[y * b.w + x];
            if va >= 0 {
                ink += 1;
            }
            if va != vb
                && !(near(&b_idx, b.w, x, y, va) && near(a_idx, a.w, x, y, vb))
                && (solid(a.data, a.w, x, y) || solid(b.data, b.w, x, y))
                && !(o.gradients && close(x, y))
            {
                diff += 1;
                marks[y * w + x] = 2;
            } else if va >= 0 {
                marks[y * w + x] = 1;
            }
        }
    }
    let (de_mean, de_over) = colour_error(a_idx, a, b);
    Compared {
        diff_pct: if ink > 0 { 100.0 * diff as f64 / ink as f64 } else { 0.0 },
        diff_png: png::write(w, h, &[[0, 0, 0], [20, 26, 34], [214, 40, 40]], Some(&[0, 34, 255]), &marks),
        de_mean,
        de_over,
    }
}
