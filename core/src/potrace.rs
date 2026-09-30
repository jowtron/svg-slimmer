//! Potrace, ported from kilobtye's JavaScript port (potrace-browser 0.0.1), which
//! the web app used before this crate. Copyright (C) 2001-2013 Peter Selinger.
//! Licensed under the GPL.
//!
//! The port is deliberately literal: the same steps, the same floating-point
//! operations in the same order, and the JS port's quirks where they change the
//! result. That keeps the output identical to the JS version, coordinate for
//! coordinate. Fixed settings: turn policy "minority", curve optimisation on.

#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct P {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug)]
pub enum Seg {
    /// A cubic Bézier: two control points, then the end point.
    Curve([P; 3]),
    /// A corner: straight to the corner point, then straight to the end point.
    Corner([P; 2]),
}

impl Seg {
    pub fn end(&self) -> P {
        match self {
            Seg::Curve(c) => c[2],
            Seg::Corner(c) => c[1],
        }
    }
}

/// One closed path. It starts at the end point of its last segment.
pub type Curve = Vec<Seg>;

pub struct Params {
    pub turdsize: i64,
    pub alphamax: f64,
    pub opttolerance: f64,
}

impl Default for Params {
    fn default() -> Self {
        Params { turdsize: 2, alphamax: 1.0, opttolerance: 0.2 }
    }
}

/// Trace a bitmap (`bits[y * w + x]` is 1 for ink) into closed curves.
pub fn trace(w: usize, h: usize, bits: &[u8], params: &Params) -> Vec<Curve> {
    trace_tolerances(w, h, bits, params, &[params.opttolerance]).pop().unwrap()
}

/// Trace once per curve tolerance. Everything before curve optimisation is the
/// same whatever the tolerance, so it runs once and only the last step repeats.
pub fn trace_tolerances(w: usize, h: usize, bits: &[u8], params: &Params, tols: &[f64]) -> Vec<Vec<Curve>> {
    let smoothed: Vec<Smoothed> =
        bm_to_pathlist(w as i64, h as i64, bits, params.turdsize).into_iter().map(|p| process_path(p, params)).collect();
    tols.iter().map(|&tol| smoothed.iter().map(|s| opti_curve(s, tol)).collect()).collect()
}

// ---------- decomposition into paths ----------

struct Path {
    pt: Vec<(i64, i64)>,
    area: i64,
    max_x: i64,
    plus: bool,
}

fn bm_to_pathlist(w: i64, h: i64, bm: &[u8], turdsize: i64) -> Vec<Path> {
    let mut bm1 = bm.to_vec();
    let at = |b: &[u8], x: i64, y: i64| x >= 0 && x < w && y >= 0 && y < h && b[(w * y + x) as usize] == 1;
    let size = (w * h) as usize;
    let mut pathlist = Vec::new();
    let mut i = 0usize;
    loop {
        while i < size && bm1[i] != 1 {
            i += 1;
        }
        if i >= size {
            break;
        }
        let (x0, y0) = (i as i64 % w, i as i64 / w);

        // findPath
        let mut path = Path { pt: Vec::new(), area: 0, max_x: -1, plus: at(bm, x0, y0) };
        let (mut x, mut y, mut dirx, mut diry) = (x0, y0, 0i64, 1i64);
        loop {
            path.pt.push((x, y));
            if x > path.max_x {
                path.max_x = x;
            }
            x += dirx;
            y += diry;
            path.area -= x * diry;
            if x == x0 && y == y0 {
                break;
            }
            let l = at(&bm1, x + (dirx + diry - 1) / 2, y + (diry - dirx - 1) / 2);
            let r = at(&bm1, x + (dirx - diry - 1) / 2, y + (diry + dirx - 1) / 2);
            if r && !l {
                // turn policy "minority"
                if !majority(&bm1, w, h, x, y) {
                    let t = dirx;
                    dirx = -diry;
                    diry = t;
                } else {
                    let t = dirx;
                    dirx = diry;
                    diry = -t;
                }
            } else if r {
                let t = dirx;
                dirx = -diry;
                diry = t;
            } else if !l {
                let t = dirx;
                dirx = diry;
                diry = -t;
            }
        }

        // xorPath
        let mut y1 = path.pt[0].1;
        for &(px, py) in &path.pt[1..] {
            if py != y1 {
                let min_y = y1.min(py);
                for j in px..path.max_x {
                    let k = (w * min_y + j) as usize;
                    bm1[k] = if bm1[k] == 1 { 0 } else { 1 };
                }
                y1 = py;
            }
        }

        if path.area > turdsize {
            pathlist.push(path);
        }
    }
    pathlist
}

