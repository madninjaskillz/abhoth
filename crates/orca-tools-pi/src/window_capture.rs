//! The pure parts of `PiWindowCapture` (the Windows helpers behind `job screenshot`): key and
//! size parsing, the process-tree walk, pixel checks and the PNG encoder. Finding, resizing and
//! capturing a window needs Win32 calls through `unsafe` FFI, which the workspace forbids, so
//! `job screenshot` itself reports that it is not available in this build.

/// One key press: virtual-key code, extended flag, modifiers (VK codes) and the token as typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiKey {
    pub vk: u8,
    pub extended: bool,
    pub name: String,
    pub modifiers: Vec<u8>,
}

/// A captured window (`PiWindowShot`): title, (possibly halved) size, PNG, the share of the
/// most common colour, the client size and the flat strips along the right and bottom edges.
#[derive(Debug, Clone, PartialEq)]
pub struct PiWindowShot {
    pub title: String,
    pub width: i32,
    pub height: i32,
    pub png: Vec<u8>,
    pub dominant_share: f64,
    pub window_width: i32,
    pub window_height: i32,
    pub undrawn_right: i32,
    pub undrawn_bottom: i32,
}

impl PiWindowShot {
    /// The right and bottom edges are one flat colour (at least 10% each) while the rest has
    /// content: the app draws at a fixed size instead of the window's.
    pub fn looks_undrawn(&self) -> bool {
        self.window_width > 0
            && self.window_height > 0
            && self.undrawn_right >= self.window_width / 10
            && self.undrawn_bottom >= self.window_height / 10
            && self.undrawn_right < self.window_width * 9 / 10
            && self.undrawn_bottom < self.window_height * 9 / 10
    }
}

fn split_tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split([' ', ',', ';']).filter(|t| !t.is_empty())
}

/// Keys from text: names separated by spaces or commas ("Right Right Tab", "g", "Shift+Tab",
/// "Esc"). `Err(bad token)` for an unknown key.
pub fn parse_keys(text: Option<&str>) -> Result<Vec<PiKey>, String> {
    let mut keys = Vec::new();
    let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
        return Ok(keys);
    };
    for token in split_tokens(text) {
        let parts: Vec<&str> = token.split('+').filter(|p| !p.is_empty()).collect();
        let Some((last, mods)) = parts.split_last() else {
            return Err(token.to_string());
        };
        let mut modifiers = Vec::new();
        for m in mods {
            modifiers.push(match m.to_lowercase().as_str() {
                "shift" => 0x10,
                "ctrl" | "control" => 0x11,
                "alt" => 0x12,
                _ => return Err(token.to_string()),
            });
        }
        let Some((vk, extended)) = key_of(last) else {
            return Err(token.to_string());
        };
        keys.push(PiKey {
            vk,
            extended,
            name: token.to_string(),
            modifiers,
        });
    }
    Ok(keys)
}

fn key_of(name: &str) -> Option<(u8, bool)> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && c.is_ascii_alphanumeric()
    {
        return Some((c.to_ascii_uppercase() as u8, false));
    }
    let lower = name.to_lowercase();
    if (lower.len() == 2 || lower.len() == 3)
        && lower.starts_with('f')
        && let Ok(f) = lower[1..].parse::<u8>()
        && (1..=12).contains(&f)
    {
        return Some((0x70 + f - 1, false));
    }
    Some(match lower.as_str() {
        "left" => (0x25, true),
        "up" => (0x26, true),
        "right" => (0x27, true),
        "down" => (0x28, true),
        "tab" => (0x09, false),
        "enter" | "return" => (0x0D, false),
        "escape" | "esc" => (0x1B, false),
        "space" => (0x20, false),
        "backspace" => (0x08, false),
        "delete" | "del" => (0x2E, true),
        "home" => (0x24, true),
        "end" => (0x23, true),
        "pageup" | "pgup" => (0x21, true),
        "pagedown" | "pgdn" => (0x22, true),
        _ => return None,
    })
}

/// "1920x1080" (also "1920×1080", "1920,1080") as a size of 200..7680 × 150..4320.
pub fn parse_size(text: Option<&str>) -> Option<(i32, i32)> {
    let text = text.filter(|t| !t.trim().is_empty())?;
    let parts: Vec<&str> = text
        .split(['x', 'X', '\u{00D7}', ',', '*', ' '])
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() != 2 {
        return None;
    }
    let w: i32 = parse_dotnet_int(parts[0])?;
    let h: i32 = parse_dotnet_int(parts[1])?;
    ((200..=7680).contains(&w) && (150..=4320).contains(&h)).then_some((w, h))
}

/// `int.TryParse` (integer style: surrounding whitespace and a sign allowed).
fn parse_dotnet_int(s: &str) -> Option<i32> {
    s.trim().parse().ok()
}

/// The tree under `root` from (pid, parent) pairs.
pub fn descendants(root: u32, pairs: &[(u32, u32)]) -> std::collections::HashSet<u32> {
    let mut set = std::collections::HashSet::from([root]);
    let mut grew = true;
    while grew {
        grew = false;
        for &(pid, parent) in pairs {
            if pid != parent && set.contains(&parent) && set.insert(pid) {
                grew = true;
            }
        }
    }
    set
}

