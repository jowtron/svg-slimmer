// Wraps index.html (written as an artifact page: no <html>/<head>/<body>) into a
// complete document for static hosting, and writes it to dist/.
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';

// The core, compiled by `npm run wasm`, goes into the page itself, so dist/index.html
// still works as a single file: served, opened from disk, or published as an artifact.
const wasm = readFileSync(new URL('./target/wasm32-unknown-unknown/wasm/slimmer_wasm.wasm', import.meta.url));
const rt = readFileSync(new URL('./web/core-rt.js', import.meta.url), 'utf8');
const fill = (text, marker, value) => {
  if (!text.includes(marker)) throw new Error(`index.html: ${marker} not found`);
  return text.replace(marker, () => value);
};
let page = readFileSync(new URL('./index.html', import.meta.url), 'utf8');
page = fill(page, '/*@core-rt*/', rt);
page = fill(page, '/*@core-wasm*/', wasm.toString('base64'));
const split = page.indexOf('<div class="shell">');
if (split < 0) throw new Error('index.html: <div class="shell"> not found');
const head = page.slice(0, split).trim();
const body = page.slice(split).trim();

const icon = encodeURIComponent(
  '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect width="32" height="32" rx="7" fill="#2B59C3"/>' +
  '<path d="M7 22c3-9 7-13 11-13s6 3 7 6" fill="none" stroke="#fff" stroke-width="3" stroke-linecap="round"/>' +
  '<circle cx="7" cy="22" r="2.6" fill="#fff"/><circle cx="25" cy="15" r="2.6" fill="#fff"/></svg>'
);

const doc = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="description" content="Re-trace logos and icons, SVG or bitmap, as compact colour SVGs, or quantize them to indexed PNGs. Runs entirely in your browser.">
<link rel="icon" href="data:image/svg+xml,${icon}">
<style>:root{color-scheme:light}body{margin:0}</style>
${head}
</head>
<body>
${body}
</body>
</html>
`;

mkdirSync(new URL('./dist/', import.meta.url), { recursive: true });
writeFileSync(new URL('./dist/index.html', import.meta.url), doc);
console.log(`dist/index.html  ${(doc.length / 1000).toFixed(1)} KB (core ${(wasm.length / 1000).toFixed(0)} KB)`);
