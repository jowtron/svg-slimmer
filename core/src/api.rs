//! One message in, one message out: how the web app talks to the core, whether
//! the core runs as WebAssembly in a worker or natively in the desktop app.
//!
//! A message is a little-endian u32 giving the length of a JSON header, the
//! header, then binary buffers back to back. The header's `bufs` lists their
//! lengths. Pixels travel as RGBA bytes and index maps as little-endian i16.
//!
//! Requests (`op`, plus what each needs; `w`/`h` give the first buffer's size):
//! - `palette`     {o, declared}      [original at CMP]      → Pal
//! - `trace`       {o, p}             [original at o.grid]   → {svg, layers}
//! - `index`       {o, p}             [original at o.grid]   → [index map]
//! - `layers`      {o, p, ks}         [index map]            → {paths}
//! - `sourceIndex` {o, p}             [original at CMP]      → [index map]
//! - `compare`     {o, p, bw, bh}     [original's index map, original, result] → {diffPct} [diff PNG]
//! - `quantize`    {o, p}             [original at any size] → {colours, transparent} [PNG]
//! - `tidy`        {o, text}          []                     → {svg, pathsIn, pathsOut, coloursIn, coloursOut}
//!                                                             or {unsupported: reason}
//!
//! Errors come back as {error}.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::palette::Pal;
use crate::{compare, index, palette, png, tidy, trace, Image, Opts, Rgb};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Header {
    op: String,
    #[serde(default)]
    bufs: Vec<usize>,
    o: Opts,
    p: Option<Pal>,
    #[serde(default)]
    declared: Vec<Rgb>,
    #[serde(default)]
    w: usize,
    #[serde(default)]
    h: usize,
    #[serde(default)]
    bw: usize,
    #[serde(default)]
    bh: usize,
    #[serde(default)]
    ks: Vec<usize>,
    #[serde(default)]
    text: String,
}

pub fn encode(head: &Value, bufs: &[&[u8]]) -> Vec<u8> {
    let mut head = head.clone();
    head["bufs"] = json!(bufs.iter().map(|b| b.len()).collect::<Vec<_>>());
    let text = serde_json::to_vec(&head).unwrap();
    let mut out = Vec::with_capacity(4 + text.len() + bufs.iter().map(|b| b.len()).sum::<usize>());
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(&text);
    for b in bufs {
        out.extend_from_slice(b);
    }
    out
}

fn i16_bytes(v: &[i16]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn bytes_i16(b: &[u8]) -> Vec<i16> {
    b.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
}

/// Handle one request message and return the response message.
pub fn handle(msg: &[u8]) -> Vec<u8> {
    match run(msg) {
        Ok(out) => out,
        Err(e) => encode(&json!({ "error": e }), &[]),
    }
}

fn run(msg: &[u8]) -> Result<Vec<u8>, String> {
    let bad = || "Malformed request.".to_string();
    let n = u32::from_le_bytes(msg.get(..4).ok_or_else(bad)?.try_into().unwrap()) as usize;
    let head: Header = serde_json::from_slice(msg.get(4..4 + n).ok_or_else(bad)?).map_err(|e| e.to_string())?;
    let mut bufs = Vec::new();
    let mut at = 4 + n;
    for &len in &head.bufs {
        bufs.push(msg.get(at..at + len).ok_or_else(bad)?);
        at += len;
    }
    let buf = |i: usize| bufs.get(i).copied().ok_or_else(bad);
    let img = |i: usize, w: usize, h: usize| -> Result<Image, String> {
        let data = buf(i)?;
        if data.len() != w * h * 4 {
            return Err(bad());
        }
        Ok(Image { w, h, data })
    };
    let pal = || head.p.as_ref().ok_or_else(bad);
    let o = &head.o;
    Ok(match head.op.as_str() {
        "palette" => {
            let p = palette::palette_for(&img(0, head.w, head.h)?, o, &head.declared)?;
            encode(&serde_json::to_value(p).unwrap(), &[])
        }
        "trace" => {
            let t = trace::trace(&img(0, head.w, head.h)?, o, pal()?);
            encode(&json!({ "svg": t.svg, "layers": t.layers }), &[])
        }
        "index" => {
            let idx = index::index_map(&img(0, head.w, head.h)?, o, pal()?);
            encode(&json!({}), &[&i16_bytes(&idx)])
        }
        "layers" => {
            let idx = bytes_i16(buf(0)?);
            if idx.len() != head.w * head.h {
                return Err(bad());
            }
            let p = pal()?;
            let paths: Vec<Option<String>> = head.ks.iter().map(|&k| trace::layer(&idx, head.w, head.h, o, p, k)).collect();
            encode(&json!({ "paths": paths }), &[])
        }
        "sourceIndex" => {
            let idx = index::source_index(&img(0, head.w, head.h)?, o, pal()?);
            encode(&json!({}), &[&i16_bytes(&idx)])
        }
        "compare" => {
            let a_idx = bytes_i16(buf(0)?);
            if a_idx.len() != head.w * head.h {
                return Err(bad());
            }
            let c = compare::compare(&a_idx, &img(1, head.w, head.h)?, &img(2, head.bw, head.bh)?, o, pal()?);
            encode(&json!({ "diffPct": c.diff_pct }), &[&c.diff_png])
        }
        "quantize" => {
            let p = pal()?;
            let idx = index::index_map(&img(0, head.w, head.h)?, o, p);
            let q = png::indexed(&idx, head.w, head.h, &p.pal);
            encode(&json!({ "colours": q.colours, "transparent": q.transparent }), &[&q.png])
        }
        "tidy" => match tidy::tidy(&head.text, o) {
            Ok(t) => encode(
                &json!({ "svg": t.svg, "pathsIn": t.paths_in, "pathsOut": t.paths_out, "coloursIn": t.colours_in, "coloursOut": t.colours_out }),
                &[],
            ),
            Err(reason) => encode(&json!({ "unsupported": reason }), &[]),
        },
        other => return Err(format!("Unknown request {other}.")),
    })
}
