//! The prompt's line editor (RadLine in C#): one line with horizontal scrolling, history on
//! Up/Down, Tab completion of `/commands` and `@paths` (repeated Tab cycles), ghost text for a
//! suggested next prompt, Shift+Tab (mode), Ctrl+O (output level), Ctrl+C, Ctrl+D, Alt+V
//! (paste from the clipboard) and bracketed paste. Without a terminal it reads stdin lines.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::ui::console::{Console, cell_width};
use crate::ui::markup;
use crate::ui::prompt::RawGuard;

use super::state::InputMode;

/// How a read ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome {
    Line(String),
    /// Ctrl+C at the prompt (the buffer is dropped).
    Interrupted,
    /// Ctrl+D on an empty line, or end of input.
    Eof,
    /// Shift+Tab: the caller cycles the mode and reads again with the kept text.
    ModeCycled(String),
    /// Ctrl+O: the caller cycles the output level and reads again with the kept text.
    VerbosityCycled(String),
    /// Alt+V: the caller reads the clipboard and reads again with the text plus what it inserts.
    PasteRequested(String),
}

/// Tab completion sources (`ReplCompletion`).
pub struct Completion {
    pub command_names: Vec<String>,
    pub cwd: PathBuf,
}

impl Completion {
    /// Candidates for `word` (`context` = the text before it).
    pub fn candidates(&self, context: &str, word: &str) -> Vec<String> {
        if word.starts_with('/') && context.trim().is_empty() {
            let prefix = word[1..].to_lowercase();
            let mut m: Vec<&String> = self
                .command_names
                .iter()
                .filter(|n| n.to_lowercase().starts_with(&prefix))
                .collect();
            m.sort_by(|a, b| {
                a.len()
                    .cmp(&b.len())
                    .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
            });
            return m.into_iter().map(|n| format!("/{n}")).collect();
        }
        if let Some(partial) = word.strip_prefix('@') {
            return complete_path(partial, &self.cwd);
        }
        Vec::new()
    }
}

/// `ReplCompletion.CompletePath`: entries of the folder named so far whose names start with
/// the typed prefix (hidden ones only when the prefix starts with '.'); folders end in '/'.
pub fn complete_path(partial: &str, cwd: &Path) -> Vec<String> {
    let partial = partial.replace('\\', "/");
    let (dir_part, name_prefix) = match partial.rfind('/') {
        Some(i) => (&partial[..=i], &partial[i + 1..]),
        None => ("", partial.as_str()),
    };
    let dir = orca_abstractions::full_path(cwd, if dir_part.is_empty() { "." } else { dir_part });
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let show_hidden = name_prefix.starts_with('.');
    let lower = name_prefix.to_lowercase();
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.to_lowercase().starts_with(&lower) || (!show_hidden && name.starts_with('.')) {
            continue;
        }
        let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
        out.push(format!(
            "@{dir_part}{name}{}",
            if is_dir { "/" } else { "" }
        ));
        if out.len() >= 200 {
            break;
        }
    }
    out.sort_by_key(|a| a.to_lowercase());
    out
}

/// The prompt markup for a mode (`ModePrompt`).
pub fn prompt_markup(mode: InputMode, continuation: bool) -> &'static str {
    if continuation {
        return "[blue]  [/] ";
    }
    match mode {
        InputMode::Plan => "[cyan][[plan]][/] [blue bold]❯[/] ",
        InputMode::Ask => "[magenta][[ask]][/] [blue bold]❯[/] ",
        InputMode::Normal => "[blue bold]❯[/] ",
    }
}

pub struct LineEditor {
    pub history: Vec<String>,
    console: Console,
}

struct Buffer {
    chars: Vec<char>,
    cursor: usize,
}

impl Buffer {
    fn text(&self) -> String {
        self.chars.iter().collect()
    }
    fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\r' {
                continue;
            }
            // A pasted line break becomes a space: the prompt is one line.
            let c = if c == '\n' { ' ' } else { c };
            self.chars.insert(self.cursor, c);
            self.cursor += 1;
        }
    }
}

impl LineEditor {
    pub fn new(console: Console) -> Self {
        Self {
            history: Vec::new(),
            console,
        }
    }