fn majority(bm1: &[u8], w: i64, h: i64, x: i64, y: i64) -> bool {
    let at = |x: i64, y: i64| x >= 0 && x < w && y >= 0 && y < h && bm1[(w * y + x) as usize] == 1;
    for i in 2..5 {
        let mut ct = 0;
        for a in (-i + 1)..=(i - 1) {
            ct += if at(x + a, y + i - 1) { 1 } else { -1 };
            ct += if at(x + i - 1, y + a - 1) { 1 } else { -1 };
            ct += if at(x + a - 1, y - i) { 1 } else { -1 };
            ct += if at(x - i, y + a) { 1 } else { -1 };
        }
        if ct > 0 {
            return true;
        } else if ct < 0 {
            return false;
        }
    }
    false
}

// ---------- geometry helpers ----------

fn modn(a: i64, n: i64) -> i64 {
    if a >= n {
        a % n
    } else if a >= 0 {
        a
    } else {
        n - 1 - (-1 - a) % n
    }
}

fn cyclic(a: i64, b: i64, c: i64) -> bool {
    if a <= c {
        a <= b && b < c
    } else {
        a <= b || b < c
    }
}

fn sign_i(i: i64) -> i64 {
    if i > 0 {
        1
    } else if i < 0 {
        -1
    } else {
        0
    }
}

fn sign(v: f64) -> f64 {
    if v > 0.0 {
        1.0
    } else if v < 0.0 {
        -1.0
    } else {
        0.0
    }
}

fn xprod(p1: (i64, i64), p2: (i64, i64)) -> i64 {
    p1.0 * p2.1 - p1.1 * p2.0
}

fn interval(lambda: f64, a: P, b: P) -> P {
    P { x: a.x + lambda * (b.x - a.x), y: a.y + lambda * (b.y - a.y) }
}

fn ddenom(p0: P, p2: P) -> f64 {
    let ry = sign(p2.x - p0.x);
    let rx = -sign(p2.y - p0.y);
    ry * (p2.x - p0.x) - rx * (p2.y - p0.y)
}

fn dpara(p0: P, p1: P, p2: P) -> f64 {
    let x1 = p1.x - p0.x;
    let y1 = p1.y - p0.y;
    let x2 = p2.x - p0.x;
    let y2 = p2.y - p0.y;
    x1 * y2 - x2 * y1
}

fn cprod(p0: P, p1: P, p2: P, p3: P) -> f64 {
    let x1 = p1.x - p0.x;
    let y1 = p1.y - p0.y;
    let x2 = p3.x - p2.x;
    let y2 = p3.y - p2.y;
    x1 * y2 - x2 * y1
}

fn iprod(p0: P, p1: P, p2: P) -> f64 {
    let x1 = p1.x - p0.x;
    let y1 = p1.y - p0.y;
    let x2 = p2.x - p0.x;
    let y2 = p2.y - p0.y;
    x1 * x2 + y1 * y2
}

fn iprod1(p0: P, p1: P, p2: P, p3: P) -> f64 {
    let x1 = p1.x - p0.x;
    let y1 = p1.y - p0.y;
    let x2 = p3.x - p2.x;
    let y2 = p3.y - p2.y;
    x1 * x2 + y1 * y2
}

fn ddist(p: P, q: P) -> f64 {
    ((p.x - q.x) * (p.x - q.x) + (p.y - q.y) * (p.y - q.y)).sqrt()
}

fn bezier(t: f64, p0: P, p1: P, p2: P, p3: P) -> P {
    let s = 1.0 - t;
    P {
        x: s * s * s * p0.x + 3.0 * (s * s * t) * p1.x + 3.0 * (t * t * s) * p2.x + t * t * t * p3.x,
        y: s * s * s * p0.y + 3.0 * (s * s * t) * p1.y + 3.0 * (t * t * s) * p2.y + t * t * t * p3.y,
    }
}

