//! Quantizing: every pixel becomes a palette index, or -1 for background.

use crate::palette::Pal;
use crate::{js_round, speck_area, Image, Mode, Opts, Rgb};

/// One-colour: dark pixels (over white) are the shape.
pub fn mono_index(r: &Image, thr: f64) -> Vec<i16> {
    r.data
        .chunks_exact(4)
        .map(|d| if (0.2126 * d[0] as f64 + 0.7152 * d[1] as f64 + 0.0722 * d[2] as f64) < thr { 0 } else { -1 })
        .collect()
}

fn nearest(pal: &[Rgb], r: i32, g: i32, b: i32) -> i16 {
    let (mut best, mut k) = (i32::MAX, 0);
    for (p, c) in pal.iter().enumerate() {
        let e = (c[0] - r).pow(2) + (c[1] - g).pow(2) + (c[2] - b).pow(2);
        if e < best {
            best = e;
            k = p;
        }
    }
    k as i16
}

/// Map every pixel to its nearest palette colour. The background colour becomes
/// -1 only where it reaches the border, so the same white inside the artwork stays.
/// `bg` below -1 means no background at all.
pub fn classify(r: &Image, pal: &[Rgb], bg: i32, edges: bool) -> Vec<i16> {
    let (w, n) = (r.w, r.w * r.h);
    let mut idx = vec![0i16; n];
    // Runs of one colour are common, so remember the last lookup.
    let (mut last_key, mut last_k) = (u32::MAX, 0i16);
    for (j, d) in r.data.chunks_exact(4).enumerate() {
        if d[3] < 128 {
            idx[j] = -1;
            continue;
        }
        let key = (d[0] as u32) << 16 | (d[1] as u32) << 8 | d[2] as u32;
        if key != last_key {
            last_key = key;
            last_k = nearest(pal, d[0] as i32, d[1] as i32, d[2] as i32);
        }
        idx[j] = last_k;
    }
    if edges {
        snap_edges(r, pal, &mut idx);
    }
    if bg >= 0 {
        let bg = bg as i16;
        let mut q = Vec::with_capacity(n / 4);
        let push = |idx: &mut [i16], q: &mut Vec<usize>, j: usize| {
            if idx[j] == bg {
                idx[j] = -1;
                q.push(j);
            }
        };
        for x in 0..w {
            push(&mut idx, &mut q, x);
            push(&mut idx, &mut q, n - w + x);
        }
        for y in 0..r.h {
            push(&mut idx, &mut q, y * w);
            push(&mut idx, &mut q, y * w + w - 1);
        }
        let mut head = 0;
        while head < q.len() {
            let j = q[head];
            head += 1;
            let x = j % w;
            if x > 0 {
                push(&mut idx, &mut q, j - 1);
            }
            if x < w - 1 {
                push(&mut idx, &mut q, j + 1);
            }
            if j >= w {
                push(&mut idx, &mut q, j - w);
            }
            if j < n - w {
                push(&mut idx, &mut q, j + w);
            }
        }
    }
    idx
}

