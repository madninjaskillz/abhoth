//! WAV in, 16 kHz mono 16-bit PCM out (what Wyoming speech to text engines take).

/// Decoded audio.
#[derive(Debug, Clone, PartialEq)]
pub struct Pcm {
    /// Little-endian i16 samples, mono.
    pub data: Vec<u8>,
    pub rate: u32,
}

impl Pcm {
    pub fn seconds(&self) -> f64 {
        self.data.len() as f64 / 2.0 / self.rate.max(1) as f64
    }
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

/// Read a RIFF WAV file (PCM 8/16/24/32-bit or 32-bit float), mix it to mono and resample it to
/// `rate`.
pub fn decode(b: &[u8], rate: u32) -> Result<Pcm, String> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err("the audio must be a WAV file (RIFF/WAVE)".into());
    }
    let mut i = 12;
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    let mut data: Option<&[u8]> = None;
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let len = u32_at(b, i + 4).unwrap_or(0) as usize;
        let start = i + 8;
        let end = start.saturating_add(len).min(b.len());
        match id {
            b"fmt " => {
                let mut tag = u16_at(b, start).ok_or("a short fmt chunk")?;
                let ch = u16_at(b, start + 2).ok_or("a short fmt chunk")?;
                let sr = u32_at(b, start + 4).ok_or("a short fmt chunk")?;
                let bits = u16_at(b, start + 14).ok_or("a short fmt chunk")?;
                if tag == 0xFFFE {
                    // WAVE_FORMAT_EXTENSIBLE: the sub-format GUID starts with the real tag
                    tag = u16_at(b, start + 24).unwrap_or(1);
                }
                fmt = Some((tag, ch, sr, bits));
            }
            b"data" => {
                data = Some(&b[start..end]);
            }
            _ => {}
        }
        i = start.saturating_add(len + (len & 1));
    }
    let (tag, ch, sr, bits) = fmt.ok_or("the WAV file has no fmt chunk")?;
    let data = data.ok_or("the WAV file has no data chunk")?;
    if ch == 0 || sr == 0 {
        return Err("the WAV file names no channels or rate".into());
    }
    let width = (bits as usize).div_ceil(8);
    let frame = width * ch as usize;
    if frame == 0 {
        return Err("the WAV file names no sample size".into());
    }
    let sample = |s: &[u8]| -> Option<f32> {
        Some(match (tag, bits) {
            (1, 8) => (s[0] as f32 - 128.0) / 128.0,
            (1, 16) => i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0,
            (1, 24) => (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8_388_608.0,
            (1, 32) => i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f32 / 2_147_483_648.0,
            (3, 32) => f32::from_le_bytes([s[0], s[1], s[2], s[3]]),
            _ => return None,
        })
    };
    let mut mono: Vec<f32> = Vec::with_capacity(data.len() / frame);
    for f in data.chunks_exact(frame) {
        let mut sum = 0.0;
        for c in f.chunks_exact(width) {
            sum += sample(c).ok_or_else(|| {
                format!("WAV format {tag} with {bits}-bit samples is not supported")
            })?;
        }
        mono.push(sum / ch as f32);
    }
    let out = resample(&mono, sr, rate);
    let mut bytes = Vec::with_capacity(out.len() * 2);
    for s in out {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    Ok(Pcm { data: bytes, rate })
}

/// Linear interpolation (speech to text needs no better).
fn resample(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || x.is_empty() {
        return x.to_vec();
    }
    let n = ((x.len() as u64 * to as u64) / from as u64) as usize;
    let step = from as f64 / to as f64;
    (0..n)
        .map(|i| {
            let p = i as f64 * step;
            let j = p.floor() as usize;
            let f = (p - j as f64) as f32;
            let a = x[j.min(x.len() - 1)];
            let b = x[(j + 1).min(x.len() - 1)];
            a + (b - a) * f
        })
        .collect()
}

/// A WAV file of 16-bit mono PCM (for tests and tools).
pub fn encode(pcm: &[i16], rate: u32) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + pcm.len() * 2);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}
