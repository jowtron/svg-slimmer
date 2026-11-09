//! Snap to lines and circles: an optional pass over a traced layer that makes
//! straight strokes exactly straight and round things true circular arcs.
//!
//! Potrace draws every outline as a chain of Bézier curves and corners. On a
//! logo, a stroke that was ruled straight comes back as a curve bowed by a
//! fraction of a pixel, and a circle as four to eight curves that are each
//! slightly off round. This pass rewrites a subpath piece by piece:
//!
//! 1. A curve whose points all lie within the tolerance of its chord, and whose
//!    ends leave in nearly the chord's direction, becomes a line.
//! 2. Runs of lines whose inner vertices lie within the tolerance of the run's
//!    chord become one line, and a line within the tolerance of level or
//!    upright is made exactly so (its ends move, and the curves either side
//!    move their nearest control point with them, so they stay smooth).
//! 3. Runs of two or more consecutive curves that one circle fits become an
//!    SVG arc. The circle is fitted through the run's two ends (its centre is
//!    on their perpendicular bisector, which leaves one unknown, solved by
//!    least squares), every sampled point must lie within the tolerance of it,
//!    and the run must turn one way only. Next to a line or another arc, the
//!    arc must leave in nearly the same direction; next to a curve it may be
//!    a little further off, and the curve's handle is turned to meet it, so
//!    joins stay smooth. If turning the handle would move that curve further
//!    than the tolerance, the arc goes back to curves. A subpath made only of
//!    curves that one circle fits becomes two half circles.
//!
//! A single curve is left alone: it is already as round as an arc would be,
//! and measured on test images, arcs for single curves made files bigger and
//! closer to the original by nothing.
//!
//! The tolerance is in grid pixels (see [`tolerance`]); `trace::write_path`
//! writes the arcs, choosing per arc how many decimals its radius needs.
//!
//! Corners are never touched: Potrace joins its curves smoothly and puts every
//! corner inside a corner segment, whose two lines are kept as lines.

use crate::trace::{Cmd, Sub};
use std::f64::consts::PI;

type P = [f64; 2];

#[derive(Clone, Copy)]
enum Kind {
    Line,
    /// A cubic's two control points.
    Cub(P, P),
    /// A circular arc: centre, radius and the signed angle it turns through
    /// (positive is SVG's sweep flag 1).
    Arc(P, f64, f64),
}

#[derive(Clone, Copy)]
struct Piece {
    a: P,
    b: P,
    k: Kind,
}

/// Points sampled on each curve when testing a fit.
const SAMPLES: usize = 8;
/// How far, in degrees, a line may leave a smooth join from the curve it
/// replaces. Keeps gently curving edges curves.
const LINE_TURN: f64 = 5.0;
/// How far an arc may leave a join with a line or another arc from the
/// direction that piece arrives or leaves in: neither can bend to meet it.
const ARC_TURN: f64 = 4.0;
/// How far an arc may leave a join with a curve from the direction the
/// replaced curves did. The neighbouring curve's handle is then turned to meet
/// the arc, so the join stays smooth.
const ARC_TURN_CURVE: f64 = 12.0;

fn sub(a: P, b: P) -> P {
    [a[0] - b[0], a[1] - b[1]]
}
fn len(v: P) -> f64 {
    v[0].hypot(v[1])
}
fn dot(a: P, b: P) -> f64 {
    a[0] * b[0] + a[1] * b[1]
}
fn cross(a: P, b: P) -> f64 {
    a[0] * b[1] - a[1] * b[0]
}

/// The angle between two directions, in degrees (0 when either is zero).
fn turn(u: P, v: P) -> f64 {
    let (lu, lv) = (len(u), len(v));
    if lu < 1e-12 || lv < 1e-12 {
        return 0.0;
    }
    (dot(u, v) / (lu * lv)).clamp(-1.0, 1.0).acos().to_degrees()
}

