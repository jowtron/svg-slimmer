//! `--polish`: the core's polish (`slimmer_core::polish`), drawn with resvg.
//!
//! The web app draws each try with the browser's canvas; here resvg draws
//! them, several at once. The work is bounded the way the app's is, by a
//! number of renders as well as by time, so a run here predicts the app's.

use rayon::prelude::*;
use resvg::{tiny_skia, usvg};
use slimmer_core::polish::Polish;
use std::time::Instant;

fn render(svg: &str, w: usize, h: usize, opt: &usvg::Options) -> Vec<u8> {
    let Ok(tree) = usvg::Tree::from_str(svg, opt) else { return vec![] };
    let Some(mut pm) = tiny_skia::Pixmap::new(w as u32, h as u32) else { return vec![] };
    let size = tree.size();
    resvg::render(&tree, tiny_skia::Transform::from_scale(w as f32 / size.width(), h as f32 / size.height()), &mut pm.as_mut());
    // Straight alpha, as a canvas hands it over.
    pm.pixels().iter().flat_map(|p| {
        let c = p.demultiply();
        [c.red(), c.green(), c.blue(), c.alpha()]
    }).collect()
}

/// The SVG drawn by resvg at w x h, as PNG (for checking against other renderers).
pub fn render_png(svg: &str, w: u32, h: u32) -> Option<Vec<u8>> {
    let mut opt = usvg::Options::default();
    opt.fontdb_mut().load_system_fonts();
    let tree = usvg::Tree::from_str(svg, &opt).ok()?;
    let size = tree.size();
    let mut pm = tiny_skia::Pixmap::new(w, h)?;
    resvg::render(&tree, tiny_skia::Transform::from_scale(w as f32 / size.width(), h as f32 / size.height()), &mut pm.as_mut());
    pm.encode_png().ok()
}

pub struct Report {
    pub svg: String,
    pub before: f64,
    pub after: f64,
    pub renders: usize,
    pub secs: f64,
    pub log: Vec<String>,
}

pub fn polish(svg: &str, img: &image::RgbaImage, long: usize, seconds: f64, max_renders: usize) -> Result<Report, String> {
    let t0 = Instant::now();
    let mut opt = usvg::Options::default();
    opt.fontdb_mut().load_system_fonts();
    let mut p = Polish::new(svg, img.as_raw(), img.width() as usize, img.height() as usize, long, max_renders)?;
    let (w, h) = p.size();
    let batch = rayon::current_num_threads().max(1);
    loop {
        // Leave the last part of the time for putting back changes that bought nothing.
        if t0.elapsed().as_secs_f64() > seconds * 0.85 {
            p.wrap_up();
        }
        if t0.elapsed().as_secs_f64() > seconds {
            break;
        }
        let asks = p.ask(batch);
        if asks.is_empty() {
            break;
        }
        let renders: Vec<Vec<u8>> = asks.par_iter().map(|t| render(t, w, h, &opt)).collect();
        let refs: Vec<&[u8]> = renders.iter().map(|v| v.as_slice()).collect();
        p.tell(&refs);
    }
    Ok(Report { svg: p.svg(), before: p.before(), after: p.score(), renders: p.renders(), secs: t0.elapsed().as_secs_f64(), log: p.log.clone() })
}
