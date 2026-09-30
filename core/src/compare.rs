//! Comparing the traced result with the original, colour by colour.

use crate::index::{classify, mono_index};
use crate::palette::Pal;
use crate::{png, Image, Mode, Opts};

pub struct Compared {
    /// Changed pixels as a percentage of the artwork's pixels.
    pub diff_pct: f64,
    /// Changed pixels in red over a faint copy of the artwork, as a PNG.
    pub diff_png: Vec<u8>,
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
    let near = |idx: &[i16], iw: usize, x: usize, y: usize, v: i16| {
        for yy in y.saturating_sub(1)..=(y + 1).min(h - 1) {
            for xx in x.saturating_sub(1)..=(x + 1).min(w - 1) {
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
            {
                diff += 1;
                marks[y * w + x] = 2;
            } else if va >= 0 {
                marks[y * w + x] = 1;
            }
        }
    }
    Compared {
        diff_pct: if ink > 0 { 100.0 * diff as f64 / ink as f64 } else { 0.0 },
        diff_png: png::write(w, h, &[[0, 0, 0], [20, 26, 34], [214, 40, 40]], Some(&[0, 34, 255]), &marks),
    }
}
