//! Port of Pi's truncate.ts (`PiTruncate`): head truncation for file reads, tail truncation for
//! shell output, Pi's size formatting and line cap. Byte counts are UTF-8 bytes, as Pi's
//! `Buffer.byteLength`.

pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
pub const DEFAULT_MAX_LINES: u64 = 2000;
pub const GREP_MAX_LINE_LENGTH: usize = 500;

/// JavaScript `Number.MAX_SAFE_INTEGER`, Pi's "no line limit".
pub const NO_LINE_LIMIT: u64 = (1u64 << 53) - 1;

/// `PiTruncation` (Pi's TruncationResult).
#[derive(Debug, Clone, PartialEq)]
pub struct Truncation {
    pub content: String,
    pub truncated: bool,
    /// "lines" or "bytes".
    pub truncated_by: Option<&'static str>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub last_line_partial: bool,
    pub first_line_exceeds_limit: bool,
    pub max_lines: u64,
    pub max_bytes: usize,
}

/// Lines for counting: a trailing newline terminates the last line.
pub fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn untruncated(
    content: &str,
    lines: usize,
    bytes: usize,
    max_lines: u64,
    max_bytes: usize,
) -> Truncation {
    Truncation {
        content: content.to_string(),
        truncated: false,
        truncated_by: None,
        total_lines: lines,
        total_bytes: bytes,
        output_lines: lines,
        output_bytes: bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Keep the first lines/bytes. Never returns a partial line.
pub fn head(content: &str, max_bytes: usize, max_lines: u64) -> Truncation {
    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();
    if total_lines as u64 <= max_lines && total_bytes <= max_bytes {
        return untruncated(content, total_lines, total_bytes, max_lines, max_bytes);
    }
    if !lines.is_empty() && lines[0].len() > max_bytes {
        return Truncation {
            content: String::new(),
            truncated: true,
            truncated_by: Some("bytes"),
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }
    let mut collected: Vec<&str> = Vec::new();
    let mut output_bytes = 0usize;
    let mut truncated_by = "lines";
    for (i, line) in lines.iter().enumerate() {
        if collected.len() as u64 >= max_lines {
            break;
        }
        let line_bytes = line.len() + usize::from(i > 0);
        if output_bytes + line_bytes > max_bytes {
            truncated_by = "bytes";
            break;
        }
        collected.push(line);
        output_bytes += line_bytes;
    }
    if collected.len() as u64 >= max_lines && output_bytes <= max_bytes {
        truncated_by = "lines";
    }
    let output = collected.join("\n");
    Truncation {
        output_bytes: output.len(),
        content: output,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: collected.len(),
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// [`head`] with Pi's defaults (2000 lines / 50KB).
pub fn head_default(content: &str) -> Truncation {
    head(content, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES)
}

/// Keep the last lines/bytes. A single over-long last line keeps its end.
pub fn tail(content: &str, max_bytes: usize, max_lines: u64) -> Truncation {
    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();
    if total_lines as u64 <= max_lines && total_bytes <= max_bytes {
        return untruncated(content, total_lines, total_bytes, max_lines, max_bytes);
    }
    let mut collected: Vec<String> = Vec::new();
    let mut output_bytes = 0usize;
    let mut truncated_by = "lines";
    let mut last_line_partial = false;
    for line in lines.iter().rev() {
        if collected.len() as u64 >= max_lines {
            break;
        }
        let line_bytes = line.len() + usize::from(!collected.is_empty());
        if output_bytes + line_bytes > max_bytes {
            truncated_by = "bytes";
            if collected.is_empty() {
                let partial = tail_bytes_utf8(line, max_bytes);
                output_bytes = partial.len();
                collected.push(partial.to_string());
                last_line_partial = true;
            }
            break;
        }
        collected.push(line.to_string());
        output_bytes += line_bytes;
    }
    collected.reverse();
    if collected.len() as u64 >= max_lines && output_bytes <= max_bytes {
        truncated_by = "lines";
    }
    let output = collected.join("\n");
    Truncation {
        output_bytes: output.len(),
        content: output,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines: collected.len(),
        last_line_partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// [`tail`] with Pi's defaults.
pub fn tail_default(content: &str) -> Truncation {
    tail(content, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES)
}

/// The last `n` UTF-8 bytes of `s`, advanced to a character boundary.
pub fn tail_bytes_utf8(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut start = s.len() - n;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Pi's formatSize: "123B", "12.3KB", "1.2MB".
pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{}KB", format_one_decimal(bytes as f64 / 1024.0))
    } else {
        format!("{}MB", format_one_decimal(bytes as f64 / (1024.0 * 1024.0)))
    }
}

/// .NET's "0.0" format: one decimal, rounded half away from zero.
fn format_one_decimal(v: f64) -> String {
    let scaled = (v * 10.0).round();
    format!("{:.1}", scaled / 10.0)
}

/// Pi's truncateLine: a line longer than `max_chars` UTF-16 units is cut to `max_chars` units
/// plus "... [truncated]". A cut inside a surrogate pair leaves U+FFFD.
pub fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
    let units: Vec<u16> = line.encode_utf16().collect();
    if units.len() <= max_chars {
        return (line.to_string(), false);
    }
    let mut cut = units[..max_chars].to_vec();
    let mut text = if cut.last().is_some_and(|u| (0xD800..0xDC00).contains(u)) {
        cut.pop();
        let mut t = String::from_utf16_lossy(&cut);
        t.push('\u{FFFD}');
        t
    } else {
        String::from_utf16_lossy(&cut)
    };
    text.push_str("... [truncated]");
    (text, true)
}

/// JavaScript `String(number)` for the finite values tool arguments carry.
pub fn js_number(v: f64) -> String {
    if v == v.floor() && v.abs() < 1e21 {
        let s = format!("{v:.0}");
        if s == "-0" { "0".into() } else { s }
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_number_and_sizes() {
        assert_eq!(js_number(5.0), "5");
        assert_eq!(js_number(2.5), "2.5");
        assert_eq!(js_number(1e20), "100000000000000000000");
        assert_eq!(format_size(1536 * 1024), "1.5MB");
        assert_eq!(format_size(60 * 1024), "60.0KB");
    }

    #[test]
    fn truncate_line_counts_utf16_units() {
        assert_eq!(
            truncate_line("a😀b", 2),
            ("a\u{FFFD}... [truncated]".into(), true)
        );
    }
}
