//! Helpers shared by the tool crates (port of `OpenOrca.Tools.Common`).

#[cfg(feature = "http")]
pub mod http;
pub mod path_safety;
pub mod task_store;

pub use path_safety::is_dangerous_path;
pub use task_store::TaskStore;

use std::future::Future;
use std::time::Duration;

use serde_json::Value;

/// `StringEscapeHelper.UnescapeLiteralSequences`: fixes double-escaped sequences from local
/// models that emit `\\n` instead of `\n` in JSON, so after parsing the string holds a literal
/// backslash + `n`. `\\` is handled first so `\\n` is not half-matched.
pub fn unescape_literal_sequences(value: &str) -> String {
    if !value.contains('\\') {
        return value.to_string();
    }
    value
        .replace("\\\\", "\u{0}")
        .replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace('\u{0}', "\\")
}

/// `JsonElement.GetInt32Lenient`: a number, or a string holding one (LLMs send `"50"`).
pub fn get_i32_lenient(value: Option<&Value>, default: i32) -> i32 {
    match value {
        Some(Value::Number(n)) => n
            .as_i64()
            .and_then(|v| i32::try_from(v).ok())
            .unwrap_or(default),
        Some(Value::String(s)) => s.trim().parse::<i32>().unwrap_or(default),
        _ => default,
    }
}

/// `JsonElement.GetBooleanLenient`: a bool, or a string "true"/"false" (any case).
pub fn get_bool_lenient(value: Option<&Value>, default: bool) -> bool {
    match value {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" => true,
            "false" => false,
            _ => default,
        },
        _ => default,
    }
}

/// A string argument (`args.GetProperty(name).GetString()`); `None` when absent or not a string.
pub fn get_str<'a>(args: &'a Value, name: &str) -> Option<&'a str> {
    args.get(name).and_then(Value::as_str)
}

