//! Indexed (palette) PNGs: one small number per pixel instead of four bytes, at
//! 1, 2, 4 or 8 bits depending on how many colours are used.

use crate::Rgb;

fn crc32(bytes: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (n, e) in t.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb88320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    let mut c = 0xffffffffu32;
    for &b in bytes {
        c = t[((c ^ b as u32) & 255) as usize] ^ (c >> 8);
    }
    c ^ 0xffffffff
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let c = crc32(&out[start..]);
    out.extend_from_slice(&c.to_be_bytes());
}

/// Write a palette PNG. `pixels` holds one palette entry per pixel; `alpha`, when
/// given, is the opacity of each palette entry.
pub fn write(w: usize, h: usize, palette: &[Rgb], alpha: Option<&[u8]>, pixels: &[u8]) -> Vec<u8> {
    let n = palette.len();
    let depth = if n <= 2 {
        1
    } else if n <= 4 {
        2
    } else if n <= 16 {
        4
    } else {
        8
    };
    let row_bytes = (w * depth).div_ceil(8);
    let mut raw = vec![0u8; (row_bytes + 1) * h];
    for y in 0..h {
        let o = y * (row_bytes + 1) + 1;
        for x in 0..w {
            let v = pixels[y * w + x];
            if depth == 8 {
                raw[o + x] = v;
            } else {
                let bit = x * depth;
                raw[o + (bit >> 3)] |= v << (8 - depth - (bit & 7));
            }
        }
    }
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[depth as u8, 3, 0, 0, 0]);
    let plte: Vec<u8> = palette.iter().flat_map(|c| [c[0] as u8, c[1] as u8, c[2] as u8]).collect();
    let idat = miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6);
    let mut out = vec![137, 80, 78, 71, 13, 10, 26, 10];
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"PLTE", &plte);
    if let Some(a) = alpha {
        // tRNS may stop at the last entry that isn't fully opaque.
        let end = a.iter().rposition(|&v| v != 255).map_or(0, |i| i + 1);
        if end > 0 {
            chunk(&mut out, b"tRNS", &a[..end]);
        }
    }
    chunk(&mut out, b"IDAT", &idat);
    chunk(&mut out, b"IEND", &[]);
    out
}

pub struct Quantized {
    pub png: Vec<u8>,
    pub colours: usize,
    pub transparent: bool,
}

/// The quantized image as a PNG with only the colours it uses. Transparent comes
/// first when present, so tRNS is one byte.
pub fn indexed(idx: &[i16], w: usize, h: usize, pal: &[Rgb]) -> Quantized {
    let mut used = vec![false; pal.len() + 1];
    for &v in idx {
        used[(v + 1) as usize] = true;
    }
    let mut map = vec![0u8; pal.len() + 1];
    let mut entries: Vec<Option<Rgb>> = Vec::new();
    if used[0] {
        entries.push(None);
    }
    for i in 0..pal.len() {
        if used[i + 1] {
            map[i + 1] = entries.len() as u8;
            entries.push(Some(pal[i]));
        }
    }
    let transparent = entries.first() == Some(&None);
    let palette: Vec<Rgb> = entries.iter().map(|e| e.unwrap_or([0, 0, 0])).collect();
    let alpha: Vec<u8> = entries.iter().map(|e| if e.is_some() { 255 } else { 0 }).collect();
    let pixels: Vec<u8> = idx.iter().map(|&v| map[(v + 1) as usize]).collect();
    Quantized {
        png: write(w, h, &palette, if transparent { Some(&alpha) } else { None }, &pixels),
        colours: entries.iter().filter(|e| e.is_some()).count(),
        transparent,
    }
}