fn tangent(p0: P, p1: P, p2: P, p3: P, q0: P, q1: P) -> f64 {
    let a_ = cprod(p0, p1, q0, q1);
    let b_ = cprod(p1, p2, q0, q1);
    let c_ = cprod(p2, p3, q0, q1);
    let a = a_ - 2.0 * b_ + c_;
    let b = -2.0 * a_ + 2.0 * b_;
    let c = a_;
    let d = b * b - 4.0 * a * c;
    if a == 0.0 || d < 0.0 {
        return -1.0;
    }
    let s = d.sqrt();
    let r1 = (-b + s) / (2.0 * a);
    let r2 = (-b - s) / (2.0 * a);
    if (0.0..=1.0).contains(&r1) {
        r1
    } else if (0.0..=1.0).contains(&r2) {
        r2
    } else {
        -1.0
    }
}

// ---------- per-path processing ----------

#[derive(Clone, Copy, Default)]
struct Sum {
    x: f64,
    y: f64,
    xy: f64,
    x2: f64,
    y2: f64,
}

#[derive(Clone, Copy, PartialEq)]
enum Tag {
    Curve,
    Corner,
}

struct Smoothed {
    tag: Vec<Tag>,
    c: Vec<[P; 3]>,
    vertex: Vec<P>,
    alpha: Vec<f64>,
}

fn process_path(path: Path, params: &Params) -> Smoothed {
    let n = path.pt.len() as i64;
    let pt = &path.pt;

    // calcSums
    let (x0, y0) = pt[0];
    let mut sums = Vec::with_capacity(n as usize + 1);
    sums.push(Sum::default());
    for (i, &(px, py)) in pt.iter().enumerate() {
        let x = (px - x0) as f64;
        let y = (py - y0) as f64;
        let s = sums[i];
        sums.push(Sum { x: s.x + x, y: s.y + y, xy: s.xy + x * y, x2: s.x2 + x * x, y2: s.y2 + y * y });
    }

    let lon = calc_lon(pt);
    let po = best_polygon(pt, &sums, &lon);
    let mut vertex = adjust_vertices(pt, &sums, &po, x0, y0);
    if !path.plus {
        vertex.reverse();
    }
    smooth(vertex, params.alphamax)
}

