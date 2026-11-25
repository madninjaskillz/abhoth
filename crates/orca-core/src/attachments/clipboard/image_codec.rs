//! `ImageCodec`: the two image conversions a clipboard paste needs, without an imaging
//! library: a Windows device-independent bitmap (CF_DIB / CF_DIBV5: a BITMAPINFOHEADER,
//! optional masks and palette, then the pixels) to RGBA, and RGBA to PNG.

use std::io::Write;

const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// A decoded image: RGBA pixels, top row first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// True when the bytes start with the PNG signature.
pub fn is_png(data: &[u8]) -> bool {
    data.len() > 8 && data[..8] == PNG_SIGNATURE
}

/// A packed DIB (as CF_DIB / CF_DIBV5 hold it) as PNG bytes; `None` when the format isn't
/// supported.
pub fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    decode_dib(dib).map(|i| encode_png(i.width, i.height, &i.rgba))
}

fn i32_le(s: &[u8], at: usize) -> i32 {
    i32::from_le_bytes(s[at..at + 4].try_into().expect("4 bytes"))
}
fn u32_le(s: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(s[at..at + 4].try_into().expect("4 bytes"))
}
fn u16_le(s: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(s[at..at + 2].try_into().expect("2 bytes"))
}

/// Decodes an uncompressed 8/24/32-bit DIB to RGBA, top row first.
pub fn decode_dib(dib: &[u8]) -> Option<RgbaImage> {
    if dib.len() < 40 {
        return None;
    }
    let header_size = i32_le(dib, 0);
    let width = i32_le(dib, 4);
    let raw_height = i32_le(dib, 8);
    let bit_count = u16_le(dib, 14);
    let compression = u32_le(dib, 16);
    let colors_used = u32_le(dib, 32);
    if header_size < 40
        || header_size as usize > dib.len()
        || width <= 0
        || raw_height == 0
        || width > 32768
        || raw_height.unsigned_abs() > 32768
    {
        return None;
    }
    let header_size = header_size as usize;
    let top_down = raw_height < 0;
    let width = width as usize;
    let height = raw_height.unsigned_abs() as usize;

    const BI_RGB: u32 = 0;
    const BI_BITFIELDS: u32 = 3;
    const BI_ALPHA_BITFIELDS: u32 = 6;
    if !matches!(compression, BI_RGB | BI_BITFIELDS | BI_ALPHA_BITFIELDS)
        || !matches!(bit_count, 8 | 24 | 32)
    {
        return None;
    }

    // Channel masks: in the header for V4/V5 (size >= 52), else right after a 40-byte header.
    let (mut r_mask, mut g_mask, mut b_mask, mut a_mask) =
        (0x00FF_0000u32, 0x0000_FF00u32, 0x0000_00FFu32, 0u32);
    let mut offset = header_size;
    if compression == BI_BITFIELDS || compression == BI_ALPHA_BITFIELDS {
        let mask_at = if header_size >= 52 { 40 } else { header_size };
        let mask_count = if compression == BI_ALPHA_BITFIELDS {
            4
        } else {
            3
        };
        if dib.len() < mask_at + mask_count * 4 {
            return None;
        }
        r_mask = u32_le(dib, mask_at);
        g_mask = u32_le(dib, mask_at + 4);
        b_mask = u32_le(dib, mask_at + 8);
        if header_size >= 56 || compression == BI_ALPHA_BITFIELDS {
            if dib.len() < mask_at + 16 {
                return None;
            }
            a_mask = u32_le(dib, mask_at + 12);
        }
        if header_size < 52 {
            offset += mask_count * 4;
        }
    } else if bit_count == 32 && header_size >= 56 {
        a_mask = 0xFF00_0000;
    }

    let mut palette: &[u8] = &[];
    if bit_count == 8 {
        let entries = if (1..=256).contains(&colors_used) {
            colors_used as usize
        } else {
            256
        };
        if dib.len() < offset + entries * 4 {
            return None;
        }
        palette = &dib[offset..offset + entries * 4];
        offset += entries * 4;
    }

    let stride = (width * bit_count as usize).div_ceil(32) * 4;
    if offset as u64 + stride as u64 * height as u64 > dib.len() as u64 {
        return None;
    }

    let mut rgba = vec![0u8; width * height * 4];
    let mut any_alpha = false;
    for y in 0..height {
        let src_row = if top_down { y } else { height - 1 - y };
        let row = &dib[offset + src_row * stride..offset + (src_row + 1) * stride];
        let mut dst = y * width * 4;
        for x in 0..width {
            match bit_count {
                8 => {
                    let i = row[x] as usize * 4;
                    if i + 3 < palette.len() {
                        rgba[dst] = palette[i + 2];
                        rgba[dst + 1] = palette[i + 1];
                        rgba[dst + 2] = palette[i];
                    }
                    rgba[dst + 3] = 255;
                }
                24 => {
                    rgba[dst] = row[x * 3 + 2];
                    rgba[dst + 1] = row[x * 3 + 1];
                    rgba[dst + 2] = row[x * 3];
                    rgba[dst + 3] = 255;
                }
                _ => {
                    let px = u32_le(row, x * 4);
                    rgba[dst] = channel(px, r_mask);
                    rgba[dst + 1] = channel(px, g_mask);
                    rgba[dst + 2] = channel(px, b_mask);
                    let a = if a_mask == 0 {
                        (px >> 24) as u8
                    } else {
                        channel(px, a_mask)
                    };
                    rgba[dst + 3] = a;
                    any_alpha |= a != 0;
                }
            }
            dst += 4;
        }
    }

    // A 32-bit DIB whose alpha byte is all zero is opaque (the fourth byte is padding):
    // screenshots copied by most apps look like that, and would otherwise paste invisible.
    if bit_count == 32 && !any_alpha {
        for a in rgba.iter_mut().skip(3).step_by(4) {
            *a = 255;
        }
    }

    Some(RgbaImage {
        width: width as u32,
        height: height as u32,
        rgba,
    })
}

fn channel(pixel: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let bits = mask.count_ones();
    let value = (pixel & mask) >> shift;
    if bits >= 8 {
        (value >> (bits - 8)) as u8
    } else {
        (value * 255 / ((1u32 << bits) - 1)) as u8
    }
}

/// RGBA pixels (top row first) as an 8-bit RGBA PNG.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len() / 2 + 64);
    out.extend_from_slice(&PNG_SIGNATURE);

    let mut ihdr = [0u8; 13];
    ihdr[..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    ihdr[8] = 8; // bit depth
    ihdr[9] = 6; // colour type: RGBA
    write_chunk(&mut out, b"IHDR", &ihdr);

    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    let row_bytes = width as usize * 4;
    for y in 0..height as usize {
        // filter: none, then the row
        let _ = z.write_all(&[0]);
        let _ = z.write_all(&rgba[y * row_bytes..(y + 1) * row_bytes]);
    }
    let idat = z.finish().unwrap_or_default();
    write_chunk(&mut out, b"IDAT", &idat);
    write_chunk(&mut out, b"IEND", &[]);
    out
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(kind, data).to_be_bytes());
}

static CRC_TABLE: std::sync::LazyLock<[u32; 256]> = std::sync::LazyLock::new(|| {
    let mut table = [0u32; 256];
    for (n, slot) in table.iter_mut().enumerate() {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *slot = c;
    }
    table
});

fn crc32(kind: &[u8], data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in kind.iter().chain(data) {
        c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}
