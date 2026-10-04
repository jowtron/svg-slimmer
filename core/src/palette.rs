//! Finding the palette, the background, and the order layers are stacked in.

use serde::{Deserialize, Serialize};

use crate::index::classify;
use crate::{dist2, js_round, Choices, Image, Opts, Rgb};

/// How close a colour must be to count as one the viewer picked.
const SAME: f64 = 24.0 * 24.0;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Share {
    pub c: Rgb,
    pub share: f64,
}

/// Palette, background and layer order for one set of settings.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pal {
    pub pal: Vec<Rgb>,
    /// Index of the background colour, or -1.
    pub bg: i32,
    /// Palette indices in use, biggest area first: the order layers are painted in.
    pub order: Vec<usize>,
    /// Each colour's share of the artwork.
    pub share: Vec<f64>,
    pub pinned: Vec<bool>,
    pub merged: Vec<Share>,
    pub removed: Vec<Share>,
}

impl Pal {
    /// One-colour mode: a single fill colour, no background.
    pub fn mono(fill: Rgb) -> Pal {
        Pal { pal: vec![fill], bg: -1, order: vec![0], share: vec![1.0], pinned: vec![false], merged: vec![], removed: vec![] }
    }
}

type C = [f64; 3];

fn to_f(c: &Rgb) -> C {
    [c[0] as f64, c[1] as f64, c[2] as f64]
}

struct Found {
    cols: Vec<C>,
    pinned: Vec<bool>,
    merged: Vec<(C, f64)>,
    removed: Vec<(C, f64)>,
    cand: Vec<(C, f64)>,
}

#[derive(Clone)]
struct Entry {
    c: C,
    n: f64,
    pin: bool,
}

