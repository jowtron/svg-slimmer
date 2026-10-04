//! svg-slim: re-trace a logo as a compact colour SVG from the command line.

use rayon::prelude::*;
use resvg::{tiny_skia, usvg};
use slimmer_core::{compare, index, palette::Pal, png, tidy, trace, Image, Method, Mode, Opts, Rgb, CMP};
use std::collections::HashMap;
use std::sync::Mutex;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::time::Instant;

const HELP: &str = "svg-slim: re-trace a logo or icon as a compact colour SVG

Usage: svg-slim <input> [options]

Input is an SVG, PNG, JPEG, WebP, GIF or BMP.

  -o, --out <file>        Write the SVG here (default: <input>.slim.svg)
  --tidy                  SVG input only: keep its outlines, just merge colours
                          closer than --merge, join paths and shorten the data
  --preset <name>         small, balanced (default) or fine
  --grid <px>             Trace size, long side: 1024, 2048 or 4096
  --smoothing <t>         Curve smoothing, 0.1 to 1.5
  --corner <a>            Corner threshold, 0 to 1.34 (default 1); 0 = straight lines
  --protect <pct>         Keep shapes under this % of the width sharp (0 = off)
  --decimals <n>          Decimal places in the output (default 0)
  --colours <n>           Most colours to keep
  --merge <d>             Merge colours closer than this (default 16)
  --mono                  One colour
  --threshold <v>         Mono threshold, 0 to 255 (default 128)
  --specks <px>           Remove specks up to this size (default 16)
  --clean <passes>        Majority-filter passes
  --slivers               Speck removal keeps long thin patches, such as the
                          gaps between pen strokes
  --background / --no-background   Remove the colour touching the border
  --no-edges              Don't restrict blended edge pixels
  --smallest              Try detail, smoothing, straight lines, stronger colour
                          merging and (for SVGs) --tidy; keep the smallest
                          result within --target percent difference
  --target <pct>          Allowed difference for --smallest (default 0.2)
  --diff <file.png>       Write the difference image
  --png <file.png>        Also write the quantized image as an indexed PNG
  --png-size <px>         Long side of that PNG (default: the original's size)
  -q, --quiet             Only print errors
";

enum Source {
    Svg(String),
    Bitmap(image::RgbaImage),
}

/// Pixels owned by us, borrowed by the core as an `Image`.
struct Raster {
    w: usize,
    h: usize,
    data: Vec<u8>,
}

impl Raster {
    fn image(&self) -> Image<'_> {
        Image { w: self.w, h: self.h, data: &self.data }
    }
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("svg-slim: {msg}");
    exit(1)
}

fn svg_options() -> usvg::Options<'static> {
    let mut opt = usvg::Options::default();
    opt.fontdb_mut().load_system_fonts();
    opt
}

/// Draw an SVG at long side `n`. Straight (not premultiplied) RGBA out.
fn raster_svg(text: &str, n: usize, on_white: bool) -> Raster {
    let tree = usvg::Tree::from_str(text, &svg_options()).unwrap_or_else(|e| fail(format!("that file isn't a readable SVG ({e})")));
    let size = tree.size();
    let s = n as f32 / size.width().max(size.height());
    let w = ((size.width() * s).round() as u32).max(1);
    let h = ((size.height() * s).round() as u32).max(1);
    let mut pixmap = tiny_skia::Pixmap::new(w, h).unwrap();
    if on_white {
        pixmap.fill(tiny_skia::Color::WHITE);
    }
    resvg::render(&tree, tiny_skia::Transform::from_scale(w as f32 / size.width(), h as f32 / size.height()), &mut pixmap.as_mut());
    let data = pixmap.pixels().iter().flat_map(|p| {
        let c = p.demultiply();
        [c.red(), c.green(), c.blue(), c.alpha()]
    });
    Raster { w: w as usize, h: h as usize, data: data.collect() }
}

fn raster(src: &Source, n: usize, on_white: bool) -> Raster {
    match src {
        Source::Svg(text) => raster_svg(text, n, on_white),
        Source::Bitmap(img) => {
            let sc = n as f64 / img.width().max(img.height()) as f64;
            let w = ((img.width() as f64 * sc).round() as u32).max(1);
            let h = ((img.height() as f64 * sc).round() as u32).max(1);
            let mut out = if (w, h) == img.dimensions() {
                img.clone()
            } else {
                image::imageops::resize(img, w, h, image::imageops::FilterType::Triangle)
            };
            if on_white {
                for p in out.pixels_mut() {
                    let a = p[3] as u32;
                    for k in 0..3 {
                        p[k] = ((p[k] as u32 * a + 255 * (255 - a) + 127) / 255) as u8;
                    }
                    p[3] = 255;
                }
            }
            Raster { w: w as usize, h: h as usize, data: out.into_raw() }
        }
    }
}

