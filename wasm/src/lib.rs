//! The core as WebAssembly with a raw ABI, so the page needs no generated glue.
//!
//! The page copies a request message (see `slimmer_core::api`) into memory from
//! `alloc`, calls `run`, which frees the request and returns a pointer to the
//! response: a little-endian u32 length, then the message. The page frees it
//! with `dealloc(ptr, length + 4)`.

use std::alloc::{alloc as sys_alloc, dealloc as sys_dealloc, Layout};

#[no_mangle]
pub extern "C" fn alloc(len: usize) -> *mut u8 {
    unsafe { sys_alloc(Layout::from_size_align(len.max(1), 8).unwrap()) }
}

#[no_mangle]
pub extern "C" fn dealloc(ptr: *mut u8, len: usize) {
    unsafe { sys_dealloc(ptr, Layout::from_size_align(len.max(1), 8).unwrap()) }
}

#[no_mangle]
pub extern "C" fn run(ptr: *mut u8, len: usize) -> *mut u8 {
    let req = unsafe { std::slice::from_raw_parts(ptr, len) };
    let res = slimmer_core::api::handle(req);
    dealloc(ptr, len);
    let out = alloc(res.len() + 4);
    unsafe {
        std::ptr::copy_nonoverlapping((res.len() as u32).to_le_bytes().as_ptr(), out, 4);
        std::ptr::copy_nonoverlapping(res.as_ptr(), out.add(4), res.len());
    }
    out
}