/// A blended pixel on an edge between two colours can sit nearer a third palette
/// colour (dark blue + white blends to something close to a pale grey), which
/// leaves a one-pixel fringe of that colour along the edge. So each pixel that
/// isn't solid looks outwards in eight directions for the nearest solid pixels,
/// and may only take one of the colours it finds there.
fn snap_edges(r: &Image, pal: &[Rgb], idx: &mut [i16]) {
    let (d, w, h) = (r.data, r.w, r.h);
    let long = w.max(h) as f64;
    let reach = (js_round(2.0 * long / 2048.0) as i64).max(2);
    // Compare across a step that grows with the image: an upscaled soft edge
    // changes too little from one pixel to the next to be noticed otherwise.
    let st = (js_round(long / 2048.0) as usize).max(1);
    let near = |i: usize, k: usize| {
        (d[i] as i32 - d[k] as i32).abs()
            + (d[i + 1] as i32 - d[k + 1] as i32).abs()
            + (d[i + 2] as i32 - d[k + 2] as i32).abs()
            + (d[i + 3] as i32 - d[k + 3] as i32).abs()
            < 24
    };
    let mut solid = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            let j = y * w + x;
            let i = j * 4;
            solid[j] = (x < st || near(i, i - 4 * st))
                && (x + st >= w || near(i, i + 4 * st))
                && (y < st || near(i, i - w * 4 * st))
                && (y + st >= h || near(i, i + w * 4 * st));
        }
    }
    const DX: [i64; 8] = [1, -1, 0, 0, 1, 1, -1, -1];
    const DY: [i64; 8] = [0, 0, 1, -1, 1, -1, 1, -1];
    let mut changes = Vec::new();
    let mut cands: Vec<i16> = Vec::with_capacity(8);
    for y in 0..h {
        for x in 0..w {
            let j = y * w + x;
            let own = idx[j];
            if solid[j] || own < 0 {
                continue;
            }
            cands.clear();
            for k in 0..8 {
                for s in 1..=reach {
                    let (xx, yy) = (x as i64 + DX[k] * s, y as i64 + DY[k] * s);
                    if xx < 0 || yy < 0 || xx >= w as i64 || yy >= h as i64 {
                        break;
                    }
                    let q = yy as usize * w + xx as usize;
                    if solid[q] {
                        let c = idx[q];
                        if c >= 0 && !cands.contains(&c) {
                            cands.push(c);
                        }
                        break;
                    }
                }
            }
            if cands.is_empty() || cands.contains(&own) {
                continue;
            }
            let i = j * 4;
            let e = |c: i16| {
                let p = pal[c as usize];
                (p[0] - d[i] as i32).pow(2) + (p[1] - d[i + 1] as i32).pow(2) + (p[2] - d[i + 2] as i32).pow(2)
            };
            if e(own) <= 20 * 20 {
                continue; // it really is that colour: a thin line, not a blend
            }
            let mut best = cands[0];
            for &c in &cands {
                if e(c) < e(best) {
                    best = c;
                }
            }
            changes.push((j, best));
        }
    }
    for (j, c) in changes {
        idx[j] = c;
    }
}

/// Every patch of one colour smaller than `min_area` takes the colour that surrounds
/// it most. Status per pixel: 0 unseen, 1 settled small, 2 part of a big patch,
/// 3 in the patch being grown. A patch that grows past `min_area`, or touches a
/// pixel already known to be in a big patch, stops early, so the work stays linear.
/// With `slivers`, long thin patches stay: in pen and ink those are the gaps
/// between strokes, and filling them darkens the drawing.
pub fn remove_specks(idx: &mut [i16], w: usize, h: usize, min_area: usize, slivers: bool) {
    if min_area < 2 {
        return;
    }
    let n = w * h;
    let mut status = vec![0u8; n];
    let mut q = vec![0usize; min_area + 1];
    let mut votes: Vec<(i16, u32)> = Vec::new();
    let nbs = |j: usize| {
        let x = j % w;
        [
            if x > 0 { Some(j - 1) } else { None },
            if x < w - 1 { Some(j + 1) } else { None },
            if j >= w { Some(j - w) } else { None },
            if j < n - w { Some(j + w) } else { None },
        ]
    };
    for j0 in 0..n {
        if status[j0] != 0 {
            continue;
        }
        let c = idx[j0];
        let (mut head, mut tail, mut big) = (0, 0, false);
        q[tail] = j0;
        tail += 1;
        status[j0] = 3;
        'grow: while head < tail {
            let j = q[head];
            head += 1;
            for k in nbs(j).into_iter().flatten() {
                if idx[k] != c {
                    continue;
                }
                if status[k] == 2 {
                    big = true;
                    break 'grow;
                }
                if status[k] == 0 {
                    if tail >= min_area {
                        big = true;
                        break 'grow;
                    }
                    status[k] = 3;
                    q[tail] = k;
                    tail += 1;
                }
            }
        }
        if big {
            for &j in &q[..tail] {
                status[j] = 2;
            }
            continue;
        }
        if slivers && thin(&q[..tail], w) {
            for &j in &q[..tail] {
                status[j] = 1;
            }
            continue;
        }
        votes.clear();
        for &j in &q[..tail] {
            for k in nbs(j).into_iter().flatten() {
                if idx[k] != c {
                    match votes.iter_mut().find(|v| v.0 == idx[k]) {
                        Some(v) => v.1 += 1,
                        None => votes.push((idx[k], 1)),
                    }
                }
            }
        }
        let (mut to, mut most) = (c, 0);
        for &(v, m) in &votes {
            if m > most {
                most = m;
                to = v;
            }
        }
        for &j in &q[..tail] {
            idx[j] = to;
            status[j] = 1;
        }
    }
}