fn bez(p: &Piece, t: f64) -> P {
    match p.k {
        Kind::Cub(c1, c2) => {
            let u = 1.0 - t;
            let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
            [a * p.a[0] + b * c1[0] + c * c2[0] + d * p.b[0], a * p.a[1] + b * c1[1] + c * c2[1] + d * p.b[1]]
        }
        _ => [p.a[0] + (p.b[0] - p.a[0]) * t, p.a[1] + (p.b[1] - p.a[1]) * t],
    }
}

/// The direction a piece leaves its start in, and arrives at its end in.
fn tangents(p: &Piece) -> (P, P) {
    match p.k {
        Kind::Cub(c1, c2) => {
            let s = if len(sub(c1, p.a)) > 1e-9 { sub(c1, p.a) } else { sub(c2, p.a) };
            let e = if len(sub(p.b, c2)) > 1e-9 { sub(p.b, c2) } else { sub(p.b, c1) };
            (s, e)
        }
        Kind::Arc(c, _, d) => {
            let (ra, rb) = (sub(p.a, c), sub(p.b, c));
            let sg = d.signum();
            ([-ra[1] * sg, ra[0] * sg], [-rb[1] * sg, rb[0] * sg])
        }
        Kind::Line => (sub(p.b, p.a), sub(p.b, p.a)),
    }
}

/// Distance from `p` to the segment `a`-`b`.
fn seg_dist(p: P, a: P, b: P) -> f64 {
    let ab = sub(b, a);
    let l2 = dot(ab, ab);
    if l2 < 1e-18 {
        return len(sub(p, a));
    }
    let t = (dot(sub(p, a), ab) / l2).clamp(0.0, 1.0);
    len(sub(p, [a[0] + ab[0] * t, a[1] + ab[1] * t]))
}

fn pieces_of(s: &Sub) -> Vec<Piece> {
    let mut out = Vec::new();
    let (mut cur, mut start) = ([0.0; 2], [0.0; 2]);
    let line = |out: &mut Vec<Piece>, a: P, b: P| {
        if len(sub(a, b)) > 1e-9 {
            out.push(Piece { a, b, k: Kind::Line });
        }
    };
    for c in s {
        match *c {
            Cmd::M(p) => {
                cur = p;
                start = p;
            }
            Cmd::C(v) => {
                let b = [v[4], v[5]];
                out.push(Piece { a: cur, b, k: Kind::Cub([v[0], v[1]], [v[2], v[3]]) });
                cur = b;
            }
            Cmd::L(v) => {
                line(&mut out, cur, [v[0], v[1]]);
                line(&mut out, [v[0], v[1]], [v[2], v[3]]);
                cur = [v[2], v[3]];
            }
            Cmd::A(v) => {
                // Not produced by the tracer; keep as a line rather than guess.
                line(&mut out, cur, [v[3], v[4]]);
                cur = [v[3], v[4]];
            }
        }
    }
    line(&mut out, cur, start);
    out
}

/// The circle through `a` and `b` that best fits `pts`, as centre and radius.
/// The centre lies on the perpendicular bisector of `a`-`b`, at a signed
/// distance `t` from the midpoint; the algebraic error |p-c|² - r² of every
/// point is then linear in `t`, so least squares solves it directly.
fn circle_through(a: P, b: P, pts: &[P]) -> Option<(P, f64)> {
    let ab = sub(b, a);
    let l = len(ab);
    if l < 1e-9 {
        return None;
    }
    let n = [-ab[1] / l, ab[0] / l];
    let m = [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0];
    let h2 = (l / 2.0).powi(2);
    let (mut num, mut den) = (0.0, 0.0);
    for &p in pts {
        let q = sub(p, m);
        let g = 2.0 * dot(n, q);
        num += (dot(q, q) - h2) * g;
        den += g * g;
    }
    if den < 1e-12 {
        return None;
    }
    let t = num / den;
    Some(([m[0] + n[0] * t, m[1] + n[1] * t], (h2 + t * t).sqrt()))
}

