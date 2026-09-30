//! SVG Slimmer as a desktop app. The window shows the same page as the web app;
//! the page sends its requests here and the core runs natively, on every core,
//! instead of as WebAssembly.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::ipc::{InvokeBody, Request, Response};
use tauri_plugin_dialog::DialogExt;

fn raw_body(request: &Request<'_>) -> Result<Vec<u8>, String> {
    match request.body() {
        InvokeBody::Raw(bytes) => Ok(bytes.clone()),
        _ => Err("Expected a binary request.".into()),
    }
}

/// One request message for the core (see core/src/api.rs), answered off the main thread.
#[tauri::command]
async fn core(request: Request<'_>) -> Result<Response, String> {
    let t = std::time::Instant::now();
    let msg = raw_body(&request)?;
    let len = msg.len();
    let t_copy = t.elapsed();
    let out = tauri::async_runtime::spawn_blocking(move || slimmer_core::api::handle(&msg)).await.map_err(|e| e.to_string())?;
    if std::env::var_os("SLIMMER_SELFTEST").is_some() {
        eprintln!("core: {} MB in, copy {:?}, total {:?}, {} KB out", len / 1_000_000, t_copy, t.elapsed(), out.len() / 1000);
    }
    Ok(Response::new(out))
}

/// Save the request body to a file the user picks. The suggested name travels,
/// percent-encoded, in the `x-name` header. Returns "saved" or "cancelled".
#[tauri::command]
async fn save(app: tauri::AppHandle, request: Request<'_>) -> Result<String, String> {
    let data = raw_body(&request)?;
    let name = request
        .headers()
        .get("x-name")
        .and_then(|v| v.to_str().ok())
        .map(|v| percent_encoding::percent_decode_str(v).decode_utf8_lossy().into_owned())
        .unwrap_or_else(|| "untitled".into());
    let Some(path) = app.dialog().file().set_file_name(&name).blocking_save_file() else {
        return Ok("cancelled".into());
    };
    let path = path.into_path().map_err(|e| e.to_string())?;
    std::fs::write(&path, data).map_err(|e| format!("Couldn't save {}: {e}", path.display()))?;
    Ok("saved".into())
}

/// Self-test: with SLIMMER_SELFTEST=<image>, the page gets that image dropped on it,
/// runs a trace, Find smallest and the Finest preset through the real UI, and
/// reports each step here. The app prints the lines and quits.
const SELFTEST: &str = r#"(async () => {
  const say = (line) => window.__TAURI_INTERNALS__.invoke('selftest', { line });
  try {
    const $ = (id) => document.getElementById(id), wait = async (ok) => { while (!ok()) await new Promise((r) => setTimeout(r, 10)); };
    const bin = atob(DATA), u = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) u[i] = bin.charCodeAt(i);
    const dt = new DataTransfer();
    dt.items.add(new File([u], NAME, { type: /\.svg$/i.test(NAME) ? 'image/svg+xml' : 'image/png' }));
    const shown = () => `${$('footB').textContent.replace(/\s+/g, ' ')} | ${$('pill').textContent}`;
    let t = performance.now();
    document.dispatchEvent(new DragEvent('drop', { dataTransfer: dt, bubbles: true, cancelable: true }));
    await wait(() => /grid/.test($('footB').textContent));
    await say(`open + trace: ${Math.round(performance.now() - t)} ms | ${shown()}`);
    t = performance.now();
    $('autoBtn').click();
    await wait(() => !$('autoBtn').disabled);
    await say(`find smallest: ${Math.round(performance.now() - t)} ms | ${$('status').textContent} | ${shown()}`);
    const before = $('footB').textContent;
    t = performance.now();
    document.querySelector('#presetSeg button[data-v="fine"]').click();
    await wait(() => $('footB').textContent !== before && /4096/.test($('footB').textContent));
    await say(`finest (incl. 350 ms settle): ${Math.round(performance.now() - t)} ms | ${shown()}`);
  } catch (e) { await say(`ERROR ${e && e.message}`); }
  await say('done');
})();"#;

#[tauri::command]
fn selftest(app: tauri::AppHandle, line: String) {
    println!("{line}");
    if line == "done" {
        app.exit(0);
    }
}

fn main() {
    let selftest_file = std::env::var_os("SLIMMER_SELFTEST").map(std::path::PathBuf::from);
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![core, save, selftest])
        .on_page_load(move |webview, payload| {
            let Some(path) = &selftest_file else { return };
            if payload.event() != tauri::webview::PageLoadEvent::Finished {
                return;
            }
            let bytes = std::fs::read(path).expect("SLIMMER_SELFTEST: can't read the file");
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let b64 = base64(&bytes);
            let script = format!("const DATA = '{b64}', NAME = {name:?};\n{SELFTEST}");
            webview.eval(&script).expect("SLIMMER_SELFTEST: eval failed");
        })
        .run(tauri::generate_context!())
        .expect("error while running SVG Slimmer");
}

fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for k in 0..4 {
            out.push(if k <= c.len() { A[(n >> (18 - 6 * k)) as usize & 63] as char } else { '=' });
        }
    }
    out
}
