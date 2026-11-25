//! Spectre.Console's interactive prompts (`SelectionPrompt`, `TextPrompt`, `Confirm`) on the
//! terminal: arrow keys and Enter in raw mode. Without a terminal they read a line from stdin
//! (a number or the choice text).

use std::io::{BufRead, IsTerminal};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

use super::console::Console;
use super::markup;

/// Whether prompts can read keys (stdin is a terminal).
pub fn interactive() -> bool {
    std::io::stdin().is_terminal()
}

/// How many raw-mode guards are alive (raw mode is on while any is), and how many of them
/// belong to prompts reading keys (a background key watcher stands back while one does).
static RAW_DEPTH: std::sync::Mutex<usize> = std::sync::Mutex::new(0);
static PROMPTS_READING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Raw mode for the lifetime of the guard (restored on drop, even on panic). Guards nest.
pub struct RawGuard {
    on: bool,
    prompt: bool,
}

impl RawGuard {
    /// Raw mode for a prompt that reads keys.
    pub fn enable() -> Self {
        Self::new(true)
    }

    /// Raw mode for a background key watcher (which pauses while a prompt reads keys).
    pub fn enable_for_watcher() -> Self {
        Self::new(false)
    }

    fn new(prompt: bool) -> Self {
        let mut depth = RAW_DEPTH.lock().unwrap_or_else(|e| e.into_inner());
        let on = *depth > 0 || crossterm::terminal::enable_raw_mode().is_ok();
        if on {
            *depth += 1;
        }
        if prompt {
            PROMPTS_READING.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        Self { on, prompt }
    }
}

/// Whether a prompt is reading keys right now.
pub fn prompt_reading() -> bool {
    PROMPTS_READING.load(std::sync::atomic::Ordering::SeqCst) > 0
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        if self.prompt {
            PROMPTS_READING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        if self.on {
            let mut depth = RAW_DEPTH.lock().unwrap_or_else(|e| e.into_inner());
            *depth -= 1;
            if *depth == 0 {
                let _ = crossterm::terminal::disable_raw_mode();
            }
        }
    }
}

fn read_stdin_line() -> Option<String> {
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim_end_matches(['\r', '\n']).to_string()),
    }
}

/// `SelectionPrompt`: the index of the chosen item; None when cancelled (Ctrl+C / Esc / EOF).
/// Blocks the calling thread (call it through `block_in_place` from async code).
pub fn select(console: &Console, title: &str, choices: &[String]) -> Option<usize> {
    if choices.is_empty() {
        return None;
    }
    if !interactive() || !console.is_terminal() {
        console.markup_line(title);
        for (i, c) in choices.iter().enumerate() {
            console.markup_line(&format!("  {}. {c}", i + 1));
        }
        let answer = read_stdin_line()?;
        let answer = answer.trim();
        return answer
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .filter(|i| *i < choices.len())
            .or_else(|| {
                choices
                    .iter()
                    .position(|c| markup::remove(c).eq_ignore_ascii_case(answer))
            });
    }
    let _raw = RawGuard::enable();
    let mut current = 0usize;
    let draw = |current: usize, first: bool| {
        let mut s = String::new();
        if !first {
            // Back to the title line.
            s.push_str(&format!("\r\x1b[{}A", choices.len()));
        }
        s.push_str("\r\x1b[J");
        s.push_str(&markup::render(title, console.is_ansi()));
        for (i, c) in choices.iter().enumerate() {
            s.push_str("\r\n");
            if i == current {
                s.push_str(&markup::render(
                    &format!("[blue]> {c}[/]"),
                    console.is_ansi(),
                ));
            } else {
                s.push_str(&markup::render(&format!("  {c}"), console.is_ansi()));
            }
        }
        console.raw(&s);
    };
    console.raw("\x1b[?25l");
    draw(current, true);
    let result = loop {
        let Ok(ev) = crossterm::event::read() else {
            break None;
        };
        let Event::Key(key) = ev else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                current = if current == 0 {
                    choices.len() - 1
                } else {
                    current - 1
                };
                draw(current, false);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                current = (current + 1) % choices.len();
                draw(current, false);
            }
            KeyCode::Enter | KeyCode::Char(' ') => break Some(current),
            KeyCode::Esc => break None,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break None,
            _ => {}
        }
    };
    // Spectre clears the prompt once answered.
    console.raw(&format!("\r\x1b[{}A\r\x1b[J\x1b[?25h", choices.len()));
    result
}

/// `TextPrompt<string>`: one line of text (None when cancelled or at EOF).
pub fn text(console: &Console, prompt: &str) -> Option<String> {
    console.markup(&format!("{prompt} "));
    if !interactive() || !console.is_terminal() {
        return read_stdin_line();
    }
    let _raw = RawGuard::enable();
    let mut buf = String::new();
    loop {
        let Ok(Event::Key(key)) = crossterm::event::read() else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Enter => {
                console.raw("\r\n");
                return Some(buf);
            }
            KeyCode::Esc => {
                console.raw("\r\n");
                return None;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                console.raw("\r\n");
                return None;
            }
            KeyCode::Backspace => {
                if buf.pop().is_some() {
                    console.raw("\x08 \x08");
                }
            }
            KeyCode::Char(c) => {
                buf.push(c);
                console.raw(&c.to_string());
            }
            _ => {}
        }
    }
}

/// `Confirm`: yes / no with a default (Enter).
pub fn confirm(console: &Console, prompt: &str, default: bool) -> bool {
    let hint = if default { "[y/n] (y)" } else { "[y/n] (n)" };
    let answer = text(
        console,
        &format!("{prompt} [blue]{}[/]", markup::escape(hint)),
    );
    match answer.map(|a| a.trim().to_ascii_lowercase()) {
        Some(a) if a == "y" || a == "yes" => true,
        Some(a) if a == "n" || a == "no" => false,
        Some(a) if a.is_empty() => default,
        _ => false,
    }
}