    /// Read one line at the terminal (raw mode). `initial`: text to start with (kept across a
    /// mode cycle); `ghost`: a suggestion shown dimmed while the line is empty (Tab accepts).
    pub fn read_line(
        &mut self,
        prompt: &str,
        initial: &str,
        ghost: Option<&str>,
        completion: &Completion,
    ) -> ReadOutcome {
        let _raw = RawGuard::enable();
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);
        let mut buf = Buffer {
            chars: initial.chars().collect(),
            cursor: initial.chars().count(),
        };
        let mut hist_pos = self.history.len();
        let mut stash = String::new();
        // Tab cycling: (word start, candidates, index).
        let mut cycle: Option<(usize, Vec<String>, usize)> = None;
        self.render(prompt, &buf, ghost);
        let outcome = loop {
            let ev = match crossterm::event::read() {
                Ok(ev) => ev,
                Err(_) => break ReadOutcome::Eof,
            };
            let key = match ev {
                Event::Key(k) if k.kind != KeyEventKind::Release => k,
                Event::Paste(text) => {
                    cycle = None;
                    buf.insert_str(&text);
                    self.render(prompt, &buf, ghost);
                    continue;
                }
                Event::Resize(..) => {
                    self.render(prompt, &buf, ghost);
                    continue;
                }
                _ => continue,
            };
            if key.code != KeyCode::Tab {
                cycle = None;
            }
            match self.handle_key(
                key,
                &mut buf,
                &mut hist_pos,
                &mut stash,
                &mut cycle,
                ghost,
                completion,
            ) {
                Some(outcome) => break outcome,
                None => self.render(prompt, &buf, ghost),
            }
        };
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
        // Leave the typed line as it is and move below it.
        self.console.raw("\r\n");
        outcome
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_key(
        &mut self,
        key: KeyEvent,
        buf: &mut Buffer,
        hist_pos: &mut usize,
        stash: &mut String,
        cycle: &mut Option<(usize, Vec<String>, usize)>,
        ghost: Option<&str>,
        completion: &Completion,
    ) -> Option<ReadOutcome> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter => return Some(ReadOutcome::Line(buf.text())),
            KeyCode::Char('c') if ctrl => return Some(ReadOutcome::Interrupted),
            KeyCode::Char('d') if ctrl => {
                if buf.chars.is_empty() {
                    return Some(ReadOutcome::Eof);
                }
                if buf.cursor < buf.chars.len() {
                    buf.chars.remove(buf.cursor);
                }
            }
            KeyCode::Char('o') if ctrl => return Some(ReadOutcome::VerbosityCycled(buf.text())),
            KeyCode::Char('v') | KeyCode::Char('V') if alt => {
                return Some(ReadOutcome::PasteRequested(buf.text()));
            }
            KeyCode::BackTab => return Some(ReadOutcome::ModeCycled(buf.text())),
            KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                return Some(ReadOutcome::ModeCycled(buf.text()));
            }
            KeyCode::Char('a') if ctrl => buf.cursor = 0,
            KeyCode::Char('e') if ctrl => buf.cursor = buf.chars.len(),
            KeyCode::Char('u') if ctrl => {
                buf.chars.drain(..buf.cursor);
                buf.cursor = 0;
            }
            KeyCode::Char('k') if ctrl => buf.chars.truncate(buf.cursor),
            KeyCode::Char('w') if ctrl => {
                let mut start = buf.cursor;
                while start > 0 && buf.chars[start - 1] == ' ' {
                    start -= 1;
                }
                while start > 0 && buf.chars[start - 1] != ' ' {
                    start -= 1;
                }
                buf.chars.drain(start..buf.cursor);
                buf.cursor = start;
            }
            KeyCode::Char(c) if !ctrl => {
                buf.chars.insert(buf.cursor, c);
                buf.cursor += 1;
            }
            KeyCode::Backspace => {
                if buf.cursor > 0 {
                    buf.cursor -= 1;
                    buf.chars.remove(buf.cursor);
                }
            }
            KeyCode::Delete => {
                if buf.cursor < buf.chars.len() {
                    buf.chars.remove(buf.cursor);
                }
            }
            KeyCode::Left => buf.cursor = buf.cursor.saturating_sub(1),
            KeyCode::Right => {
                if buf.cursor < buf.chars.len() {
                    buf.cursor += 1;
                } else if buf.chars.is_empty()
                    && let Some(g) = ghost
                {
                    buf.insert_str(g);
                }
            }
            KeyCode::Home => buf.cursor = 0,
            KeyCode::End => buf.cursor = buf.chars.len(),
            KeyCode::Up => {
                if *hist_pos > 0 {
                    if *hist_pos == self.history.len() {
                        *stash = buf.text();
                    }
                    *hist_pos -= 1;
                    buf.chars = self.history[*hist_pos].chars().collect();
                    buf.cursor = buf.chars.len();
                }
            }
            KeyCode::Down => {
                if *hist_pos < self.history.len() {
                    *hist_pos += 1;
                    let text = if *hist_pos == self.history.len() {
                        stash.clone()
                    } else {
                        self.history[*hist_pos].clone()
                    };
                    buf.chars = text.chars().collect();
                    buf.cursor = buf.chars.len();
                }
            }
            KeyCode::Tab => {
                if buf.chars.is_empty()
                    && let Some(g) = ghost
                {
                    buf.insert_str(g);
                    return None;
                }
                if let Some((start, cands, idx)) = cycle.as_mut() {
                    *idx = (*idx + 1) % cands.len();
                    buf.chars.drain(*start..buf.cursor);
                    buf.cursor = *start;
                    let c = cands[*idx].clone();
                    buf.insert_str(&c);
                    return None;
                }
                let before: String = buf.chars[..buf.cursor].iter().collect();
                let start = before.rfind(' ').map_or(0, |i| i + 1);
                let word = &before[start..];
                let context = &before[..start];
                let cands = completion.candidates(context, word);
                if let Some(first) = cands.first().cloned() {
                    let start_chars = before[..start].chars().count();
                    buf.chars.drain(start_chars..buf.cursor);
                    buf.cursor = start_chars;
                    buf.insert_str(&first);
                    if cands.len() > 1 {
                        *cycle = Some((start_chars, cands, 0));
                    }
                }
            }
            KeyCode::Esc => {}
            _ => {}
        }
        None
    }

    /// Redraw the prompt line: prompt, the visible part of the text, the cursor.
    fn render(&self, prompt: &str, buf: &Buffer, ghost: Option<&str>) {
        let width = self.console.width().max(20);
        let prompt_ansi = markup::render(prompt, self.console.is_ansi());
        let prompt_w = cell_width(&markup::remove(prompt));
        let avail = width.saturating_sub(prompt_w + 1).max(5);
        // Scroll so the cursor stays visible.
        let widths: Vec<usize> = buf
            .chars
            .iter()
            .map(|c| unicode_width::UnicodeWidthChar::width(*c).unwrap_or(0))
            .collect();
        let mut start = 0;
        while widths[start..buf.cursor].iter().sum::<usize>() >= avail {
            start += 1;
        }
        let mut shown = String::new();
        let mut w = 0;
        for (i, c) in buf.chars.iter().enumerate().skip(start) {
            if w + widths[i] > avail {
                break;
            }
            shown.push(*c);
            w += widths[i];
        }
        let cursor_col = prompt_w + widths[start..buf.cursor].iter().sum::<usize>();
        let mut s = format!("\r\x1b[2K{prompt_ansi}{shown}");
        if buf.chars.is_empty()
            && let Some(g) = ghost
        {
            let g = crate::ui::console::ellipsize(g, avail);
            s.push_str(&format!("\x1b[2m{g}\x1b[0m"));
        }
        s.push_str(&format!("\r\x1b[{}C", cursor_col).replace("\x1b[0C", ""));
        self.console.raw(&s);
    }

    /// Remember a submitted line (no repeats of the last entry).
    pub fn add_history(&mut self, line: &str) {
        if !line.is_empty() && self.history.last().map(String::as_str) != Some(line) {
            self.history.push(line.to_string());
        }
    }
}