/// The best circle through all of `pts` (Kåsa's algebraic fit).
fn circle_free(pts: &[P]) -> Option<(P, f64)> {
    let k = pts.len() as f64;
    let mx = pts.iter().map(|p| p[0]).sum::<f64>() / k;
    let my = pts.iter().map(|p| p[1]).sum::<f64>() / k;
    // Solve for x² + y² + D x + E y + F = 0 in coordinates about the mean.
    let (mut sxx, mut sxy, mut syy, mut sx, mut sy, mut sxz, mut syz, mut sz) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    for p in pts {
        let (x, y) = (p[0] - mx, p[1] - my);
        let z = x * x + y * y;
        sxx += x * x;
        sxy += x * y;
        syy += y * y;
        sx += x;
        sy += y;
        sxz += x * z;
        syz += y * z;
        sz += z;
    }
    let m = [[sxx, sxy, sx], [sxy, syy, sy], [sx, sy, k]];
    let r = [-sxz, -syz, -sz];
    let det = |m: &[[f64; 3]; 3]| {
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    };
    let d0 = det(&m);
    if d0.abs() < 1e-12 {
        return None;
    }
    let mut sol = [0.0; 3];
    for (i, s) in sol.iter_mut().enumerate() {
        let mut mi = m;
        for row in 0..3 {
            mi[row][i] = r[row];
        }
        *s = det(&mi) / d0;
    }
    let (cx, cy) = (-sol[0] / 2.0, -sol[1] / 2.0);
    let r2 = cx * cx + cy * cy - sol[2];
    if r2 <= 0.0 {
        return None;
    }
    Some(([cx + mx, cy + my], r2.sqrt()))
}

/// Sample points of pieces, the first piece's start included.
fn samples(ps: &[Piece]) -> Vec<P> {
    let mut pts = vec![ps[0].a];
    for p in ps {
        for i in 1..=SAMPLES {
            pts.push(bez(p, i as f64 / SAMPLES as f64));
        }
    }
    pts
}

/// The signed angle `pts` turn through about `c`, or None when they turn back
/// on themselves.
fn sweep(c: P, pts: &[P]) -> Option<f64> {
    let mut total = 0.0;
    let mut sign = 0.0;
    for w in pts.windows(2) {
        let (u, v) = (sub(w[0], c), sub(w[1], c));
        let d = cross(u, v).atan2(dot(u, v));
        if d.abs() < 1e-9 {
            continue;
        }
        if sign == 0.0 {
            sign = d.signum();
        } else if d.signum() != sign {
            return None;
        }
        total += d;
    }
    Some(total)
}

/// One arc for the curves `ps`, if a circle fits them within `tol`. `prev` and
/// `next` are the pieces either side, as far as they are known.
fn fit_arc(ps: &[Piece], tol: f64, prev: Option<&Piece>, next: Option<&Piece>) -> Option<Piece> {
    let (a, b) = (ps[0].a, ps[ps.len() - 1].b);
    if len(sub(b, a)) < 2.0 * tol {
        return None;
    }
    let pts = samples(ps);
    let (c, r) = circle_through(a, b, &pts)?;
    if pts.iter().any(|&p| (len(sub(p, c)) - r).abs() > tol) {
        return None;
    }
    let d = sweep(c, &pts)?;
    if d.abs() < 0.05 || d.abs() > 2.0 * PI - 0.2 {
        return None;
    }
    let arc = Piece { a, b, k: Kind::Arc(c, r, d) };
    let (s1, e1) = tangents(&arc);
    // At each end, a curve alongside can bend to meet the arc; anything else must
    // already meet it.
    let ok = |nb: Option<&Piece>, own: P, arc_t: P, at_start: bool| match nb {
        Some(q) if !matches!(q.k, Kind::Cub(..)) => {
            let (qs, qe) = tangents(q);
            turn(if at_start { qe } else { qs }, arc_t) <= ARC_TURN
        }
        _ => turn(own, arc_t) <= ARC_TURN_CURVE,
    };
    let (s0, _) = tangents(&ps[0]);
    let (_, e0) = tangents(&ps[ps.len() - 1]);
    (ok(prev, s0, s1, true) && ok(next, e0, e1, false)).then_some(arc)
}