fn calc_lon(pt: &[(i64, i64)]) -> Vec<i64> {
    let n = pt.len() as i64;
    let p = |i: i64| pt[i as usize];
    let mut pivk = vec![0i64; n as usize];
    let mut nc = vec![0i64; n as usize];
    let mut lon = vec![0i64; n as usize];

    let mut k = 0i64;
    for i in (0..n).rev() {
        if p(i).0 != p(k).0 && p(i).1 != p(k).1 {
            k = i + 1;
        }
        nc[i as usize] = k;
    }

    for i in (0..n).rev() {
        let mut ct = [0i64; 4];
        let nx = p(modn(i + 1, n));
        // (3 + 3*dx + dy) / 2. In the JS port an odd numerator gives a fractional
        // index, which counts nothing, so only even numerators count here.
        let bump = |ct: &mut [i64; 4], num: i64| {
            if num % 2 == 0 && (0..8).contains(&num) {
                ct[(num / 2) as usize] += 1;
            }
        };
        bump(&mut ct, 3 + 3 * (nx.0 - p(i).0) + (nx.1 - p(i).1));

        let mut constraint = [(0i64, 0i64), (0i64, 0i64)];
        let mut k = nc[i as usize];
        let mut k1 = i;
        let mut foundk = false;
        loop {
            bump(&mut ct, 3 + 3 * sign_i(p(k).0 - p(k1).0) + sign_i(p(k).1 - p(k1).1));
            if ct[0] != 0 && ct[1] != 0 && ct[2] != 0 && ct[3] != 0 {
                pivk[i as usize] = k1;
                foundk = true;
                break;
            }
            let cur = (p(k).0 - p(i).0, p(k).1 - p(i).1);
            if xprod(constraint[0], cur) < 0 || xprod(constraint[1], cur) > 0 {
                break;
            }
            if !(cur.0.abs() <= 1 && cur.1.abs() <= 1) {
                let off = (
                    cur.0 + if cur.1 >= 0 && (cur.1 > 0 || cur.0 < 0) { 1 } else { -1 },
                    cur.1 + if cur.0 <= 0 && (cur.0 < 0 || cur.1 < 0) { 1 } else { -1 },
                );
                if xprod(constraint[0], off) >= 0 {
                    constraint[0] = off;
                }
                let off = (
                    cur.0 + if cur.1 <= 0 && (cur.1 < 0 || cur.0 < 0) { 1 } else { -1 },
                    cur.1 + if cur.0 >= 0 && (cur.0 > 0 || cur.1 < 0) { 1 } else { -1 },
                );
                if xprod(constraint[1], off) <= 0 {
                    constraint[1] = off;
                }
            }
            k1 = k;
            k = nc[k1 as usize];
            if !cyclic(k, i, k1) {
                break;
            }
        }
        if !foundk {
            let dk = (sign_i(p(k).0 - p(k1).0), sign_i(p(k).1 - p(k1).1));
            let cur = (p(k1).0 - p(i).0, p(k1).1 - p(i).1);
            let a = xprod(constraint[0], cur);
            let b = xprod(constraint[0], dk);
            let c = xprod(constraint[1], cur);
            let d = xprod(constraint[1], dk);
            let mut j = 10_000_000i64;
            if b < 0 {
                j = a.div_euclid(-b);
            }
            if d > 0 {
                j = j.min((-c).div_euclid(d));
            }
            pivk[i as usize] = modn(k1 + j, n);
        }
    }

    let mut j = pivk[(n - 1) as usize];
    lon[(n - 1) as usize] = j;
    for i in (0..=n - 2).rev() {
        if cyclic(i + 1, pivk[i as usize], j) {
            j = pivk[i as usize];
        }
        lon[i as usize] = j;
    }
    let mut i = n - 1;
    while i >= 0 && cyclic(modn(i + 1, n), j, lon[i as usize]) {
        lon[i as usize] = j;
        i -= 1;
    }
    lon
}

