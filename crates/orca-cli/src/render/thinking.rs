//! `ThinkingIndicator`: the swimming-orca spinner with elapsed time and a live token count
//! while the model thinks. It animates on a background thread every 120 ms, writing either a
//! `\r` line on the console or (when the REPL has its bottom panel) through a callback such
//! as `TerminalPanel::write_info_line`.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::ui::console::Console;

const THINKING_WORDS: [&str; 33] = [
    "Thinking",
    "Pondering",
    "Mulling",
    "Reasoning",
    "Contemplating",
    "Deliberating",
    "Cogitating",
    "Ruminating",
    "Reflecting",
    "Musing",
    "Noodling",
    "Chewing on it",
    "Brainstorming",
    "Processing",
    "Figuring it out",
    "Working it out",
    "Crunching",
    "Brewing",
    "Percolating",
    "Stewing",
    "Cooking up",
    "Conjuring",
    "Daydreaming",
    "Scheming",
    "Plotting",
    "Hatching a plan",
    "Deep in thought",
    "Spinning gears",
    "Connecting dots",
    "Mind-melding",
    "Decoding",
    "Untangling",
    "Assembling thoughts",
];

// Orca-themed animation frames — a little swimming orca.
const FRAMES: [&str; 8] = [
    "   ~•>     ",
    "    ~•>    ",
    "     ~•>   ",
    "      ~•>  ",
    "       ~•> ",
    "      ~•>  ",
    "     ~•>   ",
    "    ~•>    ",
];

/// Where the indicator draws.
#[derive(Clone)]
pub enum IndicatorOutput {
    /// A `\r`-rewritten line on the console (cleared with `\r ESC[K` on stop).
    Console(Console),
    /// The panel's info line: `write` gets the ANSI line, `clear` removes it.
    Panel {
        write: Arc<dyn Fn(&str) + Send + Sync>,
        clear: Arc<dyn Fn() + Send + Sync>,
    },
    /// Nowhere (tests, benchmark mode).
    Null,
}

impl IndicatorOutput {
    fn render(&self, content: &str) {
        let ansi = format!("\x1b[2m\x1b[36m{content}\x1b[0m");
        match self {
            IndicatorOutput::Console(c) => c.raw(&format!("\r{ansi}\x1b[K")),
            IndicatorOutput::Panel { write, .. } => write(&ansi),
            IndicatorOutput::Null => {}
        }
    }

    fn clear(&self) {
        match self {
            IndicatorOutput::Console(c) => c.raw("\r\x1b[K"),
            IndicatorOutput::Panel { clear, .. } => clear(),
            IndicatorOutput::Null => {}
        }
    }
}

struct Shared {
    word: &'static str,
    started: Instant,
    /// Elapsed time frozen at stop (ms; -1 while running).
    stopped_at_ms: AtomicI64,
    token_count: AtomicI64,
    receiving_tokens: AtomicBool,
    budget_tokens: AtomicI64,
}

impl Shared {
    fn elapsed(&self) -> Duration {
        match self.stopped_at_ms.load(Ordering::Relaxed) {
            ms if ms >= 0 => Duration::from_millis(ms as u64),
            _ => self.started.elapsed(),
        }
    }

    fn status_line(&self, frame: usize) -> String {
        let orca = FRAMES[frame % FRAMES.len()];
        let elapsed = self.elapsed();
        let time = format_elapsed(elapsed);
        if self.receiving_tokens.load(Ordering::Relaxed) {
            let tokens = self.token_count.load(Ordering::Relaxed);
            let budget = self.budget_tokens.load(Ordering::Relaxed);
            let secs = elapsed.as_secs_f64();
            let tps = if secs > 0.5 {
                format!(" · {:.1} tok/s", tokens as f64 / secs)
            } else {
                String::new()
            };
            let shown = if budget > 0 {
                format!("{tokens}/{budget} tokens")
            } else {
                format!("{tokens} tokens")
            };
            return format!("{orca} {}... {time} · {shown}{tps}", self.word);
        }
        format!("{orca} {}... {time}", self.word)
    }
}

