//! Check the Rust pipeline against reference output dumped from the JS version.
//! `cargo run --release -p slimmer-core --example parity -- <case dir>...`
//! Each case dir holds meta.json plus the browser's raw pixels (cmp.bin, grid.bin,
//! bcmp.bin, cmpw.bin) and its index maps (rawidx.bin, idx.bin) and diff image.

use slimmer_core::{compare, index, palette, trace, Image, Mode, Opts, Rgb};
use std::time::Instant;

fn read(dir: &str, name: &str) -> Option<Vec<u8>> {
    std::fs::read(format!("{dir}/{name}.bin")).ok()
}
fn i16s(b: &[u8]) -> Vec<i16> {
    b.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
}
fn dims(v: &serde_json::Value) -> (usize, usize) {
    (v[0].as_u64().unwrap() as usize, v[1].as_u64().unwrap() as usize)
}

fn main() {
    let mut failed = false;
    for dir in std::env::args().skip(1) {
        let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(format!("{dir}/meta.json")).unwrap()).unwrap();
        let o: Opts = serde_json::from_value(meta["o"].clone()).unwrap();
        let declared: Vec<Rgb> = serde_json::from_value(meta["declared"].clone()).unwrap();
        let js_p: palette::Pal = serde_json::from_value(meta["p"].clone()).unwrap();
        let (cw, ch) = dims(&meta["cmp"]);
        let (gw, gh) = dims(&meta["grid"]);
        let (bw, bh) = dims(&meta["bcmp"]);
        let cmp = read(&dir, "cmp").unwrap();
        let grid = read(&dir, "grid").unwrap();
        let bcmp = read(&dir, "bcmp").unwrap();
        let cimg = Image { w: cw, h: ch, data: &cmp };
        let gimg = Image { w: gw, h: gh, data: &grid };
        let bimg = Image { w: bw, h: bh, data: &bcmp };
        let mut ok = Vec::new();
        let mut check = |what: &str, good: bool| {
            ok.push(format!("{what}:{}", if good { "ok" } else { "FAIL" }));
            if !good {
                failed = true;
            }
        };

        let t = Instant::now();
        let p = match o.mode {
            Mode::Mono => palette::Pal::mono(js_p.pal[0]),
            Mode::Colour => palette::palette_for(&cimg, &o, &declared).unwrap(),
        };
        let t_pal = t.elapsed();
        let close = |a: &[f64], b: &[f64]| a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-12);
        let shares = |l: &[palette::Share]| l.iter().map(|s| s.share).collect::<Vec<_>>();
        let cols = |l: &[palette::Share]| l.iter().map(|s| s.c).collect::<Vec<_>>();
        let same_pal = p.pal == js_p.pal
            && p.bg == js_p.bg
            && p.order == js_p.order
            && p.pinned == js_p.pinned
            && close(&p.share, &js_p.share)
            && cols(&p.merged) == cols(&js_p.merged)
            && close(&shares(&p.merged), &shares(&js_p.merged))
            && cols(&p.removed) == cols(&js_p.removed)
            && close(&shares(&p.removed), &shares(&js_p.removed));
        check("palette", same_pal);
        if !same_pal {
            eprintln!("  rust {}\n  js   {}", serde_json::to_string(&p).unwrap(), meta["p"]);
        }

        let raw = match o.mode {
            Mode::Mono => index::mono_index(&gimg, o.thr),
            Mode::Colour => index::classify(&gimg, &js_p.pal, js_p.bg, o.edges),
        };
        check("classify", raw == i16s(&read(&dir, "rawidx").unwrap()));
        let idx = index::index_map(&gimg, &o, &js_p);
        let js_idx = i16s(&read(&dir, "idx").unwrap());
        let nd = idx.iter().zip(&js_idx).filter(|(a, b)| a != b).count();
        check("index", nd == 0);
        if nd > 0 {
            eprintln!("  index differs at {nd} pixels");
        }

        let t = Instant::now();
        let traced = trace::trace(&gimg, &o, &js_p);
        let t_trace = t.elapsed();
        let js_svg = meta["svg"].as_str().unwrap();
        check("svg", traced.svg == js_svg);
        if traced.svg != js_svg {
            let at = traced.svg.bytes().zip(js_svg.bytes()).position(|(a, b)| a != b).unwrap_or(0);
            let lo = at.saturating_sub(80);
            eprintln!("  svg sizes rust {} js {}; first difference at byte {at}", traced.svg.len(), js_svg.len());
            eprintln!("  rust …{}…", &traced.svg[lo..(at + 80).min(traced.svg.len())]);
            eprintln!("  js   …{}…", &js_svg[lo..(at + 80).min(js_svg.len())]);
        }

        let t = Instant::now();
        let cmpw = read(&dir, "cmpw");
        let a = match (&o.mode, &cmpw) {
            (Mode::Mono, Some(w)) => Image { w: cw, h: ch, data: w },
            _ => cimg,
        };
        let a_idx = index::source_index(&a, &o, &js_p);
        let c = compare::compare(&a_idx, &a, &bimg, &o, &js_p);
        let t_cmp = t.elapsed();
        let js_pct = meta["diffPct"].as_f64().unwrap();
        check("diff%", (c.diff_pct - js_pct).abs() < 1e-9);
        if (c.diff_pct - js_pct).abs() >= 1e-9 {
            eprintln!("  diff% rust {} js {}", c.diff_pct, js_pct);
        }
        println!(
            "{:<22} {}  | rust: palette {:>4} ms, trace {:>5} ms, compare {:>4} ms (JS whole trace {:.0} ms)",
            dir.rsplit('/').next().unwrap(),
            ok.join(" "),
            t_pal.as_millis(),
            t_trace.as_millis(),
            t_cmp.as_millis(),
            meta["ms"].as_f64().unwrap()
        );
    }
    if failed {
        std::process::exit(1);
    }
}