/// Find the palette from the pixels, then snap to the colours the SVG declares.
/// Returns the palette plus what was merged or removed.
fn find_palette(r: &Image, merge: f64, declared: &[Rgb], max_col: usize, choices: &Choices) -> Found {
    let (d, w) = (r.data, r.w);
    let declared: Vec<C> = declared.iter().map(to_f).collect();
    let keep: Vec<C> = choices.keep.iter().map(to_f).collect();
    let drop: Vec<C> = choices.drop.iter().map(to_f).collect();

    // Bins in the order they were first seen, as a JS Map iterates.
    let mut slot = vec![u32::MAX; 1 << 15];
    let mut bins: Vec<[f64; 4]> = Vec::new();
    let flat = |i: usize, k: usize| {
        (d[i] as i32 - d[k] as i32).abs()
            + (d[i + 1] as i32 - d[k + 1] as i32).abs()
            + (d[i + 2] as i32 - d[k + 2] as i32).abs()
            + (d[i + 3] as i32 - d[k + 3] as i32).abs()
            < 12
    };
    // Only solid areas count: a pixel matching its right and lower neighbours.
    // Blended edge pixels between two colours never become colours of their own.
    for y in 0..r.h.saturating_sub(1) {
        for x in 0..w.saturating_sub(1) {
            let i = (y * w + x) * 4;
            if d[i + 3] < 250 || !flat(i, i + 4) || !flat(i, i + w * 4) {
                continue;
            }
            let key = ((d[i] >> 3) as usize) << 10 | ((d[i + 1] >> 3) as usize) << 5 | (d[i + 2] >> 3) as usize;
            if slot[key] == u32::MAX {
                slot[key] = bins.len() as u32;
                bins.push([0.0; 4]);
            }
            let b = &mut bins[slot[key] as usize];
            b[0] += d[i] as f64;
            b[1] += d[i + 1] as f64;
            b[2] += d[i + 2] as f64;
            b[3] += 1.0;
        }
    }
    let snap = |c: &C| -> C {
        let mut c = [js_round(c[0]), js_round(c[1]), js_round(c[2])];
        if !declared.is_empty() {
            let mut near = &declared[0];
            for b in &declared[1..] {
                if dist2(b, &c) < dist2(near, &c) {
                    near = b;
                }
            }
            if dist2(near, &c) <= 30.0 * 30.0 {
                c = *near;
            }
        }
        c
    };
    let all: Vec<(C, f64)> = bins.iter().map(|b| ([b[0] / b[3], b[1] / b[3], b[2] / b[3]], b[3])).collect();
    // A fixed floor rather than a share of the image: a big background would
    // otherwise push a small, distinct colour (an eye) under the bar.
    let mut buckets: Vec<&(C, f64)> = all.iter().filter(|b| b.1 >= 12.0).collect();
    buckets.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    let m2 = (merge * merge).max(64.0);
    let mut cand: Vec<(C, f64)> = Vec::new();
    for b in buckets {
        match cand.iter_mut().find(|p| dist2(&p.0, &b.0) <= m2) {
            Some(hit) => hit.1 += b.1,
            None => cand.push(*b),
        }
        if cand.len() >= 64 {
            break;
        }
    }
    let picked = |c: &C, list: &[C]| list.iter().position(|q| dist2(q, c) <= SAME);
    let mut removed: Vec<(C, f64)> = Vec::new();
    let mut pal: Vec<Entry> = Vec::new();
    for c in &cand {
        let shown = snap(&c.0);
        if picked(&shown, &drop).is_some() {
            match removed.iter_mut().find(|q| dist2(&q.0, &shown) <= SAME) {
                Some(r0) => r0.1 += c.1,
                None => removed.push((shown, c.1)),
            }
        } else {
            match picked(&shown, &keep) {
                // Shades of one locked colour share its entry rather than each
                // taking a slot of their own.
                Some(k) => match pal.iter_mut().find(|p| p.pin && dist2(&p.c, &keep[k]) == 0.0) {
                    Some(p) => p.n += c.1,
                    None => pal.push(Entry { c: keep[k], n: c.1, pin: true }),
                },
                None => pal.push(Entry { c: c.0, n: c.1, pin: false }),
            }
        }
    }
    for k in &keep {
        if !pal.iter().any(|p| p.pin && dist2(&p.c, k) == 0.0) {
            pal.push(Entry { c: *k, n: 1.0, pin: true });
        }
    }
    // Fold y into x. A kept colour always wins and never moves.
    let fold = |x: &mut Entry, y: &Entry| {
        let n = x.n + y.n;
        if y.pin && !x.pin {
            x.c = y.c;
        } else if !x.pin {
            for k in 0..3 {
                x.c[k] = (x.c[k] * x.n + y.c[k] * y.n) / n;
            }
        }
        x.n = n;
        x.pin = x.pin || y.pin;
    };
    // Too many colours (a gradient, say): merge the closest pair until it fits.
    // Small, distinct colours such as an eye survive because nothing is near them.
    while pal.len() > max_col {
        let (mut bi, mut bj, mut best) = (usize::MAX, 0, f64::INFINITY);
        for i in 0..pal.len() {
            for j in i + 1..pal.len() {
                if pal[i].pin && pal[j].pin {
                    continue;
                }
                let e = dist2(&pal[i].c, &pal[j].c) * pal[i].n.min(pal[j].n);
                if e < best {
                    best = e;
                    bi = i;
                    bj = j;
                }
            }
        }
        if bi == usize::MAX {
            break;
        }
        let y = pal.remove(bj);
        fold(&mut pal[bi], &y);
    }
    // A few rounds of k-means over the solid colours to settle each one on its cluster.
    let mut it = 0;
    while it < 4 && pal.len() > 1 {
        let mut acc = vec![[0f64; 4]; pal.len()];
        for b in &all {
            let (mut bi, mut best) = (0, f64::INFINITY);
            for (i, p) in pal.iter().enumerate() {
                let e = dist2(&p.c, &b.0);
                if e < best {
                    best = e;
                    bi = i;
                }
            }
            let t = &mut acc[bi];
            t[0] += b.0[0] * b.1;
            t[1] += b.0[1] * b.1;
            t[2] += b.0[2] * b.1;
            t[3] += b.1;
        }
        for (p, t) in pal.iter_mut().zip(&acc) {
            if t[3] != 0.0 {
                p.n = t[3];
                if !p.pin {
                    p.c = [t[0] / t[3], t[1] / t[3], t[2] / t[3]];
                }
            }
        }
        it += 1;
    }
    // k-means can walk two centres towards each other; fold any that ended up closer than the merge distance.
    'again: loop {
        for i in 0..pal.len() {
            for j in i + 1..pal.len() {
                if (pal[i].pin && pal[j].pin) || dist2(&pal[i].c, &pal[j].c) > m2 {
                    continue;
                }
                let y = pal.remove(j);
                fold(&mut pal[i], &y);
                continue 'again;
            }
        }
        break;
    }
    let cols: Vec<C> = pal.iter().map(|p| if p.pin { p.c } else { snap(&p.c) }).collect();
    // Candidates that no longer have a colour of their own were merged away.
    let mut merged: Vec<(C, f64)> = Vec::new();
    for c in &cand {
        let shown = snap(&c.0);
        if picked(&shown, &drop).is_some() || cols.iter().any(|q| dist2(q, &shown) <= SAME) {
            continue;
        }
        match merged.iter_mut().find(|q| dist2(&q.0, &shown) <= SAME) {
            Some(m0) => m0.1 += c.1,
            None => merged.push((shown, c.1)),
        }
    }
    Found {
        cols,
        pinned: pal.iter().map(|p| p.pin).collect(),
        merged,
        removed,
        cand: cand.iter().map(|c| (snap(&c.0), c.1)).collect(),
    }
}