/// `ReadNonInteractive`: a line from stdin; a trailing backslash continues on the next line.
pub fn read_non_interactive() -> Option<String> {
    let mut stdin = std::io::stdin().lock();
    let read = |stdin: &mut std::io::StdinLock| {
        let mut l = String::new();
        match stdin.read_line(&mut l) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(l.trim_end_matches(['\r', '\n']).to_string()),
        }
    };
    let mut line = read(&mut stdin)?;
    while line.ends_with('\\') {
        line.pop();
        line.push('\n');
        match read(&mut stdin) {
            Some(next) => line.push_str(&next),
            None => break,
        }
    }
    Some(line.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_completion_prefers_short_names() {
        let c = Completion {
            command_names: vec!["session".into(), "stats".into(), "s".into(), "help".into()],
            cwd: PathBuf::from("."),
        };
        assert_eq!(c.candidates("", "/s"), ["/s", "/stats", "/session"]);
        assert!(c.candidates("x ", "/s").is_empty());
    }

    #[test]
    fn path_completion_lists_matching_entries() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        std::fs::write(d.path().join("src").join("main.rs"), "").unwrap();
        std::fs::write(d.path().join("Readme.md"), "").unwrap();
        std::fs::write(d.path().join(".hidden"), "").unwrap();
        assert_eq!(complete_path("", d.path()), ["@Readme.md", "@src/"]);
        assert_eq!(complete_path("src/m", d.path()), ["@src/main.rs"]);
        assert_eq!(complete_path(".h", d.path()), ["@.hidden"]);
    }

    #[test]
    fn prompt_shows_mode() {
        assert_eq!(
            markup::remove(prompt_markup(InputMode::Plan, false)),
            "[plan] ❯ "
        );
        assert_eq!(
            markup::remove(prompt_markup(InputMode::Normal, false)),
            "❯ "
        );
    }
}