fn best_polygon(pt: &[(i64, i64)], sums: &[Sum], lon: &[i64]) -> Vec<i64> {
    let n = pt.len() as i64;
    let nu = n as usize;

    let penalty3 = |i: i64, j: i64| -> f64 {
        let (mut j, mut r) = (j, 0);
        if j >= n {
            j -= n;
            r = 1;
        }
        let (iu, ju) = (i as usize, j as usize);
        let (x, y, x2, xy, y2, k);
        if r == 0 {
            x = sums[ju + 1].x - sums[iu].x;
            y = sums[ju + 1].y - sums[iu].y;
            x2 = sums[ju + 1].x2 - sums[iu].x2;
            xy = sums[ju + 1].xy - sums[iu].xy;
            y2 = sums[ju + 1].y2 - sums[iu].y2;
            k = (j + 1 - i) as f64;
        } else {
            x = sums[ju + 1].x - sums[iu].x + sums[nu].x;
            y = sums[ju + 1].y - sums[iu].y + sums[nu].y;
            x2 = sums[ju + 1].x2 - sums[iu].x2 + sums[nu].x2;
            xy = sums[ju + 1].xy - sums[iu].xy + sums[nu].xy;
            y2 = sums[ju + 1].y2 - sums[iu].y2 + sums[nu].y2;
            k = (j + 1 - i + n) as f64;
        }
        let px = (pt[iu].0 + pt[ju].0) as f64 / 2.0 - pt[0].0 as f64;
        let py = (pt[iu].1 + pt[ju].1) as f64 / 2.0 - pt[0].1 as f64;
        let ey = (pt[ju].0 - pt[iu].0) as f64;
        let ex = -((pt[ju].1 - pt[iu].1) as f64);
        let a = (x2 - 2.0 * x * px) / k + px * px;
        let b = (xy - x * py - y * px) / k + px * py;
        let c = (y2 - 2.0 * y * py) / k + py * py;
        let s = ex * ex * a + 2.0 * ex * ey * b + ey * ey * c;
        s.sqrt()
    };

    let mut pen = vec![0f64; nu + 1];
    let mut prev = vec![0i64; nu + 1];
    let mut clip0 = vec![0i64; nu];
    let mut clip1 = vec![0i64; nu + 1];
    let mut seg0 = vec![0i64; nu + 1];
    let mut seg1 = vec![0i64; nu + 1];

    for i in 0..n {
        let mut c = modn(lon[modn(i - 1, n) as usize] - 1, n);
        if c == i {
            c = modn(i + 1, n);
        }
        clip0[i as usize] = if c < i { n } else { c };
    }
    let mut j = 1i64;
    for i in 0..n {
        while j <= clip0[i as usize] {
            clip1[j as usize] = i;
            j += 1;
        }
    }
    let mut i = 0i64;
    j = 0;
    while i < n {
        seg0[j as usize] = i;
        i = clip0[i as usize];
        j += 1;
    }
    seg0[j as usize] = n;
    let m = j;
    i = n;
    for j in (1..=m).rev() {
        seg1[j as usize] = i;
        i = clip1[i as usize];
    }
    seg1[0] = 0;

    pen[0] = 0.0;
    for j in 1..=m {
        for i in seg1[j as usize]..=seg0[j as usize] {
            let mut best = -1.0;
            let mut k = seg0[(j - 1) as usize];
            while k >= clip1[i as usize] {
                let thispen = penalty3(k, i) + pen[k as usize];
                if best < 0.0 || thispen < best {
                    prev[i as usize] = k;
                    best = thispen;
                }
                k -= 1;
            }
            pen[i as usize] = best;
        }
    }
    let mut po = vec![0i64; m as usize];
    let mut i = n;
    let mut j = m - 1;
    while i > 0 {
        i = prev[i as usize];
        po[j as usize] = i;
        j -= 1;
    }
    po
}

