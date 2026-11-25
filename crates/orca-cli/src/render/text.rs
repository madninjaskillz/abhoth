//! `TerminalText.Sanitize`: untrusted text (tool output, commands) made safe to paint inside
//! the TUI. Raw escape sequences from a tool's stdout would otherwise reach the terminal:
//! colours bleed into the surrounding style, and cursor moves / line clears / screen clears
//! (progress bars, `clear`, npm spinners) can wipe the pinned input panel. Cell widths and
//! `Ellipsize` live in `ui::console` (`cell_width`, `ellipsize`).

use std::sync::LazyLock;

use regex::Regex;

const LINE_SEPARATOR: char = '\u{2028}';
const PARAGRAPH_SEPARATOR: char = '\u{2029}';

// CSI (ESC [ ... final), OSC (ESC ] ... BEL | ESC \), other two-char ESC sequences, and
// 8-bit CSI (U+009B).
static ESCAPE_SEQUENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)?|\x1b[@-Z\\-_]|\x{9b}[0-?]*[ -/]*[@-~]",
    )
    .expect("static regex")
});

/// Strip ANSI/VT escape sequences and control characters; apply carriage-return overwrite
/// semantics per line (a progress bar's `10%\r20%\r100%` shows `100%`); expand tabs to four
/// spaces. Newlines are kept (CRLF normalised to LF).
pub fn sanitize(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut text = if text.contains(['\x1b', '\u{9b}']) {
        ESCAPE_SEQUENCE.replace_all(text, "").into_owned()
    } else {
        text.to_string()
    };
    text = text.replace("\r\n", "\n");

    let needs_work = text
        .chars()
        .any(|c| c != '\n' && (c.is_control() || c == LINE_SEPARATOR || c == PARAGRAPH_SEPARATOR));
    if !needs_work {
        return text;
    }

    let mut sb = String::with_capacity(text.len());
    for raw_line in text.split('\n') {
        // Carriage return: the terminal would overwrite from column 0 — keep the last
        // non-empty segment (a trailing \r alone leaves the line as it was).
        let line = if raw_line.contains('\r') {
            raw_line
                .split('\r')
                .rfind(|s| !s.is_empty())
                .unwrap_or("")
        } else {
            raw_line
        };
        for c in line.chars() {
            if c == '\t' {
                sb.push_str("    ");
            } else if c == '\x08' {
                if !sb.is_empty() && !sb.ends_with('\n') {
                    sb.pop();
                }
            } else if !c.is_control() && c != LINE_SEPARATOR && c != PARAGRAPH_SEPARATOR {
                sb.push(c);
            }
        }
        sb.push('\n');
    }
    sb.pop(); // the separator added after the last line
    sb
}

/// `string.IsNullOrWhiteSpace`.
pub(crate) fn is_blank(s: &str) -> bool {
    s.chars().all(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    //! Ported from TerminalTextTests (UiPolishTests.cs). `Ellipsize` / `CellWidth` are
    //! `ui::console::{ellipsize, cell_width}` and tested here too.
    use super::*;
    use crate::ui::console::{cell_width, ellipsize};

    #[test]
    fn sanitize_strips_colour_and_cursor_sequences() {
        let raw = "\x1b[32mPASS\x1b[0m test\n\x1b[2K\x1b[1A\x1b[?25lnext\x1b]0;title\x07!";
        assert_eq!(sanitize(raw), "PASS test\nnext!");
    }

    #[test]
    fn sanitize_carriage_return_keeps_last_overwrite() {
        assert_eq!(sanitize("10%\r50%\r100%\r\ndone"), "100%\ndone");
    }

    #[test]
    fn sanitize_expands_tabs_and_drops_control_chars() {
        assert_eq!(sanitize("a\tb\u{7} c"), "a    b c");
    }

    #[test]
    fn sanitize_plain_text_unchanged() {
        let text = "def add(a, b):\n    return a + b";
        assert_eq!(sanitize(text), text);
    }

    #[test]
    fn ellipsize_counts_wide_characters_as_two_cells() {
        assert_eq!(ellipsize("abc", 10), "abc");
        assert_eq!(ellipsize("abcdefgh", 5), "abcd…");
        assert_eq!(cell_width("日本"), 4);
        assert!(cell_width(&ellipsize("日本語のテキスト", 7)) <= 7);
    }
}