/// A patch at least 4 px long whose bounding box is mostly empty: its longer
/// side squared is at least three times its area. A round dot never is.
fn thin(patch: &[usize], w: usize) -> bool {
    let (mut x0, mut x1, mut y0, mut y1) = (usize::MAX, 0, usize::MAX, 0);
    for &j in patch {
        let (x, y) = (j % w, j / w);
        x0 = x0.min(x);
        x1 = x1.max(x);
        y0 = y0.min(y);
        y1 = y1.max(y);
    }
    let extent = (x1 - x0 + 1).max(y1 - y0 + 1);
    extent >= 4 && extent * extent >= 3 * patch.len()
}

/// Majority filter: each pixel takes the most common colour of its 3x3
/// neighbourhood. Clears the speckle and ragged edges of noisy or upscaled images.
pub fn clean_up(idx: Vec<i16>, w: usize, h: usize, passes: u32, n_col: usize) -> Vec<i16> {
    let mut idx = idx;
    let mut counts = vec![0i32; n_col + 1];
    for _ in 0..passes {
        let mut out = vec![0i16; idx.len()];
        for y in 0..h {
            for x in 0..w {
                let j = y * w + x;
                if x == 0 || y == 0 || x == w - 1 || y == h - 1 {
                    out[j] = idx[j];
                    continue;
                }
                let (mut best, mut bn) = (idx[j], 0);
                for yy in 0..3 {
                    for xx in 0..3 {
                        let v = idx[j + yy * w + xx - w - 1];
                        let slot = (v + 1) as usize;
                        counts[slot] += 1;
                        let c = counts[slot];
                        if c > bn || (c == bn && v == idx[j]) {
                            bn = c;
                            best = v;
                        }
                    }
                }
                for yy in 0..3 {
                    for xx in 0..3 {
                        counts[(idx[j + yy * w + xx - w - 1] + 1) as usize] = 0;
                    }
                }
                out[j] = best;
            }
        }
        idx = out;
    }
    idx
}

/// The index map the tracer starts from: nearest colour, background flood,
/// clean-up and speck removal. Also the quantized image.
pub fn index_map(r: &Image, o: &Opts, p: &Pal) -> Vec<i16> {
    let mut idx = match o.mode {
        Mode::Mono => mono_index(r, o.thr),
        Mode::Colour => classify(r, &p.pal, p.bg, o.edges),
    };
    if o.clean > 0 {
        idx = clean_up(idx, r.w, r.h, o.clean, p.pal.len());
    }
    remove_specks(&mut idx, r.w, r.h, speck_area(o, r.w, r.h), o.slivers);
    idx
}

/// The original as the comparison sees it: nearest colour, no clean-up.
/// For mono, `r` must be drawn over white.
pub fn source_index(r: &Image, o: &Opts, p: &Pal) -> Vec<i16> {
    match o.mode {
        Mode::Mono => mono_index(r, o.thr),
        Mode::Colour => classify(r, &p.pal, p.bg, o.edges),
    }
}
