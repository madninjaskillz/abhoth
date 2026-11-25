//! Interactive scenarios: the binary runs without `--print` inside a pseudo-terminal (ConPTY on
//! Windows), driven by scripted steps (typed text, keys, waits for screen text), with a `vt100`
//! screen model standing in for the terminal. The step vocabulary follows the C# TUI harness
//! (`tests/OpenOrca.TuiHarness`).

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde::Deserialize;

/// `"interactive": {...}` in a scenario.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Interactive {
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
    pub steps: Vec<Step>,
    /// Screen lines compared between the two binaries: a line is kept when it contains one of
    /// these texts or matches it as a regex (default: none; the rendering differs by design).
    #[serde(default)]
    pub compare_screen: Vec<String>,
    /// How long to wait for the process to exit after the last step before killing it.
    #[serde(default = "default_exit_timeout")]
    pub exit_timeout_ms: u64,
}

fn default_cols() -> u16 {
    100
}

fn default_rows() -> u16 {
    32
}

fn default_exit_timeout() -> u64 {
    10_000
}

fn default_timeout() -> u64 {
    30_000
}

fn default_count() -> usize {
    1
}

/// One step. `action`: waitFor | waitGone (visible screen, `text`, `timeoutMs`), type (`text`),
/// key (`name`, `count`), wait (`ms`), waitQuiet (`ms` of output silence, `timeoutMs`), snap
/// (`label`), expect | expectNot (`text` on the screen or in the scrollback), exit (wait up to
/// `timeoutMs` for the process to exit, e.g. after `/exit`; `expect` steps can follow).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Step {
    pub action: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub ms: u64,
    #[serde(default = "default_count")]
    pub count: usize,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub label: Option<String>,
}

/// What one interactive run produced.
pub struct PtyRun {
    pub exit_code: Option<i32>,
    /// One entry per failed step (`step N: ...`), plus a note when the process had to be killed.
    pub failures: Vec<String>,
    /// Scrollback then visible screen, each line trimmed, blank lines dropped.
    pub screen: Vec<String>,
    /// `label` -> the visible screen at that `snap` step.
    pub snaps: Vec<(String, String)>,
    /// The raw VT stream the terminal received.
    pub raw: Vec<u8>,
}

/// The terminal's answers to queries in the output (DSR cursor position).
#[derive(Default)]
struct Answers {
    pending: Vec<u8>,
}

impl vt100::Callbacks for Answers {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        // DSR 6 (`ESC[6n`): report the cursor position, 1-based. ConPTY asks at startup
        // (PSEUDOCONSOLE_INHERIT_CURSOR) and waits for the answer.
        if c == 'n' && i1.is_none() && params.first().and_then(|p| p.first()) == Some(&6) {
            let (row, col) = screen.cursor_position();
            self.pending
                .extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
        }
    }
}

struct Term {
    parser: vt100::Parser<Answers>,
    last_output: Instant,
    raw: Vec<u8>,
}

impl Term {
    fn visible(&self) -> Vec<String> {
        let (_, cols) = self.parser.screen().size();
        self.parser.screen().rows(0, cols.get()).collect()
    }

    /// The lines that scrolled off the top, oldest first.
    fn scrollback(&mut self) -> Vec<String> {
        let screen = self.parser.screen_mut();
        let (_, cols) = screen.size();
        screen.set_scrollback(usize::MAX);
        let len = screen.scrollback();
        let mut out = Vec::with_capacity(len);
        for off in (1..=len).rev() {
            screen.set_scrollback(off);
            out.push(screen.rows(0, cols.get()).next().unwrap_or_default());
        }
        screen.set_scrollback(0);
        out
    }

    fn transcript(&mut self) -> String {
        let mut lines = self.scrollback();
        lines.extend(self.visible());
        lines.join("\n")
    }
}

/// The bytes a terminal sends for a key name (as the C# TUI harness encodes them).
pub fn encode_key(name: &str) -> Option<&'static str> {
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" => "\r",
        "esc" | "escape" => "\x1b",
        "tab" => "\t",
        "shift-tab" => "\x1b[Z",
        "backspace" => "\x7f",
        "up" => "\x1b[A",
        "down" => "\x1b[B",
        "right" => "\x1b[C",
        "left" => "\x1b[D",
        "home" => "\x1b[H",
        "end" => "\x1b[F",
        "pgup" => "\x1b[5~",
        "pgdn" => "\x1b[6~",
        "delete" => "\x1b[3~",
        "ctrl-c" => "\x03",
        "ctrl-d" => "\x04",
        "ctrl-g" => "\x07",
        "ctrl-o" => "\x0f",
        "ctrl-r" => "\x12",
        "ctrl-t" => "\x14",
        "alt-v" => "\x1bv",
        _ => return None,
    })
}