fn straight(p: &Piece, tol: f64) -> bool {
    let Kind::Cub(c1, c2) = p.k else { return false };
    let chord = sub(p.b, p.a);
    let (s, e) = tangents(p);
    (1..SAMPLES).all(|i| seg_dist(bez(p, i as f64 / SAMPLES as f64), p.a, p.b) <= tol)
        && seg_dist(c1, p.a, p.b) <= 3.0 * tol
        && seg_dist(c2, p.a, p.b) <= 3.0 * tol
        && turn(s, chord) <= LINE_TURN
        && turn(e, chord) <= LINE_TURN
}

/// Move the vertex at the end of piece `i` (the start of piece `i + 1`) by `d`,
/// with the nearest control points either side, so smooth joins stay smooth.
fn shift(ps: &mut [Piece], i: usize, d: P) {
    let n = ps.len();
    let j = (i + 1) % n;
    let mv = |p: &mut P| {
        p[0] += d[0];
        p[1] += d[1];
    };
    mv(&mut ps[i].b);
    if let Kind::Cub(_, ref mut c2) = ps[i].k {
        mv(c2);
    }
    mv(&mut ps[j].a);
    if let Kind::Cub(ref mut c1, _) = ps[j].k {
        mv(c1);
    }
}

/// Snap one closed subpath.
fn snap_sub(s: &Sub, tol: f64) -> Sub {
    let mut ps = pieces_of(s);
    if ps.len() < 2 {
        return s.clone();
    }
    // A shape made only of curves may be a whole circle.
    if ps.iter().all(|p| matches!(p.k, Kind::Cub(..))) {
        if let Some(c) = whole_circle(&ps, tol) {
            return c;
        }
    }
    // Start at a line, if there is one, so no run of curves wraps round the end.
    if let Some(i) = ps.iter().position(|p| matches!(p.k, Kind::Line)) {
        ps.rotate_left(i);
    }

    // 1. Nearly straight curves become lines.
    for p in ps.iter_mut() {
        if straight(p, tol) {
            p.k = Kind::Line;
        }
    }

    // 2. Join runs of lines that lie along one line.
    let mut merged: Vec<Piece> = Vec::with_capacity(ps.len());
    let mut i = 0;
    while i < ps.len() {
        if !matches!(ps[i].k, Kind::Line) {
            merged.push(ps[i]);
            i += 1;
            continue;
        }
        let mut j = i;
        while j + 1 < ps.len() && matches!(ps[j + 1].k, Kind::Line) && (i..=j).all(|k| seg_dist(ps[k].b, ps[i].a, ps[j + 1].b) <= tol) {
            j += 1;
        }
        merged.push(Piece { a: ps[i].a, b: ps[j].b, k: Kind::Line });
        i = j + 1;
    }
    let mut ps = merged;

    // Level and upright lines: make them exactly so.
    let n = ps.len();
    for i in 0..n {
        if !matches!(ps[i].k, Kind::Line) {
            continue;
        }
        let (a, b) = (ps[i].a, ps[i].b);
        let (dx, dy) = ((b[0] - a[0]).abs(), (b[1] - a[1]).abs());
        let prev = (i + n - 1) % n;
        if dy > 0.0 && dy <= tol && dx > 10.0 * dy {
            let m = (a[1] + b[1]) / 2.0;
            shift(&mut ps, prev, [0.0, m - a[1]]);
            shift(&mut ps, i, [0.0, m - b[1]]);
        } else if dx > 0.0 && dx <= tol && dy > 10.0 * dx {
            let m = (a[0] + b[0]) / 2.0;
            shift(&mut ps, prev, [m - a[0], 0.0]);
            shift(&mut ps, i, [m - b[0], 0.0]);
        }
    }

    // 3. Runs of curves on one circle become arcs. Each arc remembers the
    // curves it replaced, in case it has to give them back.
    let mut out: Vec<(Piece, Option<(usize, usize)>)> = Vec::with_capacity(ps.len());
    let mut i = 0;
    while i < ps.len() {
        if !matches!(ps[i].k, Kind::Cub(..)) {
            out.push((ps[i], None));
            i += 1;
            continue;
        }
        let mut best = None;
        let mut j = i;
        while j < ps.len() && matches!(ps[j].k, Kind::Cub(..)) {
            let prev = if i == 0 { ps.last() } else { out.last().map(|o| &o.0) };
            let next = if j + 1 < ps.len() { ps.get(j + 1) } else { out.first().map(|o| &o.0) };
            match fit_arc(&ps[i..=j], tol, prev, next) {
                // One curve alone is already as round as an arc would be, and is
                // usually shorter written, so only runs of two or more change.
                Some(a) if j > i => best = Some((a, j)),
                Some(_) => {}
                None => break,
            }
            j += 1;
        }
        match best {
            Some((a, j)) => {
                out.push((a, Some((i, j))));
                i = j + 1;
            }
            None => {
                out.push((ps[i], None));
                i += 1;
            }
        }
    }
    // Turn the handles of curves next to arcs to meet them. Where that moves a
    // curve further than the tolerance, the arc beside it goes back to curves.
    let pieces = loop {
        let n = out.len();
        let mut bent: Vec<Piece> = out.iter().map(|o| o.0).collect();
        let mut undo = None;
        for k in 0..n {
            let Kind::Cub(c1, c2) = out[k].0.k else { continue };
            let (pk, nk) = ((k + n - 1) % n, (k + 1) % n);
            let p = out[k].0;
            let (mut d1, mut d2) = (c1, c2);
            if matches!(out[pk].0.k, Kind::Arc(..)) {
                let (_, t) = tangents(&out[pk].0);
                let (h, lt) = (len(sub(c1, p.a)), len(t));
                d1 = [p.a[0] + t[0] / lt * h, p.a[1] + t[1] / lt * h];
            }
            if matches!(out[nk].0.k, Kind::Arc(..)) {
                let (t, _) = tangents(&out[nk].0);
                let (h, lt) = (len(sub(p.b, c2)), len(t));
                d2 = [p.b[0] - t[0] / lt * h, p.b[1] - t[1] / lt * h];
            }
            let q = Piece { k: Kind::Cub(d1, d2), ..p };
            if (1..SAMPLES).any(|i| len(sub(bez(&p, i as f64 / SAMPLES as f64), bez(&q, i as f64 / SAMPLES as f64))) > tol) {
                undo = Some(if matches!(out[pk].0.k, Kind::Arc(..)) { pk } else { nk });
                break;
            }
            bent[k] = q;
        }
        match undo {
            None => break bent,
            Some(u) => {
                let (a, b) = out[u].1.unwrap();
                out.splice(u..=u, ps[a..=b].iter().map(|&p| (p, None)));
            }
        }
    };
    write(&pieces)
}