/// `FormatElapsed`: "4.2s", or "1m 05s" from a minute on.
pub fn format_elapsed(elapsed: Duration) -> String {
    if elapsed.as_secs() >= 60 {
        format!("{}m {:02}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    }
}

pub struct ThinkingIndicator {
    shared: Arc<Shared>,
    output: IndicatorOutput,
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<()>>,
    stopped: bool,
}

impl ThinkingIndicator {
    /// Start animating on `output`.
    pub fn new(output: IndicatorOutput) -> Self {
        // A random word (C# Random.Shared): the clock's nanoseconds are enough.
        let pick = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as usize)
            .unwrap_or(0);
        let shared = Arc::new(Shared {
            word: THINKING_WORDS[pick % THINKING_WORDS.len()],
            started: Instant::now(),
            stopped_at_ms: AtomicI64::new(-1),
            token_count: AtomicI64::new(0),
            receiving_tokens: AtomicBool::new(false),
            budget_tokens: AtomicI64::new(0),
        });
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let thread = {
            let shared = shared.clone();
            let stop = stop.clone();
            let output = output.clone();
            std::thread::spawn(move || {
                let mut frame = 0;
                output.render(&shared.status_line(0));
                let (lock, cvar) = &*stop;
                let mut stopped = lock.lock().unwrap();
                loop {
                    let (guard, _) = cvar
                        .wait_timeout(stopped, Duration::from_millis(120))
                        .unwrap();
                    stopped = guard;
                    if *stopped {
                        break;
                    }
                    frame = (frame + 1) % FRAMES.len();
                    output.render(&shared.status_line(frame));
                }
            })
        };
        Self {
            shared,
            output,
            stop,
            thread: Some(thread),
            stopped: false,
        }
    }

    /// Thinking token budget; 0 = unlimited.
    pub fn budget_tokens(&self) -> i64 {
        self.shared.budget_tokens.load(Ordering::Relaxed)
    }

    pub fn set_budget_tokens(&self, budget: i64) {
        self.shared.budget_tokens.store(budget, Ordering::Relaxed);
    }

    /// The live token count (from the streaming loop).
    pub fn update_token_count(&self, tokens: i64) {
        self.shared.token_count.store(tokens, Ordering::Relaxed);
        self.shared.receiving_tokens.store(true, Ordering::Relaxed);
    }

    /// The current status text (without colour).
    pub fn status_line(&self) -> String {
        self.shared.status_line(0)
    }

    /// Stop the animation and clear its line. Safe to call more than once.
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.shared.stopped_at_ms.store(
            self.shared.started.elapsed().as_millis() as i64,
            Ordering::Relaxed,
        );
        {
            let (lock, cvar) = &*self.stop;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.output.clear();
    }
}

impl Drop for ThinkingIndicator {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    //! Ported from ThinkingIndicatorTests.cs, plus the status line and the console output.
    use super::*;

    #[test]
    fn budget_tokens_defaults_to_zero() {
        let indicator = ThinkingIndicator::new(IndicatorOutput::Null);
        assert_eq!(indicator.budget_tokens(), 0);
    }

    #[test]
    fn budget_tokens_can_be_set() {
        let indicator = ThinkingIndicator::new(IndicatorOutput::Null);
        indicator.set_budget_tokens(200);
        assert_eq!(indicator.budget_tokens(), 200);
    }

    #[test]
    fn stop_can_be_called_multiple_times() {
        let mut indicator = ThinkingIndicator::new(IndicatorOutput::Null);
        indicator.stop();
        indicator.stop();
    }

    #[test]
    fn update_token_count_sets_receiving_tokens() {
        let mut indicator = ThinkingIndicator::new(IndicatorOutput::Null);
        indicator.update_token_count(42);
        indicator.set_budget_tokens(100);
        assert!(indicator.status_line().contains("42/100 tokens"));
        indicator.stop();
    }

    #[test]
    fn format_elapsed_seconds_and_minutes() {
        assert_eq!(format_elapsed(Duration::from_millis(4200)), "4.2s");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "1m 05s");
    }

    #[test]
    fn console_output_draws_and_clears_the_line() {
        let (console, buf) = Console::buffer();
        let mut indicator = ThinkingIndicator::new(IndicatorOutput::Console(console));
        indicator.stop();
        let out = Console::buffer_text(&buf);
        // (The test console drops control characters such as \r.)
        assert!(out.starts_with("   ~•>      "), "{out:?}");
        assert!(out.contains("... 0."));
    }

    #[test]
    fn panel_output_uses_the_callbacks() {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let cleared = Arc::new(AtomicBool::new(false));
        let (l, c) = (lines.clone(), cleared.clone());
        let mut indicator = ThinkingIndicator::new(IndicatorOutput::Panel {
            write: Arc::new(move |s| l.lock().unwrap().push(s.to_string())),
            clear: Arc::new(move || c.store(true, Ordering::Relaxed)),
        });
        indicator.stop();
        assert!(lines.lock().unwrap()[0].starts_with("\x1b[2m\x1b[36m"));
        assert!(cleared.load(Ordering::Relaxed));
    }
}
