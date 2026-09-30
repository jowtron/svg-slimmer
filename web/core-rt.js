// Talking to slimmer-core: messages are a u32 header length, a JSON header and
// binary buffers (see core/src/api.rs). Runs in the page, in workers and in Node.
const slimEnc = new TextEncoder(), slimDec = new TextDecoder();

function slimPack(head, bufs = []) {
  const text = slimEnc.encode(JSON.stringify({ ...head, bufs: bufs.map((b) => b.byteLength) }));
  const out = new Uint8Array(4 + text.length + bufs.reduce((a, b) => a + b.byteLength, 0));
  new DataView(out.buffer).setUint32(0, text.length, true);
  out.set(text, 4);
  let at = 4 + text.length;
  for (const b of bufs) { out.set(new Uint8Array(b.buffer || b, b.byteOffset || 0, b.byteLength), at); at += b.byteLength; }
  return out;
}

function slimUnpack(msg) {
  const n = new DataView(msg.buffer, msg.byteOffset).getUint32(0, true);
  const head = JSON.parse(slimDec.decode(msg.subarray(4, 4 + n)));
  let at = 4 + n;
  const bufs = (head.bufs || []).map((len) => { const b = msg.slice(at, at + len); at += len; return b; });
  return { head, bufs };
}

// Call the WebAssembly build's raw ABI with one packed message; returns the packed response.
function slimWasmCall(exports, msg) {
  const ptr = exports.alloc(msg.length);
  new Uint8Array(exports.memory.buffer, ptr, msg.length).set(msg);
  const out = exports.run(ptr, msg.length); // frees the request
  const len = new DataView(exports.memory.buffer).getUint32(out, true);
  const res = new Uint8Array(exports.memory.buffer, out + 4, len).slice();
  exports.dealloc(out, len + 4);
  return res;
}

