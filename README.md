# SVG Slimmer

Re-trace logos and icons as compact SVGs, colour by colour, or quantize them to small indexed PNGs. It runs entirely on your own machine, in the browser or as a desktop app: nothing is uploaded.

**Use it:** https://svg-slimmer.jderrick.app

Auto-tracers such as VTracer, and exports from design tools, often produce SVGs made of thousands of short straight segments, plus editor metadata. SVG Slimmer redraws the artwork as long smooth curves, then checks the result against the original pixel by pixel so you can see exactly what changed. A traced logo typically comes out 70–90% smaller with no visible difference.

## What it does

- **Opens SVGs and bitmaps** (PNG, JPEG, WebP). Drop a file anywhere on the page, pick one, or paste SVG code.
- **Keeps colours.** It finds the palette, traces each colour as a stacked layer (each layer runs under the ones above it, so no gaps open between colours), and snaps to the exact colours an SVG declares.
- **Lets you choose the palette.** Set the most colours to keep and how similar two colours must be to merge. Click a colour once to lock it in, so it survives however far you reduce; again to leave it out; a third time to hand it back. Each colour shows its share of the artwork.
- **Handles AI images and upscales.** Blended edge pixels only take one of the colours actually meeting at that edge, so no fringe of a third colour appears. Specks and upscaler halos are removed before tracing, and the background is removed only where it touches the border, so white inside the artwork stays white.
- **Balances size against detail.** Presets run from Smallest to Finest. "Protect small shapes" keeps eyes, letters and thin lines sharp while big curves are still simplified.
- **Shows what changed.** A difference view marks changed pixels in red, and a zoom moves both views together. The verdict ignores edges that moved by less than a pixel.
- **Tidies SVGs that are already well traced.** Redrawing a good trace, such as one from VTracer, only loses detail, and on busy artwork like pen hatching the curves cost more bytes than the straight lines they replace. *Tidy* keeps the SVG's own outlines and only merges near-identical colours, joins shapes of one colour into a single path wherever that can't change what's painted over what, and rewrites the path data compactly. It reads flat SVGs, the kind tracers write; anything with strokes, styles, gradients or other shapes is left to *Redraw*. A 166 KB VTracer drawing comes out 94 KB with 0.01% of pixels changed.
- **Straight lines when they're smaller.** Corners at 0 draws every edge as a straight line, like VTracer's polygon mode.
- **Keep thin slivers** stops speck removal filling the gaps between pen strokes, so sketches don't darken; round specks still go.
- **Find smallest** tries each detail and smoothing setting, straight lines, stronger colour merging and, for an SVG, Tidy, and keeps the smallest file that stays within the difference you allow.
- **Exports** the slimmed SVG, or a **quantized indexed PNG** of the original: the image reduced to the palette, written as a true palette PNG at 1, 2, 4 or 8 bits per pixel, with a transparent background if you removed it.

It works best on flat artwork: logos, icons and illustrations with a handful of solid colours. Gradients and soft shadows are reduced to the nearest solid colour, and text is traced as shapes.

## How it works

1. The source is drawn at the trace size (1024, 2048 or 4096 px on the long side).
2. **Palette:** a histogram of solid pixels only, so blended edge pixels never become colours of their own; similar colours merged; reduced to the colour limit by merging the closest pair (weighted so small, distinct colours survive); refined with k-means; your locked and removed colours applied.
3. **Quantize:** every pixel takes its nearest palette colour. Blended edge pixels are restricted to the colours of the nearest solid pixels around them. The background is flood-filled from the border. A majority filter and speck removal clean the map.
4. **Trace** (Redraw): each colour becomes a mask covering itself and everything above it, traced with [Potrace](https://potrace.sourceforge.net/). Small shapes can take a lightly smoothed second trace. Paths are rewritten as rounded relative commands.
5. **Compare:** original and result are drawn at 2048 px and compared colour by colour, with a one-pixel tolerance.

**Tidy** skips steps 1 to 4: it reads the SVG's paths directly (`core/src/tidy.rs`), snaps fills within *Merge similar colours* to the most used of them, joins each path to the latest earlier path of its colour when nothing painted in between overlaps it and it overlaps nothing in that path (by bounding box, so fill rules can't open holes), and writes the data as relative or absolute commands, whichever is shorter, with `h`/`v` for level lines. Step 5 still checks the result.

## Develop and deploy

The pipeline (palette, quantize, trace, compare) is Rust, in `core/`, and runs in three places:

- **The web app** (`index.html`) runs it as WebAssembly in a pool of Web Workers, one request per worker, so "Find smallest" tries its settings in parallel and multi-colour logos trace their layers in parallel. Where workers aren't allowed it runs on the page's own thread.
- **The command line**: `svg-slim` in `cli/`.
- **The desktop app** in `src-tauri/` (Tauri), which shows the same page in the system's web view and runs the core natively, on every core. It saves through the system's Save dialog.

The page draws images with the browser's canvas and hands the core RGBA pixels, so an SVG looks exactly as it does in a browser. Requests and replies are single binary messages, described in `core/src/api.rs`; `web/core-rt.js` packs them.

```sh
npm run build     # compiles the core to WebAssembly, then writes dist/index.html with it inlined
npm run dev       # build, then serve locally with wrangler
npm run deploy    # build, then deploy to Cloudflare (needs wrangler login or CLOUDFLARE_API_TOKEN)
```

Building needs Rust with the `wasm32-unknown-unknown` target (`rustup target add wasm32-unknown-unknown`). `dist/index.html` is a single file and also works opened straight from disk.

### Command line

```sh
cargo install --path cli
svg-slim logo.png                       # writes logo.slim.svg with the Balanced preset
svg-slim logo.svg --preset fine -o out.svg
svg-slim logo.png --smallest --target 0.5 --png logo-quantized.png
svg-slim --help
```

### Desktop app

```sh
cargo install tauri-cli --version "^2"   # once
cargo tauri dev                          # build the page, then run the app
cargo tauri build                        # SVG Slimmer.app and a .dmg in target/release/bundle/
```

`SLIMMER_SELFTEST=logo.png target/release/svg-slimmer-app` drops the image on the page, runs a trace, Find smallest and the Finest preset through the real UI, prints the timings and each native request, and quits.

### Checking the port

The Rust pipeline started as a port of the JavaScript this app used to run, and keeps its arithmetic, JS rounding rules included, so it gives byte-identical SVGs from the same pixels. `core/examples/parity.rs` checks that against reference data dumped from the JS version: raw pixels, palette, index maps, SVG and difference score for each case.

```sh
cargo run --release -p slimmer-core --example parity -- <case dir>...
```

## Credits

Tracing by [Potrace](https://potrace.sourceforge.net/) by Peter Selinger, ported to Rust (`core/src/potrace.rs`) from kilobtye's JavaScript port, the [`potrace-browser`](https://www.npmjs.com/package/potrace-browser) package. Potrace is licensed under the GPL, and so is this project.
