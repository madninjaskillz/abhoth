//! Port of Pi's OutputAccumulator (output-accumulator.ts, `PiOutputAccumulator`): streams raw
//! shell output through a streaming UTF-8 decoder, keeps a bounded decoded tail for the
//! snapshot, and spills the raw bytes to `%TEMP%/<prefix>-<16 hex>.log` once the output
//! exceeds the limits.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use crate::truncate::{self, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, Truncation};

/// A streaming UTF-8 decoder (invalid sequences become U+FFFD, as .NET's replacement decoder).
#[derive(Default)]
pub struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    pub fn decode(&mut self, data: &[u8], flush: bool) -> String {
        self.pending.extend_from_slice(data);
        let mut out = String::new();
        let mut rest: &[u8] = &self.pending;
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    out.push_str(s);
                    rest = &[];
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    out.push_str(std::str::from_utf8(&rest[..valid]).unwrap_or_default());
                    match e.error_len() {
                        Some(n) => {
                            out.push('\u{FFFD}');
                            rest = &rest[valid + n..];
                        }
                        None => {
                            rest = &rest[valid..];
                            if flush {
                                out.push('\u{FFFD}');
                                rest = &[];
                            }
                            break;
                        }
                    }
                }
            }
        }
        self.pending = rest.to_vec();
        out
    }
}

pub struct OutputAccumulator {
    max_lines: u64,
    max_bytes: usize,
    max_rolling_bytes: usize,
    temp_file_prefix: String,
    decoder: Utf8Stream,
    raw_chunks: Vec<Vec<u8>>,
    tail_text: String,
    tail_starts_at_line_boundary: bool,
    total_raw_bytes: u64,
    total_decoded_bytes: u64,
    completed_lines: usize,
    total_lines: usize,
    current_line_bytes: usize,
    has_open_line: bool,
    started: bool,
    finished: bool,
    temp_file: Option<File>,
    full_output_path: Option<PathBuf>,
}

impl OutputAccumulator {
    pub fn new(temp_file_prefix: &str) -> Self {
        Self::with_limits(temp_file_prefix, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES)
    }

    pub fn with_limits(temp_file_prefix: &str, max_lines: u64, max_bytes: usize) -> Self {
        Self {
            max_lines,
            max_bytes,
            max_rolling_bytes: (max_bytes * 2).max(1),
            temp_file_prefix: temp_file_prefix.to_string(),
            decoder: Utf8Stream::default(),
            raw_chunks: Vec::new(),
            tail_text: String::new(),
            tail_starts_at_line_boundary: true,
            total_raw_bytes: 0,
            total_decoded_bytes: 0,
            completed_lines: 0,
            total_lines: 0,
            current_line_bytes: 0,
            has_open_line: false,
            started: false,
            finished: false,
            temp_file: None,
            full_output_path: None,
        }
    }

    /// Where the full output went (set once it exceeded the limits).
    pub fn full_output_path(&self) -> Option<&PathBuf> {
        self.full_output_path.as_ref()
    }

    pub fn last_line_bytes(&self) -> usize {
        self.current_line_bytes
    }

