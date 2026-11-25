//! The console the interactive CLI writes to (`IAnsiConsole`): the terminal, or a buffer in
//! tests. All writes go through one lock, so the bottom panel's repaints (cursor save/restore)
//! never interleave with conversation output.

use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};

use super::markup;

enum Sink {
    Stdout,
    Buffer(Arc<Mutex<Vec<u8>>>),
}

struct Inner {
    sink: Sink,
}

/// A cheap handle (clones share the output and its lock).
#[derive(Clone)]
pub struct Console {
    inner: Arc<Mutex<Inner>>,
    ansi: bool,
    terminal: bool,
    fixed_width: Option<usize>,
}

impl Console {
    /// The process's stdout. ANSI styling when it is a terminal and `NO_COLOR` is not set.
    pub fn stdout() -> Self {
        let terminal = std::io::stdout().is_terminal();
        if terminal {
            // Windows consoles need VT processing switched on for escape sequences.
            let _ = crossterm::ansi_support::supports_ansi();
        }
        let ansi = terminal && std::env::var_os("NO_COLOR").is_none();
        Self {
            inner: Arc::new(Mutex::new(Inner { sink: Sink::Stdout })),
            ansi,
            terminal,
            fixed_width: None,
        }
    }

    /// A console that records plain text (tests).
    pub fn buffer() -> (Self, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                inner: Arc::new(Mutex::new(Inner {
                    sink: Sink::Buffer(buf.clone()),
                })),
                ansi: false,
                terminal: false,
                fixed_width: Some(100),
            },
            buf,
        )
    }

    /// The text written to a [`Console::buffer`] console so far.
    pub fn buffer_text(buf: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8_lossy(&buf.lock().unwrap()).to_string()
    }

    pub fn is_ansi(&self) -> bool {
        self.ansi
    }

    /// Whether this is a real terminal (cursor movement, raw-mode prompts).
    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Columns (80 when unknown).
    pub fn width(&self) -> usize {
        if let Some(w) = self.fixed_width {
            return w;
        }
        crossterm::terminal::size()
            .map(|(w, _)| w as usize)
            .ok()
            .filter(|w| *w > 0)
            .unwrap_or(80)
    }

    /// Rows (24 when unknown).
    pub fn height(&self) -> usize {
        crossterm::terminal::size()
            .map(|(_, h)| h as usize)
            .ok()
            .filter(|h| *h > 0)
            .unwrap_or(24)
    }

    /// Write text as it is (already ANSI, or plain).
    pub fn raw(&self, text: &str) {
        let inner = self.inner.lock().unwrap();
        match &inner.sink {
            Sink::Stdout => {
                let mut out = std::io::stdout().lock();
                if self.terminal {
                    let _ = out.write_all(crlf(text).as_bytes());
                } else {
                    let _ = out.write_all(text.as_bytes());
                }
                let _ = out.flush();
            }
            Sink::Buffer(buf) => {
                let plain = if self.ansi {
                    text.to_string()
                } else {
                    strip_ansi(text)
                };
                buf.lock().unwrap().extend_from_slice(plain.as_bytes());
            }
        }
    }

    /// `MarkupLine`.
    pub fn markup_line(&self, markup_text: &str) {
        self.raw(&(markup::render(markup_text, self.ansi) + "\n"));
    }

    /// `Markup` (no line break).
    pub fn markup(&self, markup_text: &str) {
        self.raw(&markup::render(markup_text, self.ansi));
    }

    /// `WriteLine` of plain text.
    pub fn write_line(&self, text: &str) {
        self.raw(&format!("{text}\n"));
    }

    /// An empty line.
    pub fn blank(&self) {
        self.raw("\n");
    }

    /// `Console.Clear`: clear the screen and home the cursor (terminals only).
    pub fn clear(&self) {
        if self.terminal {
            self.raw("\x1b[2J\x1b[3J\x1b[H");
        }
    }
}

/// Lone `\n` as `\r\n`: raw mode (Unix) does not translate line feeds.
fn crlf(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut prev = '\0';
    for c in text.chars() {
        if c == '\n' && prev != '\r' {
            out.push('\r');
        }
        out.push(c);
        prev = c;
    }
    out
}

/// Text without escape sequences (CSI, OSC) and other control characters except `\n` / `\t`.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for t in chars.by_ref() {
                        if ('@'..='~').contains(&t) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(t) = chars.next() {
                        if t == '\x07' {
                            break;
                        }
                        if t == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            }
            continue;
        }
        if c.is_control() && c != '\n' && c != '\t' {
            continue;
        }
        out.push(c);
    }
    out
}

/// Display width in terminal cells (`TerminalText.CellWidth`).
pub fn cell_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// Cut to `max` cells with a trailing `…` (`TerminalText.Ellipsize`).
pub fn ellipsize(text: &str, max: usize) -> String {
    if cell_width(text) <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in text.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_sequences() {
        assert_eq!(
            strip_ansi("\x1b[1;32mok\x1b[0m \x1b]8;;file:///x\x1b\\x\x1b]8;;\x1b\\"),
            "ok x"
        );
    }

    #[test]
    fn crlf_translates_lone_line_feeds() {
        assert_eq!(crlf("a\nb\r\nc"), "a\r\nb\r\nc");
    }

    #[test]
    fn ellipsize_cuts_to_width() {
        assert_eq!(ellipsize("abcdef", 4), "abc…");
        assert_eq!(ellipsize("abc", 4), "abc");
    }

    #[test]
    fn buffer_console_records_plain_text() {
        let (c, buf) = Console::buffer();
        c.markup_line("[green]hi[/] [[x]]");
        assert_eq!(Console::buffer_text(&buf), "hi [x]\n");
    }
}