/// The colour that owns most of the border, if it owns enough of it to be a background.
fn edge_colour(r: &Image, pal: &[Rgb]) -> i32 {
    let idx = classify(r, pal, -2, false);
    let mut counts: Vec<(i16, usize)> = Vec::new();
    let mut add = |j: usize| {
        let k = idx[j];
        if k >= 0 {
            match counts.iter_mut().find(|c| c.0 == k) {
                Some(c) => c.1 += 1,
                None => counts.push((k, 1)),
            }
        }
    };
    for x in 0..r.w {
        add(x);
        add((r.h - 1) * r.w + x);
    }
    for y in 0..r.h {
        add(y * r.w);
        add(y * r.w + r.w - 1);
    }
    let (mut best, mut n) = (-1, 0);
    for &(k, c) in &counts {
        if c > n {
            best = k as i32;
            n = c;
        }
    }
    if n > r.w + r.h {
        best
    } else {
        -1
    }
}

fn to_rgb(c: &C) -> Rgb {
    [c[0] as i32, c[1] as i32, c[2] as i32]
}

/// Palette, background and layer order for colour mode, from the original drawn
/// at `CMP` on a transparent background. `declared` holds the colours an SVG source
/// declares, which the palette snaps to.
pub fn palette_for(r: &Image, o: &Opts, declared: &[Rgb]) -> Result<Pal, String> {
    let found = find_palette(r, o.merge, declared, o.max_col, &o.choices);
    let pal: Vec<Rgb> = found.cols.iter().map(to_rgb).collect();
    if pal.is_empty() {
        return Err("Every colour is left out. Click a faded colour in the palette to bring it back.".into());
    }
    let bg = if o.edge_bg { edge_colour(r, &pal) } else { -1 };
    let idx = classify(r, &pal, bg, o.edges);
    let mut area = vec![0usize; pal.len()];
    for &k in &idx {
        if k >= 0 {
            area[k as usize] += 1;
        }
    }
    let mut order: Vec<usize> = (0..pal.len()).filter(|&i| area[i] > 0).collect();
    order.sort_by(|&a, &b| area[b].cmp(&area[a]));
    // Shares of the artwork. In-use colours are measured from the traced pixels;
    // merged and removed ones from their solid pixels, so those are estimates.
    let art = match area.iter().sum::<usize>() {
        0 => 1,
        s => s,
    } as f64;
    let bgc = if bg >= 0 { Some(to_f(&pal[bg as usize])) } else { None };
    let solid: f64 = found.cand.iter().filter(|c| !bgc.is_some_and(|b| dist2(&c.0, &b) <= SAME)).map(|c| c.1).sum();
    let solid = if solid == 0.0 { 1.0 } else { solid };
    let est = |list: &[(C, f64)]| list.iter().map(|m| Share { c: to_rgb(&m.0), share: m.1 / solid }).collect();
    Ok(Pal {
        share: area.iter().map(|&a| a as f64 / art).collect(),
        pal,
        bg,
        order,
        pinned: found.pinned,
        merged: est(&found.merged),
        removed: est(&found.removed),
    })
}