fn adjust_vertices(pt: &[(i64, i64)], sums: &[Sum], po: &[i64], x0: i64, y0: i64) -> Vec<P> {
    let n = pt.len() as i64;
    let m = po.len() as i64;
    let nu = n as usize;

    let pointslope = |i: i64, j: i64| -> (P, P) {
        let (mut i, mut j, mut r) = (i, j, 0i64);
        while j >= n {
            j -= n;
            r += 1;
        }
        while i >= n {
            i -= n;
            r -= 1;
        }
        while j < 0 {
            j += n;
            r -= 1;
        }
        while i < 0 {
            i += n;
            r += 1;
        }
        let (iu, ju, rf) = (i as usize, j as usize, r as f64);
        let x = sums[ju + 1].x - sums[iu].x + rf * sums[nu].x;
        let y = sums[ju + 1].y - sums[iu].y + rf * sums[nu].y;
        let x2 = sums[ju + 1].x2 - sums[iu].x2 + rf * sums[nu].x2;
        let xy = sums[ju + 1].xy - sums[iu].xy + rf * sums[nu].xy;
        let y2 = sums[ju + 1].y2 - sums[iu].y2 + rf * sums[nu].y2;
        let k = (j + 1 - i + r * n) as f64;
        let ctr = P { x: x / k, y: y / k };
        let mut a = (x2 - x * x / k) / k;
        let b = (xy - x * y / k) / k;
        let mut c = (y2 - y * y / k) / k;
        let lambda2 = (a + c + ((a - c) * (a - c) + 4.0 * b * b).sqrt()) / 2.0;
        a -= lambda2;
        c -= lambda2;
        let mut dir = P::default();
        let l;
        if a.abs() >= c.abs() {
            l = (a * a + b * b).sqrt();
            if l != 0.0 {
                dir = P { x: -b / l, y: a / l };
            }
        } else {
            l = (c * c + b * b).sqrt();
            if l != 0.0 {
                dir = P { x: -c / l, y: b / l };
            }
        }
        if l == 0.0 {
            dir = P { x: 0.0, y: 0.0 };
        }
        (ctr, dir)
    };

    let mut ctr = Vec::with_capacity(m as usize);
    let mut dir = Vec::with_capacity(m as usize);
    for i in 0..m {
        let j = po[modn(i + 1, m) as usize];
        let j = modn(j - po[i as usize], n) + po[i as usize];
        let (c, d) = pointslope(po[i as usize], j);
        ctr.push(c);
        dir.push(d);
    }

    let mut q = vec![[0f64; 9]; m as usize];
    for i in 0..m as usize {
        let d = dir[i].x * dir[i].x + dir[i].y * dir[i].y;
        if d != 0.0 {
            let v0 = dir[i].y;
            let v1 = -dir[i].x;
            let v = [v0, v1, -v1 * ctr[i].y - v0 * ctr[i].x];
            for l in 0..3 {
                for k in 0..3 {
                    q[i][l * 3 + k] = v[l] * v[k] / d;
                }
            }
        }
    }

    let quadform = |qm: &[f64; 9], w: P| -> f64 {
        let v = [w.x, w.y, 1.0];
        let mut sum = 0.0;
        for i in 0..3 {
            for j in 0..3 {
                sum += v[i] * qm[i * 3 + j] * v[j];
            }
        }
        sum
    };

    let mut vertex = Vec::with_capacity(m as usize);
    for i in 0..m {
        let s = P { x: (pt[po[i as usize] as usize].0 - x0) as f64, y: (pt[po[i as usize] as usize].1 - y0) as f64 };
        let j = modn(i - 1, m) as usize;
        let mut qm = [0f64; 9];
        for l in 0..9 {
            qm[l] = q[j][l] + q[i as usize][l];
        }
        let mut w;
        loop {
            let det = qm[0] * qm[4] - qm[1] * qm[3];
            if det != 0.0 {
                w = P { x: (-qm[2] * qm[4] + qm[5] * qm[1]) / det, y: (qm[2] * qm[3] - qm[5] * qm[0]) / det };
                break;
            }
            let (v0, v1) = if qm[0] > qm[4] {
                (-qm[1], qm[0])
            } else if qm[4] != 0.0 {
                (-qm[4], qm[3])
            } else {
                (1.0, 0.0)
            };
            let d = v0 * v0 + v1 * v1;
            let v = [v0, v1, -v1 * s.y - v0 * s.x];
            for l in 0..3 {
                for k in 0..3 {
                    qm[l * 3 + k] += v[l] * v[k] / d;
                }
            }
        }
        let dx = (w.x - s.x).abs();
        let dy = (w.y - s.y).abs();
        if dx <= 0.5 && dy <= 0.5 {
            vertex.push(P { x: w.x + x0 as f64, y: w.y + y0 as f64 });
            continue;
        }

        let mut min = quadform(&qm, s);
        let mut xmin = s.x;
        let mut ymin = s.y;
        if qm[0] != 0.0 {
            for z in 0..2 {
                w.y = s.y - 0.5 + z as f64;
                w.x = -(qm[1] * w.y + qm[2]) / qm[0];
                let dx = (w.x - s.x).abs();
                let cand = quadform(&qm, w);
                if dx <= 0.5 && cand < min {
                    min = cand;
                    xmin = w.x;
                    ymin = w.y;
                }
            }
        }
        if qm[4] != 0.0 {
            for z in 0..2 {
                w.x = s.x - 0.5 + z as f64;
                w.y = -(qm[3] * w.x + qm[5]) / qm[4];
                let dy = (w.y - s.y).abs();
                let cand = quadform(&qm, w);
                if dy <= 0.5 && cand < min {
                    min = cand;
                    xmin = w.x;
                    ymin = w.y;
                }
            }
        }
        for l in 0..2 {
            for k in 0..2 {
                w.x = s.x - 0.5 + l as f64;
                w.y = s.y - 0.5 + k as f64;
                let cand = quadform(&qm, w);
                if cand < min {
                    min = cand;
                    xmin = w.x;
                    ymin = w.y;
                }
            }
        }
        vertex.push(P { x: xmin + x0 as f64, y: ymin + y0 as f64 });
    }
    vertex
}