    pub fn append(&mut self, data: &[u8]) {
        if self.finished {
            return;
        }
        self.total_raw_bytes += data.len() as u64;
        let text = self.decode(data, false);
        self.append_decoded(&text);
        if self.temp_file.is_some() || self.should_use_temp_file() {
            self.ensure_temp_file();
            if let Some(f) = self.temp_file.as_mut() {
                let _ = f.write_all(data);
            }
        } else if !data.is_empty() {
            self.raw_chunks.push(data.to_vec());
        }
    }

    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let text = self.decode(&[], true);
        self.append_decoded(&text);
        if self.should_use_temp_file() {
            self.ensure_temp_file();
        }
        if let Some(f) = self.temp_file.as_mut() {
            let _ = f.flush();
        }
    }

    /// Pi's `snapshot({ persistIfTruncated: true })`.
    pub fn snapshot(&mut self) -> Truncation {
        let mut tr = truncate::tail(self.snapshot_text(), self.max_bytes, self.max_lines);
        let truncated = self.total_lines as u64 > self.max_lines
            || self.total_decoded_bytes > self.max_bytes as u64;
        let truncated_by = if truncated {
            tr.truncated_by
                .or(Some(if self.total_decoded_bytes > self.max_bytes as u64 {
                    "bytes"
                } else {
                    "lines"
                }))
        } else {
            None
        };
        if truncated {
            self.ensure_temp_file();
        }
        tr.truncated = truncated;
        tr.truncated_by = truncated_by;
        tr.total_lines = self.total_lines;
        tr.total_bytes = self.total_decoded_bytes.min(i32::MAX as u64) as usize;
        tr.max_lines = self.max_lines;
        tr.max_bytes = self.max_bytes;
        tr
    }

    fn decode(&mut self, data: &[u8], flush: bool) -> String {
        let mut text = self.decoder.decode(data, flush);
        if !self.started && !text.is_empty() {
            self.started = true;
            if let Some(rest) = text.strip_prefix('\u{FEFF}') {
                text = rest.to_string(); // TextDecoder's default ignoreBOM: false
            }
        }
        text
    }

    fn append_decoded(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let bytes = text.len();
        self.total_decoded_bytes += bytes as u64;
        self.tail_text.push_str(text);
        if self.tail_text.len() > self.max_rolling_bytes * 2 {
            self.trim_tail();
        }
        match text.rfind('\n') {
            None => {
                self.current_line_bytes += bytes;
                self.has_open_line = true;
            }
            Some(last_nl) => {
                self.completed_lines += text.matches('\n').count();
                self.current_line_bytes = text.len() - (last_nl + 1);
                self.has_open_line = last_nl < text.len() - 1;
            }
        }
        self.total_lines = self.completed_lines + usize::from(self.has_open_line);
    }

    fn trim_tail(&mut self) {
        let len = self.tail_text.len();
        if len <= self.max_rolling_bytes {
            return;
        }
        let mut start = len - self.max_rolling_bytes;
        while start < len && !self.tail_text.is_char_boundary(start) {
            start += 1;
        }
        if start != 0 {
            self.tail_starts_at_line_boundary = self.tail_text.as_bytes()[start - 1] == b'\n';
        }
        self.tail_text = self.tail_text[start..].to_string();
    }

    fn snapshot_text(&self) -> &str {
        if self.tail_starts_at_line_boundary {
            return &self.tail_text;
        }
        match self.tail_text.find('\n') {
            Some(i) => &self.tail_text[i + 1..],
            None => &self.tail_text,
        }
    }

    fn should_use_temp_file(&self) -> bool {
        self.total_raw_bytes > self.max_bytes as u64
            || self.total_decoded_bytes > self.max_bytes as u64
            || self.total_lines as u64 > self.max_lines
    }

    fn ensure_temp_file(&mut self) {
        if self.full_output_path.is_some() {
            return;
        }
        let path = crate::temp_dir().join(format!(
            "{}-{:016x}.log",
            self.temp_file_prefix,
            crate::random_u64()
        ));
        // Pi keeps the path when the stream fails to open; only the contents are lost.
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            for chunk in &self.raw_chunks {
                let _ = f.write_all(chunk);
            }
            self.temp_file = Some(f);
        }
        self.raw_chunks.clear();
        self.full_output_path = Some(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_keeps_split_characters_and_replaces_bad_bytes() {
        let mut d = Utf8Stream::default();
        let e = "é".as_bytes();
        assert_eq!(d.decode(&e[..1], false), "");
        assert_eq!(d.decode(&e[1..], false), "é");
        assert_eq!(d.decode(&[0xFF, b'a'], false), "\u{FFFD}a");
        assert_eq!(d.decode(&e[..1], true), "\u{FFFD}");
    }

    #[test]
    fn counts_lines_and_spills_when_over_the_limit() {
        let mut acc = OutputAccumulator::with_limits("pi-test", 3, 1000);
        acc.append(b"a\nb\nc\nd\n");
        acc.finish();
        let snap = acc.snapshot();
        assert!(snap.truncated);
        assert_eq!(snap.total_lines, 4);
        assert_eq!(snap.content, "b\nc\nd");
        let path = acc.full_output_path().unwrap().clone();
        drop(acc);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\nc\nd\n");
        let _ = std::fs::remove_file(path);
    }
}
