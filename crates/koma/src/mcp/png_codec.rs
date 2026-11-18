//! Self-contained RFC-2083 (`image/png`) encoder & safe sealed `memfd` reader.
//!
//! Converts raw `RGBA8` scanlines from `koma`'s write-sealed `SCM_RIGHTS` `memfd` directly into
//! standard PNG bytes (with optional `max_edge` box/nearest downscaling) using zero external C
//! `libpng` or compression crate dependencies.

use std::io;

#[cfg(unix)]
use std::{
    fs::File,
    io::{Read as _, Seek as _, SeekFrom},
    os::fd::OwnedFd,
};

const PNG_SIGNATURE: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
const MAX_DEFLATE_STORE_BLOCK: usize = 65_535;

/// Reads a write-sealed `SCM_RIGHTS` `memfd` safely into memory, optionally downscales if
/// `max_edge > 0`, and encodes an RFC-2083 `image/png`.
#[cfg(unix)]
pub fn read_sealed_memfd_and_encode_png(
    fd: OwnedFd,
    width: u32,
    height: u32,
    byte_len: usize,
    max_edge: u32,
) -> io::Result<(u32, u32, Vec<u8>)> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame dimensions overflow"))?;
    if byte_len < expected || expected == 0 || expected > isize::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("memfd byte_len ({byte_len}) smaller than width*height*4 ({expected})"),
        ));
    }

    let mut f = File::from(fd);
    f.seek(SeekFrom::Start(0))?;
    let mut buf = vec![0_u8; expected];
    f.read_exact(&mut buf)?;

    let (out_w, out_h, scaled_buf);
    let target_pixels: &[u8] = if max_edge > 0 && (width > max_edge || height > max_edge) {
        let (dw, dh, scaled) = downscale_rgba8(&buf, width, height, max_edge);
        out_w = dw;
        out_h = dh;
        scaled_buf = scaled;
        &scaled_buf
    } else {
        out_w = width;
        out_h = height;
        &buf
    };
    let png_bytes = encode_rgba8_to_png(out_w, out_h, target_pixels)?;
    Ok((out_w, out_h, png_bytes))
}

#[must_use]
pub fn downscale_rgba8(src: &[u8], src_w: u32, src_h: u32, max_edge: u32) -> (u32, u32, Vec<u8>) {
    let scale = f64::from(max_edge) / f64::from(src_w.max(src_h));
    let dst_w = ((f64::from(src_w) * scale).round() as u32).max(1);
    let dst_h = ((f64::from(src_h) * scale).round() as u32).max(1);
    let mut out = vec![0_u8; (dst_w as usize) * (dst_h as usize) * 4];

    for dy in 0..dst_h {
        let sy = ((u64::from(dy) * u64::from(src_h)) / u64::from(dst_h)) as usize;
        for dx in 0..dst_w {
            let sx = ((u64::from(dx) * u64::from(src_w)) / u64::from(dst_w)) as usize;
            let s_off = (sy * (src_w as usize) + sx) * 4;
            let d_off = ((dy as usize) * (dst_w as usize) + (dx as usize)) * 4;
            out[d_off..d_off + 4].copy_from_slice(&src[s_off..s_off + 4]);
        }
    }
    (dst_w, dst_h, out)
}

/// Encodes raw tightly-packed `RGBA8` pixels (`width * height * 4` bytes) into a valid RFC-2083 PNG.
pub fn encode_rgba8_to_png(width: u32, height: u32, rgba: &[u8]) -> io::Result<Vec<u8>> {
    let row_bytes = (width as usize) * 4;
    let expected = row_bytes.saturating_mul(height as usize);
    if width == 0 || height == 0 || rgba.len() < expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid RGBA8 dimensions or buffer length for PNG encode",
        ));
    }

    // Build filtered scanlines (filter type 0 = None prepended to each scanline)
    let raw_len = (row_bytes + 1) * (height as usize);
    let mut filtered = Vec::with_capacity(raw_len);
    for row in 0..(height as usize) {
        filtered.push(0_u8); // Filter 0 (None)
        let start = row * row_bytes;
        filtered.extend_from_slice(&rgba[start..start + row_bytes]);
    }

    // Wrap filtered scanlines inside RFC-1950 zlib stream with RFC-1951 stored DEFLATE blocks
    let num_blocks = filtered.len().div_ceil(MAX_DEFLATE_STORE_BLOCK).max(1);
    let mut zlib_data = Vec::with_capacity(2 + filtered.len() + num_blocks * 5 + 4);
    zlib_data.extend_from_slice(&[0x78, 0x01]); // CMF=0x78 (32K window, deflate), FLG=0x01 (check bits)

    let chunks: Vec<&[u8]> = if filtered.is_empty() {
        vec![&[]]
    } else {
        filtered.chunks(MAX_DEFLATE_STORE_BLOCK).collect()
    };
    for (idx, block) in chunks.iter().enumerate() {
        let is_final = u8::from(idx + 1 == chunks.len());
        zlib_data.push(is_final); // BFINAL | BTYPE=00 (stored)
        let len = block.len() as u16;
        let nlen = !len;
        zlib_data.extend_from_slice(&len.to_le_bytes());
        zlib_data.extend_from_slice(&nlen.to_le_bytes());
        zlib_data.extend_from_slice(block);
    }
    zlib_data.extend_from_slice(&adler32(&filtered).to_be_bytes());

    // Assemble PNG file: signature + IHDR + IDAT + IEND
    let mut out = Vec::with_capacity(64 + zlib_data.len());
    out.extend_from_slice(&PNG_SIGNATURE);

    let mut ihdr = [0_u8; 13];
    ihdr[0..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    ihdr[8] = 8; // Bit depth = 8
    ihdr[9] = 6; // Color type = 6 (Truecolour with alpha RGBA)
    ihdr[10] = 0; // Compression = 0
    ihdr[11] = 0; // Filter = 0
    ihdr[12] = 0; // Interlace = 0
    write_png_chunk(&mut out, b"IHDR", &ihdr);
    write_png_chunk(&mut out, b"IDAT", &zlib_data);
    write_png_chunk(&mut out, b"IEND", &[]);

    Ok(out)
}

fn write_png_chunk(out: &mut Vec<u8>, chunk_type: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let crc_start = out.len();
    out.extend_from_slice(chunk_type);
    out.extend_from_slice(data);
    let crc = crc32_ieee(&out[crc_start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn adler32(bytes: &[u8]) -> u32 {
    const MOD_ADLER: u32 = 65_521;
    let mut s1: u32 = 1;
    let mut s2: u32 = 0;
    for chunk in bytes.chunks(5_552) {
        for &b in chunk {
            s1 += u32::from(b);
            s2 += s1;
        }
        s1 %= MOD_ADLER;
        s2 %= MOD_ADLER;
    }
    (s2 << 16) | s1
}

fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_valid_rgba8_png_header_and_chunks() {
        let pixels = [
            255, 0, 0, 255, 0, 255, 0, 255, //
            0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let png = encode_rgba8_to_png(2, 2, &pixels).unwrap();
        assert_eq!(&png[0..8], &PNG_SIGNATURE);
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }
}
