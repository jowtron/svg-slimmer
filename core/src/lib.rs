//! SVG Slimmer's image pipeline: palette, quantize, trace, compare.
//!
//! The same code runs natively (the CLI and the desktop app) and as WebAssembly
//! in the browser. Rasterising is left to the caller, which hands over RGBA
//! pixels: the web and desktop apps draw with the browser's own canvas, so an SVG
//! looks exactly as it does in a browser, and the CLI decodes images itself.
//!
//! The pipeline is a port of the JavaScript the web app ran before, and keeps
//! its arithmetic, including JS rounding rules, so both give the same output.

#[cfg(feature = "api")]
pub mod api;
pub mod compare;
pub mod index;
pub mod palette;
pub mod png;
pub mod potrace;
pub mod tidy;
pub mod trace;

use serde::{Deserialize, Serialize};

/// The long side the original and the result are compared at, and the palette found at.
pub const CMP: usize = 2048;

pub type Rgb = [i32; 3];

/// RGBA pixels, row by row, straight (not premultiplied) alpha.
#[derive(Clone, Copy)]
pub struct Image<'a> {
    pub w: usize,
    pub h: usize,
    pub data: &'a [u8],
}

/// Colours the viewer locked in (`keep`) or left out (`drop`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Choices {
    #[serde(default)]
    pub keep: Vec<Rgb>,
    #[serde(default)]
    pub drop: Vec<Rgb>,
}

/// Redraw: rasterise and trace again. Tidy: keep an SVG's own outlines and only
/// merge colours, join paths and rewrite the path data (see [`tidy`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    #[default]
    Redraw,
    Tidy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Colour,
    Mono,
}

/// Every setting the pipeline reads. Mirrors the web app's `opts()`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Opts {
    pub mode: Mode,
    /// Trace size: long side in pixels.
    pub grid: usize,
    /// Potrace curve tolerance ("smoothing").
    pub tol: f64,
    /// Potrace corner threshold.
    pub alpha: f64,
    /// Speck size at 2048 px across.
    pub turd: f64,
    /// Mono threshold on luminance.
    pub thr: f64,
    /// Decimal places in the output.
    pub dec: u32,
    /// Colours closer than this merge.
    pub merge: f64,
    /// Remove the background where it touches the border.
    pub edge_bg: bool,
    /// Shapes smaller than this percent of the long side keep a finer trace.
    pub protect: f64,
    pub max_col: usize,
    /// Majority-filter passes.
    pub clean: u32,
    /// Restrict blended edge pixels to the colours meeting there.
    pub edges: bool,
    /// Speck removal spares long thin patches (gaps between pen strokes, fine
    /// lines) and only removes compact ones.
    #[serde(default)]
    pub slivers: bool,
    #[serde(default)]
    pub method: Method,
    #[serde(default)]
    pub choices: Choices,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            mode: Mode::Colour,
            grid: 2048,
            tol: 0.5,
            alpha: 1.0,
            turd: 16.0,
            thr: 128.0,
            dec: 0,
            merge: 16.0,
            edge_bg: false,
            protect: 6.0,
            max_col: 24,
            clean: 0,
            edges: true,
            slivers: false,
            method: Method::Redraw,
            choices: Choices::default(),
        }
    }
}

/// JavaScript's `Math.round`: halves round towards +infinity.
pub(crate) fn js_round(x: f64) -> f64 {
    let f = x.floor();
    if x - f >= 0.5 {
        f + 1.0
    } else {
        f
    }
}

pub(crate) fn dist2(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)
}

pub fn to_hex(c: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

/// Speck size is set at 2048 px across and scaled with the image's area.
pub fn speck_area(o: &Opts, w: usize, h: usize) -> usize {
    js_round(o.turd * (w.max(h) as f64 / 2048.0).powi(2)).max(0.0) as usize
}