fn poll(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Run `exe args` in a pseudo-terminal in `cwd` with `env` added (and `RUST_LOG` removed),
/// play the steps and wait for the process to exit (killing it after `exit_timeout_ms`).
pub fn run(
    exe: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(String, String)],
    inter: &Interactive,
) -> Result<PtyRun> {
    // Scenario mistakes are errors before anything starts (no stray process).
    for (i, step) in inter.steps.iter().enumerate() {
        let known = [
            "waitFor",
            "waitGone",
            "waitQuiet",
            "wait",
            "type",
            "key",
            "snap",
            "expect",
            "expectNot",
            "exit",
        ];
        if !known.contains(&step.action.as_str()) {
            return Err(anyhow!("step {}: unknown action {:?}", i + 1, step.action));
        }
        if step.action == "key" && encode_key(step.name.as_deref().unwrap_or_default()).is_none() {
            return Err(anyhow!("step {}: unknown key name {:?}", i + 1, step.name));
        }
    }
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: inter.rows,
            cols: inter.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| anyhow!("openpty: {e}"))?;
    let mut cmd = CommandBuilder::new(exe);
    cmd.args(args);
    cmd.cwd(cwd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.env_remove("RUST_LOG");
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| anyhow!("spawning {} in a pty: {e}", exe.display()))?;
    drop(pair.slave);
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| anyhow!("pty reader: {e}"))?;
    let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(
        pair.master
            .take_writer()
            .map_err(|e| anyhow!("pty writer: {e}"))?,
    ));

    let rows = std::num::NonZeroU16::new(inter.rows).context("rows must be > 0")?;
    let cols = std::num::NonZeroU16::new(inter.cols).context("cols must be > 0")?;
    let term = Arc::new(Mutex::new(Term {
        parser: vt100::Parser::new_with_callbacks(rows, cols, 20_000, Answers::default()),
        last_output: Instant::now(),
        raw: Vec::new(),
    }));
    let reader_thread = {
        let (term, writer) = (term.clone(), writer.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let answer = {
                    let mut t = term.lock().unwrap();
                    t.raw.extend_from_slice(&buf[..n]);
                    t.last_output = Instant::now();
                    t.parser.process(&buf[..n]);
                    std::mem::take(&mut t.parser.callbacks_mut().pending)
                };
                if !answer.is_empty() {
                    let mut w = writer.lock().unwrap();
                    let _ = w.write_all(&answer);
                    let _ = w.flush();
                }
            }
        })
    };

    let send = |bytes: &str| -> Result<()> {
        let mut w = writer.lock().unwrap();
        w.write_all(bytes.as_bytes())?;
        w.flush()?;
        Ok(())
    };
    let screen_has = |text: &str| {
        let shown = term.lock().unwrap().visible();
        // Text can wrap across rows: also look in the joined screen.
        shown.iter().any(|l| l.contains(text)) || shown.concat().contains(text)
    };

    let mut failures = Vec::new();
    let mut snaps = Vec::new();
    for (i, step) in inter.steps.iter().enumerate() {
        let n = i + 1;
        let text = step.text.clone().unwrap_or_default();
        let timeout = Duration::from_millis(step.timeout_ms);
        match step.action.as_str() {
            "waitFor" => {
                if !poll(timeout, || screen_has(&text)) {
                    failures.push(format!("step {n}: timed out waiting for {text:?}"));
                }
            }
            "waitGone" => {
                if !poll(timeout, || !screen_has(&text)) {
                    failures.push(format!("step {n}: timed out waiting for {text:?} to go"));
                }
            }
            "waitQuiet" => {
                let quiet = Duration::from_millis(if step.ms > 0 { step.ms } else { 800 });
                if !poll(timeout, || {
                    term.lock().unwrap().last_output.elapsed() >= quiet
                }) {
                    failures.push(format!(
                        "step {n}: output did not go quiet for {}ms",
                        quiet.as_millis()
                    ));
                }
            }
            "wait" => std::thread::sleep(Duration::from_millis(step.ms)),
            "type" | "key" => {
                let bytes = match step.action.as_str() {
                    "type" => text.clone(),
                    _ => encode_key(step.name.as_deref().unwrap_or_default())
                        .unwrap_or_default()
                        .repeat(step.count.max(1)),
                };
                // The process may have gone (a crash): a step result, not a harness error.
                if let Err(e) = send(&bytes) {
                    failures.push(format!("step {n}: could not send input: {e}"));
                }
            }
            "snap" => {
                let shown = term.lock().unwrap().visible().join("\n");
                snaps.push((step.label.clone().unwrap_or_else(|| format!("{n}")), shown));
            }
            "expect" | "expectNot" => {
                let found = term.lock().unwrap().transcript().contains(&text);
                if found != (step.action == "expect") {
                    failures.push(format!(
                        "step {n}: {} {text:?}",
                        if found { "unexpected" } else { "did not find" }
                    ));
                }
            }
            "exit" => {
                let ok = poll(timeout, || matches!(child.try_wait(), Ok(Some(_))));
                if !ok {
                    failures.push(format!(
                        "step {n}: process did not exit within {}ms",
                        step.timeout_ms
                    ));
                }
            }
            _ => unreachable!("checked above"),
        }
    }

    // The end: wait for the process to exit, kill it when it lingers.
    let mut status = None;
    let deadline = Instant::now() + Duration::from_millis(inter.exit_timeout_ms);
    while Instant::now() < deadline {
        if let Ok(Some(s)) = child.try_wait() {
            status = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if status.is_none() {
        failures.push(format!(
            "process still running {}ms after the last step: killed",
            inter.exit_timeout_ms
        ));
        let _ = child.kill();
        status = child.wait().ok();
    }
    // Let the last output arrive, then close the pseudo-console so the reader sees the end.
    std::thread::sleep(Duration::from_millis(300));
    drop(writer);
    drop(pair.master);
    let _ = poll(Duration::from_secs(5), || reader_thread.is_finished());

    let mut t = term.lock().unwrap();
    let mut screen = t.scrollback();
    screen.extend(t.visible());
    let screen = screen
        .iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    Ok(PtyRun {
        // A killed process reports no exit code (as `ExitStatus::code` does for a signal).
        exit_code: status
            .filter(|_| !failures.iter().any(|f| f.ends_with(": killed")))
            .map(|s| s.exit_code() as i32),
        failures,
        screen,
        snaps,
        raw: std::mem::take(&mut t.raw),
    })
}