/// A subpath of curves that one circle fits, as two half circles.
fn whole_circle(ps: &[Piece], tol: f64) -> Option<Sub> {
    let pts = samples(ps);
    let (c, r) = circle_free(&pts)?;
    if r < 2.0 * tol || pts.iter().any(|&p| (len(sub(p, c)) - r).abs() > tol) {
        return None;
    }
    let d = sweep(c, &pts)?;
    if (d.abs() - 2.0 * PI).abs() > 0.3 {
        return None;
    }
    let u = sub(ps[0].a, c);
    let lu = len(u);
    let u = [u[0] / lu * r, u[1] / lu * r];
    let a = [c[0] + u[0], c[1] + u[1]];
    let b = [c[0] - u[0], c[1] - u[1]];
    let half = d.signum() * PI;
    Some(write(&[Piece { a, b, k: Kind::Arc(c, r, half) }, Piece { a: b, b: a, k: Kind::Arc(c, r, half) }]))
}

fn write(ps: &[Piece]) -> Sub {
    let mut s = vec![Cmd::M(ps[0].a)];
    for p in ps {
        s.push(match p.k {
            Kind::Line => Cmd::L([p.b[0], p.b[1], p.b[0], p.b[1]]),
            Kind::Cub(c1, c2) => Cmd::C([c1[0], c1[1], c2[0], c2[1], p.b[0], p.b[1]]),
            Kind::Arc(c, r, d) => {
                let a0 = (p.a[1] - c[1]).atan2(p.a[0] - c[0]) + d / 2.0;
                Cmd::A([r, (d.abs() > PI) as u8 as f64, (d > 0.0) as u8 as f64, p.b[0], p.b[1], c[0] + r * a0.cos(), c[1] + r * a0.sin()])
            }
        });
    }
    s
}