/// Crop a BGRA bitmap of `width` columns to `w`×`h` at (`x`, `y`) (clamped).
pub fn crop(bgra: &[u8], width: i32, x: i32, y: i32, w: i32, h: i32) -> Vec<u8> {
    let height = bgra.len() as i32 / 4 / width;
    let x = x.clamp(0, width);
    let y = y.clamp(0, height);
    let w = w.min(width - x);
    let h = h.min(height - y);
    let mut out = vec![0u8; (w * h * 4) as usize];
    for row in 0..h {
        let src = (((y + row) * width + x) * 4) as usize;
        let dst = (row * w * 4) as usize;
        out[dst..dst + (w * 4) as usize].copy_from_slice(&bgra[src..src + (w * 4) as usize]);
    }
    out
}

/// 2×2 box downscale.
pub fn halve(bgra: &[u8], w: i32, h: i32) -> (i32, i32, Vec<u8>) {
    let (nw, nh) = (w / 2, h / 2);
    let mut o = vec![0u8; (nw * nh * 4) as usize];
    for y in 0..nh {
        for x in 0..nw {
            for c in 0..4 {
                let i = (((2 * y) * w + 2 * x) * 4 + c) as usize;
                let row = (w * 4) as usize;
                let sum = bgra[i] as u32
                    + bgra[i + 4] as u32
                    + bgra[i + row] as u32
                    + bgra[i + row + 4] as u32;
                o[((y * nw + x) * 4 + c) as usize] = (sum / 4) as u8;
            }
        }
    }
    (nw, nh, o)
}

/// How many columns at the right edge and rows at the bottom edge are the colour of the pixel
/// just inside the bottom-right corner (sampled, ±4 per channel).
pub fn undrawn_edges(bgra: &[u8], w: i32, h: i32) -> (i32, i32) {
    if w < 20 || h < 20 || (bgra.len() as i64) < (w as i64 * h as i64 * 4) {
        return (0, 0);
    }
    let inset_x = (w / 100).max(2);
    let inset_y = (h / 100).max(2);
    let corner = (((h - 1 - inset_y) * w + (w - 1 - inset_x)) * 4) as usize;
    let same = |x: i32, y: i32| {
        let i = ((y * w + x) * 4) as usize;
        (bgra[i] as i32 - bgra[corner] as i32).abs() <= 4
            && (bgra[i + 1] as i32 - bgra[corner + 1] as i32).abs() <= 4
            && (bgra[i + 2] as i32 - bgra[corner + 2] as i32).abs() <= 4
    };
    let row_step = (h / 300).max(1);
    let col_step = (w / 300).max(1);
    let flat_column = |x: i32| {
        let (mut n, mut hits) = (0, 0);
        let mut y = 0;
        while y < h {
            if same(x, y) {
                hits += 1;
            }
            n += 1;
            y += row_step;
        }
        hits as f64 >= n as f64 * 0.98
    };
    let flat_row = |y: i32| {
        let (mut n, mut hits) = (0, 0);
        let mut x = 0;
        while x < w {
            if same(x, y) {
                hits += 1;
            }
            n += 1;
            x += col_step;
        }
        hits as f64 >= n as f64 * 0.98
    };
    let mut right = 0;
    let mut x = w - 1 - inset_x;
    while x >= 0 && flat_column(x) {
        right += 1;
        x -= 1;
    }
    let mut bottom = 0;
    let mut y = h - 1 - inset_y;
    while y >= 0 && flat_row(y) {
        bottom += 1;
        y -= 1;
    }
    (
        if right > 0 { right + inset_x } else { 0 },
        if bottom > 0 { bottom + inset_y } else { 0 },
    )
}

pub fn is_uniform(bgra: &[u8]) -> bool {
    dominant_share(bgra) > 0.995
}

/// Share of pixels (sampled) that have the most common colour: near 1 = a blank window.
pub fn dominant_share(bgra: &[u8]) -> f64 {
    let n = bgra.len() / 4;
    if n == 0 {
        return 1.0;
    }
    let step = (n / 20000).max(1);
    let mut counts: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let mut total = 0u32;
    let mut i = 0;
    while i < n {
        let k =
            ((bgra[i * 4] as u32) << 16) | ((bgra[i * 4 + 1] as u32) << 8) | bgra[i * 4 + 2] as u32;
        *counts.entry(k).or_default() += 1;
        total += 1;
        i += step;
    }
    *counts.values().max().unwrap_or(&0) as f64 / total as f64
}

/// PNG (8-bit RGB, no filter) from top-down BGRA pixels. The zlib stream uses stored
/// (uncompressed) deflate blocks: no compression crate is needed and every decoder reads it;
/// the C# build compresses (`CompressionLevel.Fastest`), so the bytes differ, not the pixels.
pub fn encode_png(bgra: &[u8], w: i32, h: i32) -> Vec<u8> {
    let (wu, hu) = (w as usize, h as usize);
    let mut raw = vec![0u8; (wu * 3 + 1) * hu];
    for y in 0..hu {
        let o = y * (wu * 3 + 1);
        for x in 0..wu {
            let i = (y * wu + x) * 4;
            raw[o + 1 + x * 3] = bgra[i + 2];
            raw[o + 2 + x * 3] = bgra[i + 1];
            raw[o + 3 + x * 3] = bgra[i];
        }
    }
    let mut out = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // bit depth 8, truecolour
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut td = Vec::with_capacity(4 + data.len());
    td.extend_from_slice(kind);
    td.extend_from_slice(data);
    out.extend_from_slice(&td);
    out.extend_from_slice(&crc32(&td).to_be_bytes());
}

fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut blocks = data.chunks(65_535).peekable();
    if blocks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(block) = blocks.next() {
        out.push(u8::from(blocks.peek().is_none()));
        let len = block.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

/// CRC-32 (IEEE), as PNG chunks use.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    c ^ 0xFFFF_FFFF
}
