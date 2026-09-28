# SVG Slimmer

Re-trace logos and icons as compact SVGs, colour by colour, or quantize them to small indexed PNGs. It runs entirely in the browser: nothing is uploaded.

**Use it:** https://svg-slimmer.jderrick.app

Auto-tracers such as VTracer, and exports from design tools, often produce SVGs made of thousands of short straight segments, plus editor metadata. SVG Slimmer redraws the artwork as long smooth curves, then checks the result against the original pixel by pixel so you can see exactly what changed. A traced logo typically comes out 70–90% smaller with no visible difference.

## What it does

- **Opens SVGs and bitmaps** (PNG, JPEG, WebP). Drop a file anywhere on the page, pick one, or paste SVG code.
- **Keeps colours.** It finds the palette, traces each colour as a stacked layer (each layer runs under the ones above it, so no gaps open between colours), and snaps to the exact colours an SVG declares.
- **Lets you choose the palette.** Set the most colours to keep and how similar two colours must be to merge. Click a colour once to lock it in, so it survives however far you reduce; again to leave it out; a third time to hand it back. Each colour shows its share of the artwork.
- **Handles AI images and upscales.** Blended edge pixels only take one of the colours actually meeting at that edge, so no fringe of a third colour appears. Specks and upscaler halos are removed before tracing, and the background is removed only where it touches the border, so white inside the artwork stays white.
- **Balances size against detail.** Presets run from Smallest to Finest. "Protect small shapes" keeps eyes, letters and thin lines sharp while big curves are still simplified.
- **Shows what changed.** A difference view marks changed pixels in red, and a zoom moves both views together. The verdict ignores edges that moved by less than a pixel.
- **Find smallest** tries each detail and smoothing setting, and keeps the smallest file that stays within the difference you allow.
- **Exports** the slimmed SVG, or a **quantized indexed PNG** of the original: the image reduced to the palette, written as a true palette PNG at 1, 2, 4 or 8 bits per pixel, with a transparent background if you removed it.

It works best on flat artwork: logos, icons and illustrations with a handful of solid colours. Gradients and soft shadows are reduced to the nearest solid colour, and text is traced as shapes.

## How it works

1. The source is drawn at the trace size (1024, 2048 or 4096 px on the long side).
2. **Palette:** a histogram of solid pixels only, so blended edge pixels never become colours of their own; similar colours merged; reduced to the colour limit by merging the closest pair (weighted so small, distinct colours survive); refined with k-means; your locked and removed colours applied.
3. **Quantize:** every pixel takes its nearest palette colour. Blended edge pixels are restricted to the colours of the nearest solid pixels around them. The background is flood-filled from the border. A majority filter and speck removal clean the map.
4. **Trace:** each colour becomes a mask covering itself and everything above it, traced with [Potrace](https://potrace.sourceforge.net/). Small shapes can take a lightly smoothed second trace. Paths are rewritten as rounded relative commands.
5. **Compare:** original and result are drawn at 2048 px and compared colour by colour, with a one-pixel tolerance.

## Develop and deploy

It's a single file, `index.html`, with no dependencies to install. `build.mjs` wraps it into a full HTML document in `dist/`, and it's served as static assets from a Cloudflare Worker.

```sh
npm run build     # writes dist/index.html
npm run dev       # build, then serve locally with wrangler
npm run deploy    # build, then deploy to Cloudflare (needs wrangler login or CLOUDFLARE_API_TOKEN)
```

You can also open `dist/index.html` straight from disk.

## Credits

Tracing by [Potrace](https://potrace.sourceforge.net/) by Peter Selinger, using the JavaScript port by kilobtye, loaded from the [`potrace-browser`](https://www.npmjs.com/package/potrace-browser) package on jsDelivr. Potrace is licensed under the GPL.