fn smooth(vertex: Vec<P>, alphamax: f64) -> Smoothed {
    let m = vertex.len() as i64;
    let mu = m as usize;
    let mut tag = vec![Tag::Corner; mu];
    let mut c = vec![[P::default(); 3]; mu];
    let mut alpha_v = vec![0f64; mu];
    for i in 0..m {
        let j = modn(i + 1, m) as usize;
        let k = modn(i + 2, m) as usize;
        let iu = i as usize;
        let p4 = interval(1.0 / 2.0, vertex[k], vertex[j]);
        let denom = ddenom(vertex[iu], vertex[k]);
        let mut alpha;
        if denom != 0.0 {
            let dd = (dpara(vertex[iu], vertex[j], vertex[k]) / denom).abs();
            alpha = if dd > 1.0 { 1.0 - 1.0 / dd } else { 0.0 };
            alpha /= 0.75;
        } else {
            alpha = 4.0 / 3.0;
        }
        if alpha >= alphamax {
            tag[j] = Tag::Corner;
            c[j][1] = vertex[j];
            c[j][2] = p4;
        } else {
            if alpha < 0.55 {
                alpha = 0.55;
            } else if alpha > 1.0 {
                alpha = 1.0;
            }
            let p2 = interval(0.5 + 0.5 * alpha, vertex[iu], vertex[j]);
            let p3 = interval(0.5 + 0.5 * alpha, vertex[k], vertex[j]);
            tag[j] = Tag::Curve;
            c[j] = [p2, p3, p4];
        }
        alpha_v[j] = alpha;
    }
    Smoothed { tag, c, vertex, alpha: alpha_v }
}

#[derive(Clone, Copy, Default)]
struct Opti {
    pen: f64,
    c: [P; 2],
    t: f64,
    s: f64,
}

fn opti_penalty(curve: &Smoothed, i: i64, j: i64, res: &mut Opti, tol: f64, convc: &[i64], areac: &[f64]) -> bool {
    let m = curve.vertex.len() as i64;
    let v = |k: i64| curve.vertex[k as usize];
    let c2 = |k: i64| curve.c[k as usize][2];
    if i == j {
        return true;
    }
    let k = i;
    let i1 = modn(i + 1, m);
    let mut k1 = modn(k + 1, m);
    let conv = convc[k1 as usize];
    if conv == 0 {
        return true;
    }
    let d = ddist(v(i), v(i1));
    let mut k = k1;
    while k != j {
        k1 = modn(k + 1, m);
        let k2 = modn(k + 2, m);
        if convc[k1 as usize] != conv {
            return true;
        }
        if sign(cprod(v(i), v(i1), v(k1), v(k2))) as i64 != conv {
            return true;
        }
        if iprod1(v(i), v(i1), v(k1), v(k2)) < d * ddist(v(k1), v(k2)) * -0.999847695156 {
            return true;
        }
        k = k1;
    }

    let p0 = c2(modn(i, m));
    let mut p1 = v(modn(i + 1, m));
    let mut p2 = v(modn(j, m));
    let p3 = c2(modn(j, m));

    let mut area = areac[j as usize] - areac[i as usize];
    area -= dpara(v(0), c2(i), c2(j)) / 2.0;
    if i >= j {
        area += areac[m as usize];
    }
    let a1 = dpara(p0, p1, p2);
    let a2 = dpara(p0, p1, p3);
    let a3 = dpara(p0, p2, p3);
    let a4 = a1 + a3 - a2;
    if a2 == a1 {
        return true;
    }
    let t = a3 / (a3 - a4);
    let s = a2 / (a2 - a1);
    let a = a2 * t / 2.0;
    if a == 0.0 {
        return true;
    }
    let r = area / a;
    let alpha = 2.0 - (4.0 - r / 0.3).sqrt();
    res.c[0] = interval(t * alpha, p0, p1);
    res.c[1] = interval(s * alpha, p3, p2);
    res.t = t;
    res.s = s;
    p1 = res.c[0];
    p2 = res.c[1];
    res.pen = 0.0;

    let mut k = modn(i + 1, m);
    while k != j {
        let k1 = modn(k + 1, m);
        let t = tangent(p0, p1, p2, p3, v(k), v(k1));
        if t < -0.5 {
            return true;
        }
        let pt = bezier(t, p0, p1, p2, p3);
        let d = ddist(v(k), v(k1));
        if d == 0.0 {
            return true;
        }
        let d1 = dpara(v(k), v(k1), pt) / d;
        if d1.abs() > tol {
            return true;
        }
        if iprod(v(k), v(k1), pt) < 0.0 || iprod(v(k1), v(k), pt) < 0.0 {
            return true;
        }
        res.pen += d1 * d1;
        k = k1;
    }

    let mut k = i;
    while k != j {
        let k1 = modn(k + 1, m);
        let t = tangent(p0, p1, p2, p3, c2(k), c2(k1));
        if t < -0.5 {
            return true;
        }
        let pt = bezier(t, p0, p1, p2, p3);
        let d = ddist(c2(k), c2(k1));
        if d == 0.0 {
            return true;
        }
        let mut d1 = dpara(c2(k), c2(k1), pt) / d;
        let mut d2 = dpara(c2(k), c2(k1), v(k1)) / d;
        d2 *= 0.75 * curve.alpha[k1 as usize];
        if d2 < 0.0 {
            d1 = -d1;
            d2 = -d2;
        }
        if d1 < d2 - tol {
            return true;
        }
        if d1 < d2 {
            res.pen += (d1 - d2) * (d1 - d2);
        }
        k = k1;
    }
    false
}