/// Snap every subpath of a layer. `tol` is in the trace's own units.
pub(crate) fn snap(subs: Vec<Sub>, tol: f64) -> Vec<Sub> {
    subs.iter().map(|s| snap_sub(s, tol)).collect()
}

/// Where an SVG arc from `s` to `e` with radius `r` and these flags puts its
/// midpoint, following the SVG rules (a radius too small for the chord grows
/// until it just fits).
pub(crate) fn arc_mid(s: P, e: P, r: f64, large: bool, sw: bool) -> Option<P> {
    let half = [(s[0] - e[0]) / 2.0, (s[1] - e[1]) / 2.0];
    let d = len(half);
    if d < 1e-12 {
        return None;
    }
    let r = r.max(d);
    let h = (r * r - d * d).max(0.0).sqrt();
    let sg = if large != sw { 1.0 } else { -1.0 };
    let m = [(s[0] + e[0]) / 2.0, (s[1] + e[1]) / 2.0];
    let c = [m[0] + sg * h / d * half[1], m[1] - sg * h / d * half[0]];
    let t0 = (s[1] - c[1]).atan2(s[0] - c[0]);
    let t1 = (e[1] - c[1]).atan2(e[0] - c[0]);
    let mut dt = t1 - t0;
    if sw && dt < 0.0 {
        dt += 2.0 * PI;
    } else if !sw && dt > 0.0 {
        dt -= 2.0 * PI;
    }
    let a = t0 + dt / 2.0;
    Some([c[0] + r * a.cos(), c[1] + r * a.sin()])
}

/// The snapping tolerance for a trace `long` px on its long side: about a
/// third of a pixel at 2048, the same share of the picture at other sizes,
/// and never under half a pixel of a bitmap traced at its own size and scaled
/// up by `s`.
pub(crate) fn tolerance(long: usize, s: f64) -> f64 {
    (0.35 * long as f64 / 2048.0).max(0.5 * s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arc_mid_matches_quarter_circle() {
        // A quarter circle of radius 10 about the origin, from (10,0) to (0,10),
        // turning the positive way (sweep 1), small arc.
        let m = arc_mid([10.0, 0.0], [0.0, 10.0], 10.0, false, true).unwrap();
        let k = 10.0 / 2f64.sqrt();
        assert!((m[0] - k).abs() < 1e-9 && (m[1] - k).abs() < 1e-9);
    }

    #[test]
    fn circle_becomes_two_arcs() {
        // Four cubics approximating a circle of radius 50 about (100,100).
        let k = 0.5522847498 * 50.0;
        let (cx, cy) = (100.0, 100.0);
        let s: Sub = vec![
            Cmd::M([150.0, 100.0]),
            Cmd::C([150.0, cy + k, cx + k, 150.0, 100.0, 150.0]),
            Cmd::C([cx - k, 150.0, 50.0, cy + k, 50.0, 100.0]),
            Cmd::C([50.0, cy - k, cx - k, 50.0, 100.0, 50.0]),
            Cmd::C([cx + k, 50.0, 150.0, cy - k, 150.0, 100.0]),
        ];
        let out = snap(vec![s], 0.5);
        assert_eq!(out[0].len(), 3);
        assert!(out[0][1..].iter().all(|c| matches!(c, Cmd::A(_))));
    }

    #[test]
    fn bowed_line_straightens() {
        let s: Sub = vec![
            Cmd::M([0.0, 0.0]),
            Cmd::C([30.0, 0.1, 60.0, 0.1, 90.0, 0.0]),
            Cmd::L([90.0, 50.0, 0.0, 50.0]),
            Cmd::L([0.0, 25.0, 0.0, 0.0]),
        ];
        let out = snap(vec![s], 0.5);
        // The bowed top is a line now, and the two lines down the left side are one.
        assert!(out[0][1..].iter().all(|c| matches!(c, Cmd::L(_))));
        assert_eq!(out[0].len(), 5);
    }
}