/// `1234567` -> `"1,234,567"` (.NET's `N0` format in the invariant/en-US culture).
pub fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `OutputTrimmer.KeepHeadAndTail`: shortens long output to roughly `max_chars` (plus a marker
/// line), keeping the first ~20% and the last ~80%, cut on line boundaries when one is near.
/// Errors and test summaries live at the END of output, so a head-only cut loses them.
///
/// Lengths count `char`s (C# counts UTF-16 units; the two agree for text without astral
/// characters).
pub fn keep_head_and_tail(content: &str, max_chars: usize, note: Option<&str>) -> String {
    let chars: Vec<char> = content.chars().collect();
    let len = chars.len();
    if len <= max_chars || max_chars == 0 {
        return content.to_string();
    }

    let head_budget = max_chars / 5;
    let tail_budget = max_chars - head_budget;

    // content.LastIndexOf('\n', max(0, headEnd - 1)): search backwards from that index.
    let mut head_end = head_budget;
    let search_from = head_end.saturating_sub(1).min(len - 1);
    if let Some(nl) = chars[..=search_from].iter().rposition(|&c| c == '\n')
        && nl > head_budget / 2
    {
        head_end = nl + 1;
    }

    let mut tail_start = len - tail_budget;
    if let Some(off) = chars[tail_start..].iter().position(|&c| c == '\n') {
        let nl2 = tail_start + off;
        if nl2 < tail_start + tail_budget / 4 {
            tail_start = nl2 + 1;
        }
    }

    let omitted = &chars[head_end..tail_start];
    let omitted_lines = omitted.iter().filter(|&&c| c == '\n').count();
    let what = if omitted_lines > 0 {
        format!(
            "{} lines ({} chars)",
            group_thousands(omitted_lines),
            group_thousands(omitted.len())
        )
    } else {
        format!("{} chars", group_thousands(omitted.len()))
    };
    let marker = match note {
        None => format!("[... {what} omitted ...]"),
        Some(note) => format!("[... {what} omitted; {note} ...]"),
    };

    let mut head: String = chars[..head_end].iter().collect();
    if !head.ends_with('\n') {
        head.push('\n');
    }
    let tail: String = chars[tail_start..].iter().collect();
    format!("{head}{marker}\n{tail}")
}

/// `FileRetryHelper.RetryOnIOExceptionAsync`: retries an I/O operation that fails (e.g. a
/// file locked by another process) with 50 / 100 / 200 ms backoff.
pub async fn retry_io<T, F, Fut>(mut op: F) -> std::io::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::io::Result<T>>,
{
    const MAX_RETRIES: u32 = 3;
    let mut attempt = 0;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            // NotFound and the like won't fix themselves; only retry what a lock can cause.
            Err(e) if attempt < MAX_RETRIES && is_transient(&e) => {
                tokio::time::sleep(Duration::from_millis(50 * (1 << attempt))).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

fn is_transient(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    !matches!(
        e.kind(),
        NotFound | InvalidInput | InvalidData | AlreadyExists | Unsupported
    )
}

/// The C# tools' `ReadAllText`: UTF-8 with a BOM stripped if present.
pub fn decode_text(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

/// `File.ReadAllLines`: splits on `\r\n`, `\n` or `\r`; no trailing empty line for a final newline.
pub fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                lines.push(&text[start..i]);
                start = i + 1;
            }
            b'\r' => {
                lines.push(&text[start..i]);
                if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    i += 1;
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < bytes.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// `Environment.NewLine`, which the C# tools' `AppendLine` writes.
pub const NEWLINE: &str = if cfg!(windows) { "\r\n" } else { "\n" };

/// A C# `string.Length`: UTF-16 code units (what the tools report as "chars").
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// A required string argument, or the error the tool returns without it.
pub fn required_str<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Missing required argument: {name}"))
}

/// An I/O error as .NET's exception message shows it: the OS text without Rust's
/// ` (os error N)` suffix (on Windows both come from `FormatMessage`).
pub fn io_message(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.rfind(" (os error ") {
        Some(i) if s.ends_with(')') => s[..i].to_string(),
        _ => s,
    }
}

/// Appends `line` and a [`NEWLINE`] (`StringBuilder.AppendLine`).
pub fn append_line(sb: &mut String, line: &str) {
    sb.push_str(line);
    sb.push_str(NEWLINE);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── StringEscapeHelperTests ──

    #[test]
    fn unescape_converts_newlines() {
        assert_eq!(
            unescape_literal_sequences("line1\\nline2\\nline3"),
            "line1\nline2\nline3"
        );
    }

    #[test]
    fn unescape_converts_tabs() {
        assert_eq!(unescape_literal_sequences("col1\\tcol2"), "col1\tcol2");
    }

    #[test]
    fn unescape_converts_backslashes() {
        assert_eq!(
            unescape_literal_sequences("path\\\\to\\\\file"),
            "path\\to\\file"
        );
    }

    #[test]
    fn unescape_no_backslash_returns_original() {
        assert_eq!(
            unescape_literal_sequences("no special chars here"),
            "no special chars here"
        );
    }

    #[test]
    fn unescape_mixed_sequences() {
        assert_eq!(
            unescape_literal_sequences("first\\nsecond\\tthird"),
            "first\nsecond\tthird"
        );
    }

    // ── OutputTrimmerTests ──

    #[test]
    fn keep_head_and_tail_under_limit_unchanged() {
        assert_eq!(keep_head_and_tail("short", 100, None), "short");
    }

    #[test]
    fn keep_head_and_tail_over_limit_keeps_twenty_eighty_split_with_marker() {
        let content: Vec<String> = (1..=1000).map(|i| format!("line {i:04}")).collect();
        let content = content.join("\n");
        let result = keep_head_and_tail(&content, 1000, Some("full output: x.txt"));

        assert!(result.starts_with("line 0001\n"));
        assert!(result.ends_with("line 1000"));
        assert!(result.contains("omitted; full output: x.txt ...]"));
        let marker = result.find("[...").unwrap();
        assert!((100..=220).contains(&marker), "marker at {marker}");
        assert!((800..=1000).contains(&(result.len() - marker)));
    }

    #[test]
    fn keep_head_and_tail_no_newlines_still_bounded() {
        let content = "z".repeat(10_000);
        let result = keep_head_and_tail(&content, 500, None);
        assert!(result.len() < 600);
        assert!(result.contains("9,500 chars omitted"));
    }

    // ── JsonElementExtensions ──

    #[test]
    fn lenient_numbers_and_bools() {
        assert_eq!(get_i32_lenient(Some(&json!(5)), 1), 5);
        assert_eq!(get_i32_lenient(Some(&json!("50")), 1), 50);
        assert_eq!(get_i32_lenient(Some(&json!("x")), 7), 7);
        assert_eq!(get_i32_lenient(None, 7), 7);
        assert!(get_bool_lenient(Some(&json!(true)), false));
        assert!(get_bool_lenient(Some(&json!("True")), false));
        assert!(!get_bool_lenient(Some(&json!("nope")), false));
    }

    #[test]
    fn thousands_grouping() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(9500), "9,500");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn split_lines_like_read_all_lines() {
        assert_eq!(split_lines("a\r\nb\nc\rd\n"), vec!["a", "b", "c", "d"]);
        assert_eq!(split_lines("a\n\nb"), vec!["a", "", "b"]);
        assert!(split_lines("").is_empty());
        assert_eq!(decode_text(b"\xEF\xBB\xBFhi"), "hi");
    }
}