/// Colours an SVG declares in fill, stroke and stop-color, in order of appearance.
fn declared_colours(text: &str) -> Vec<Rgb> {
    let mut out: Vec<Rgb> = Vec::new();
    let lower = text.to_ascii_lowercase();
    let mut i = 0;
    while i < lower.len() {
        let rest = &lower[i..];
        let hit = ["stop-color", "stroke", "fill"].iter().find(|k| rest.starts_with(*k));
        let Some(k) = hit else {
            i += rest.chars().next().map_or(1, |c| c.len_utf8());
            continue;
        };
        let mut j = i + k.len();
        let bytes = lower.as_bytes();
        let value = if bytes.get(j) == Some(&b'=') && bytes.get(j + 1) == Some(&b'"') {
            j += 2;
            let end = lower[j..].find(['"', ';']).map_or(lower.len(), |e| j + e);
            Some(&text[j..end])
        } else {
            while bytes.get(j).is_some_and(|b| b.is_ascii_whitespace()) {
                j += 1;
            }
            if bytes.get(j) == Some(&b':') {
                j += 1;
                let end = lower[j..].find(['"', ';', '}', '\n']).map_or(lower.len(), |e| j + e);
                Some(&text[j..end])
            } else {
                None
            }
        };
        i = j.max(i + 1);
        let Some(v) = value.map(str::trim) else { continue };
        if v.is_empty() || v.starts_with("url(") || ["none", "transparent", "currentcolor", "inherit"].contains(&v.to_ascii_lowercase().as_str()) {
            continue;
        }
        if let Ok(c) = v.parse::<svgtypes::Color>() {
            let c = [c.red as i32, c.green as i32, c.blue as i32];
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    out
}

struct Attempt {
    svg: String,
    layers: usize,
    diff_pct: f64,
    diff_png: Vec<u8>,
    opts: Opts,
}

/// The palette and the original's index map for a set of options. Both depend
/// only on the colour settings, so attempts that differ in those share them.
struct Setup {
    a: Raster,
    declared: Vec<Rgb>,
    cache: Mutex<HashMap<String, std::sync::Arc<(Pal, Vec<i16>)>>>,
}

impl Setup {
    fn get(&self, o: &Opts) -> std::sync::Arc<(Pal, Vec<i16>)> {
        let key = format!("{:?}{}{}{}{}{}{:?}", o.mode, o.merge, o.edge_bg, o.thr, o.max_col, o.edges, o.choices);
        if let Some(v) = self.cache.lock().unwrap().get(&key) {
            return v.clone();
        }
        let p = if o.mode == Mode::Mono {
            Pal::mono(self.declared.first().copied().unwrap_or([0, 0, 0]))
        } else {
            slimmer_core::palette::palette_for(&self.a.image(), o, &self.declared).unwrap_or_else(|e| fail(e))
        };
        let a_idx = index::source_index(&self.a.image(), o, &p);
        let v = std::sync::Arc::new((p, a_idx));
        self.cache.lock().unwrap().insert(key, v.clone());
        v
    }
}

/// Redraw or tidy with `o`, then compare. None when Tidy can't read the SVG.
fn attempt(src: &Source, o: &Opts, setup: &Setup) -> Result<Attempt, String> {
    let mono = o.mode == Mode::Mono;
    let pa = setup.get(o);
    let (p, a_idx) = (&pa.0, &pa.1);
    let (svg, layers) = match (o.method, src) {
        (Method::Tidy, Source::Svg(text)) => {
            let t = tidy::tidy(text, o).map_err(|e| format!("Tidy can't keep this SVG's outlines: {e}"))?;
            (t.svg, t.paths_out)
        }
        (Method::Tidy, _) => return Err("--tidy needs an SVG".into()),
        (Method::Redraw, _) => {
            let r = raster(src, o.grid, mono);
            let t = trace::trace(&r.image(), o, p);
            (t.svg, t.layers)
        }
    };
    let b = raster_svg(&svg, CMP, mono);
    let c = compare::compare(a_idx, &setup.a.image(), &b.image(), o, p);
    Ok(Attempt { svg, layers, diff_pct: c.diff_pct, diff_png: c.diff_png, opts: o.clone() })
}

/// One line describing how an attempt was made.
fn describe(o: &Opts) -> String {
    match o.method {
        Method::Tidy => format!("tidy, merge {}", o.merge),
        Method::Redraw => {
            let lines = if o.alpha == 0.0 { "straight lines".to_string() } else { format!("smoothing {:.1}", o.tol) };
            format!("grid {}, {lines}, merge {}", o.grid, o.merge)
        }
    }
}

/// Coordinates in the SVG's paths, counted as points (pairs of numbers).
fn count_points(svg: &str) -> usize {
    let mut n = 0;
    for part in svg.split(" d=\"").skip(1) {
        let d = part.split('"').next().unwrap_or("").as_bytes();
        let digits = |mut j: usize| {
            while j < d.len() && d[j].is_ascii_digit() {
                j += 1;
            }
            j
        };
        let mut i = 0;
        while i < d.len() {
            // A number is -?\d*\.?\d+ : it must end in a digit.
            let s = if d[i] == b'-' { i + 1 } else { i };
            let a = digits(s);
            let end = if a < d.len() && d[a] == b'.' && digits(a + 1) > a + 1 {
                digits(a + 1)
            } else if a > s {
                a
            } else {
                i += 1;
                continue;
            };
            n += 1;
            i = end;
        }
    }
    n / 2
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{HELP}");
        return;
    }
    let mut input: Option<PathBuf> = None;
    let (mut out, mut diff_out, mut png_out, mut png_size) = (None, None, None, None);
    let (mut smallest, mut target, mut quiet) = (false, 0.2, false);
    let mut set: Vec<(String, String)> = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| fail(format!("{a} needs a value")));
        match a.as_str() {
            "-o" | "--out" => out = Some(PathBuf::from(val())),
            "--diff" => diff_out = Some(PathBuf::from(val())),
            "--png" => png_out = Some(PathBuf::from(val())),
            "--png-size" => png_size = Some(val().parse::<usize>().unwrap_or_else(|_| fail("--png-size takes a number"))),
            "--smallest" => smallest = true,
            "--target" => target = val().parse().unwrap_or_else(|_| fail("--target takes a number")),
            "-q" | "--quiet" => quiet = true,
            "--mono" | "--background" | "--no-background" | "--no-edges" | "--tidy" | "--slivers" => set.push((a.clone(), String::new())),
            "--preset" | "--grid" | "--smoothing" | "--corner" | "--protect" | "--decimals" | "--colours" | "--colors" | "--merge"
            | "--threshold" | "--specks" | "--clean" => {
                let v = val();
                set.push((a.clone(), v))
            }
            s if s.starts_with('-') => fail(format!("unknown option {s}; see --help")),
            s => {
                if input.replace(PathBuf::from(s)).is_some() {
                    fail("give one input file")
                }
            }
        }
    }
    let input = input.unwrap_or_else(|| fail("no input file"));
    let bytes = std::fs::read(&input).unwrap_or_else(|e| fail(format!("{}: {e}", input.display())));
    let is_svg = input.extension().is_some_and(|e| e.eq_ignore_ascii_case("svg")) || bytes.starts_with(b"<");
    let src = if is_svg {
        Source::Svg(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        Source::Bitmap(image::load_from_memory(&bytes).unwrap_or_else(|e| fail(format!("that image couldn't be read ({e})"))).into_rgba8())
    };

    // Defaults as the web app sets them for this kind of file.
    let mut o = Opts { edge_bg: !is_svg, max_col: if is_svg { 24 } else { 8 }, clean: if is_svg { 0 } else { 1 }, ..Opts::default() };
    let num = |k: &str, v: &str| -> f64 { v.parse().unwrap_or_else(|_| fail(format!("{k} takes a number"))) };
    for (k, v) in &set {
        match k.as_str() {
            "--preset" => match v.as_str() {
                "small" => (o.grid, o.tol, o.protect, o.dec) = (1024, 1.0, 0.0, 0),
                "balanced" => (o.grid, o.tol, o.protect, o.dec) = (2048, 0.5, 6.0, 0),
                "fine" => (o.grid, o.tol, o.protect, o.dec) = (4096, 0.2, 15.0, 1),
                _ => fail("--preset is small, balanced or fine"),
            },
            "--grid" => o.grid = num(k, v) as usize,
            "--smoothing" => o.tol = num(k, v),
            "--corner" => o.alpha = num(k, v),
            "--protect" => o.protect = num(k, v),
            "--decimals" => o.dec = num(k, v) as u32,
            "--colours" | "--colors" => o.max_col = num(k, v) as usize,
            "--merge" => o.merge = num(k, v),
            "--threshold" => o.thr = num(k, v),
            "--specks" => o.turd = num(k, v),
            "--clean" => o.clean = num(k, v) as u32,
            "--mono" => o.mode = Mode::Mono,
            "--background" => o.edge_bg = true,
            "--no-background" => o.edge_bg = false,
            "--no-edges" => o.edges = false,
            "--tidy" => o.method = Method::Tidy,
            "--slivers" => o.slivers = true,
            _ => unreachable!(),
        }
    }

    let t0 = Instant::now();
    let mono = o.mode == Mode::Mono;
    let declared = match &src {
        Source::Svg(text) => declared_colours(text),
        _ => vec![],
    };
    let setup = Setup { a: raster(&src, CMP, mono), declared, cache: Mutex::new(HashMap::new()) };

    let pick = if smallest {
        let run = |plan: &[Opts]| -> Vec<Attempt> { plan.par_iter().filter_map(|o| attempt(&src, o, &setup).ok()).collect() };
        let redraw = Opts { method: Method::Redraw, ..o.clone() };
        let mut plan: Vec<Opts> = Vec::new();
        for grid in [1024, 2048] {
            for tol in [1.0, 0.5, 0.2] {
                plan.push(Opts { grid, tol, ..redraw.clone() });
            }
            // Straight lines: on busy artwork, like pen hatching, short lines beat curves.
            plan.push(Opts { grid, tol: 0.5, alpha: 0.0, ..redraw.clone() });
        }
        if !mono {
            // Merging shades the eye can't tell apart (locked colours still stay).
            let merges: Vec<f64> = [32.0, 48.0].into_iter().filter(|&m| m > o.merge).collect();
            for &merge in &merges {
                plan.push(Opts { grid: 2048, tol: 0.5, merge, ..redraw.clone() });
                plan.push(Opts { grid: 2048, tol: 0.5, alpha: 0.0, merge, ..redraw.clone() });
            }
            if matches!(src, Source::Svg(_)) {
                for merge in std::iter::once(o.merge).chain(merges) {
                    plan.push(Opts { method: Method::Tidy, merge, ..o.clone() });
                }
            }
        }
        let mut tries = run(&plan);
        if !tries.iter().any(|t| t.diff_pct <= target) {
            tries.extend(run(&[Opts { grid: 4096, tol: 0.5, ..redraw.clone() }, Opts { grid: 4096, tol: 0.2, ..redraw.clone() }]));
        }
        if !quiet {
            for t in &tries {
                eprintln!("  {:<42} {:>8} bytes, {:.3}% different", describe(&t.opts), t.svg.len(), t.diff_pct);
            }
        }
        let within = tries.iter().filter(|t| t.diff_pct <= target).min_by_key(|t| t.svg.len()).map(|t| t as *const Attempt);
        let closest = tries.iter().min_by(|x, y| x.diff_pct.partial_cmp(&y.diff_pct).unwrap()).map(|t| t as *const Attempt);
        let chosen = within.or(closest).unwrap();
        if within.is_none() && !quiet {
            eprintln!("Nothing got within {target}%. Keeping the closest match.");
        }
        let i = tries.iter().position(|t| std::ptr::eq(t, chosen)).unwrap();
        tries.swap_remove(i)
    } else {
        attempt(&src, &o, &setup).unwrap_or_else(|e| fail(e))
    };

    let out = out.unwrap_or_else(|| {
        let stem = input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "logo".into());
        input.with_file_name(format!("{stem}.slim.svg"))
    });
    write(&out, pick.svg.as_bytes());
    if let Some(d) = &diff_out {
        write(d, &pick.diff_png);
    }
    if let Some(po) = &png_out {
        let n = png_size.unwrap_or(match &src {
            Source::Bitmap(img) => img.width().max(img.height()) as usize,
            Source::Svg(_) => 2048,
        });
        let r = raster(&src, n, mono);
        let p = &setup.get(&pick.opts).0;
        let q = png::indexed(&index::index_map(&r.image(), &pick.opts, p), r.w, r.h, &p.pal);
        write(po, &q.png);
        if !quiet {
            eprintln!("{}: {} colours{}, {} bytes", po.display(), q.colours, if q.transparent { " + transparent" } else { "" }, q.png.len());
        }
    }
    if !quiet {
        let o = &pick.opts;
        eprintln!(
            "{}: {} bytes (from {}), {} layer{}, ≈{} points, {:.3}% different; {} — {:.2} s",
            out.display(),
            pick.svg.len(),
            bytes.len(),
            pick.layers,
            if pick.layers == 1 { "" } else { "s" },
            count_points(&pick.svg),
            pick.diff_pct,
            describe(o),
            t0.elapsed().as_secs_f64()
        );
    }
}

fn write(path: &Path, data: &[u8]) {
    std::fs::write(path, data).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
}