fn opti_curve(curve: &Smoothed, tol: f64) -> Curve {
    let m = curve.vertex.len() as i64;
    let mu = m as usize;
    let vert = &curve.vertex;

    let mut convc = vec![0i64; mu];
    for i in 0..m {
        if curve.tag[i as usize] == Tag::Curve {
            convc[i as usize] = sign(dpara(vert[modn(i - 1, m) as usize], vert[i as usize], vert[modn(i + 1, m) as usize])) as i64;
        }
    }
    let mut areac = vec![0f64; mu + 1];
    let mut area = 0.0;
    let p0 = vert[0];
    for i in 0..m {
        let i1 = modn(i + 1, m) as usize;
        let iu = i as usize;
        if curve.tag[i1] == Tag::Curve {
            let alpha = curve.alpha[i1];
            area += 0.3 * alpha * (4.0 - alpha) * dpara(curve.c[iu][2], vert[i1], curve.c[i1][2]) / 2.0;
            area += dpara(p0, curve.c[iu][2], curve.c[i1][2]) / 2.0;
        }
        areac[iu + 1] = area;
    }

    let mut pt = vec![0i64; mu + 1];
    let mut pen = vec![0f64; mu + 1];
    let mut len = vec![0i64; mu + 1];
    let mut opt = vec![Opti::default(); mu + 1];
    let mut o = Opti::default();
    pt[0] = -1;
    for j in 1..=m {
        let ju = j as usize;
        pt[ju] = j - 1;
        pen[ju] = pen[ju - 1];
        len[ju] = len[ju - 1] + 1;
        let mut i = j - 2;
        while i >= 0 {
            if opti_penalty(curve, i, modn(j, m), &mut o, tol, &convc, &areac) {
                break;
            }
            let iu = i as usize;
            if len[ju] > len[iu] + 1 || (len[ju] == len[iu] + 1 && pen[ju] > pen[iu] + o.pen) {
                pt[ju] = i;
                pen[ju] = pen[iu] + o.pen;
                len[ju] = len[iu] + 1;
                opt[ju] = o;
                o = Opti::default();
            }
            i -= 1;
        }
    }

    let om = len[mu] as usize;
    let mut out = vec![Seg::Corner([P::default(); 2]); om];
    let mut j = m;
    for i in (0..om).rev() {
        let jm = modn(j, m) as usize;
        if pt[j as usize] == j - 1 {
            out[i] = match curve.tag[jm] {
                Tag::Curve => Seg::Curve(curve.c[jm]),
                Tag::Corner => Seg::Corner([curve.c[jm][1], curve.c[jm][2]]),
            };
        } else {
            let op = &opt[j as usize];
            out[i] = Seg::Curve([op.c[0], op.c[1], curve.c[jm][2]]);
        }
        j = pt[j as usize];
    }
    out
}
