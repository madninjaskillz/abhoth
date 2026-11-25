//! The file log (`OpenOrca.Cli.Logging.FileLoggerProvider`): `<config home>/logs/openorca-{date}.log`,
//! one file per day, appended on each run, lines as
//! `HH:mm:ss.fff [INF] Category: message`. The bench reads its runs' "Slot usage" lines from
//! there (`BenchRunner.Metrics`), so the format matches the C# one exactly.
//!
//! The process's own log is set once at startup ([`set_process_log`]); an in-process bench
//! session writes to a log of its own run home instead (C# runs a worker process per run).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// `LogLevel`, as far as the file log tells them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Critical,
}

impl Level {
    /// `FileLoggerProvider.LevelTag`.
    pub fn tag(self) -> &'static str {
        match self {
            Level::Trace => "TRC",
            Level::Debug => "DBG",
            Level::Info => "INF",
            Level::Warn => "WRN",
            Level::Error => "ERR",
            Level::Critical => "CRT",
        }
    }
}

/// Where log entries go (`ILogger` for the parts that write to the file log).
pub trait LogSink: Send + Sync {
    /// One entry. `category` is the logger's category; the file log keeps its last segment
    /// (`OpenOrca.Usage` -> `Usage`).
    fn log(&self, level: Level, category: &str, message: &str);
}

/// `FileLogger`'s category shortening: the part after the last `.` (or `::`).
pub fn short_category(category: &str) -> &str {
    let after_colons = category.rsplit("::").next().unwrap_or(category);
    after_colons.rsplit('.').next().unwrap_or(after_colons)
}

struct State {
    date: String,
    file: Option<File>,
}

/// The daily log file under `<dir>/logs`.
pub struct FileLog {
    dir: PathBuf,
    state: Mutex<State>,
}

impl FileLog {
    /// Opens (creates) today's file under `<config_dir>/logs` and writes the "log opened" line,
    /// as the C# provider does when it is constructed.
    pub fn open(config_dir: &Path) -> Arc<Self> {
        let dir = config_dir.join("logs");
        let _ = std::fs::create_dir_all(&dir);
        let log = Arc::new(Self {
            dir,
            state: Mutex::new(State {
                date: String::new(),
                file: None,
            }),
        });
        log.ensure(&mut log.state.lock().unwrap());
        log
    }

    /// The logs folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `EnsureWriter`: a new file when the day changes. Tries `openorca-{date}.log`, then
    /// `.1`, `.2`, ... when one cannot be opened; without any, entries are dropped.
    fn ensure(&self, state: &mut State) {
        let now = chrono::Local::now();
        let today = now.format("%Y-%m-%d").to_string();
        if state.date == today && state.file.is_some() {
            return;
        }
        state.date = today.clone();
        state.file = None;
        for i in 0..100 {
            let suffix = if i == 0 {
                String::new()
            } else {
                format!(".{i}")
            };
            let path = self.dir.join(format!("openorca-{today}{suffix}.log"));
            // std opens files shared for reading and writing on Windows (FileShare.ReadWrite).
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
                let header = format!(
                    "--- OpenOrca log opened at {} ---{}",
                    now.format("%Y-%m-%d %H:%M:%S"),
                    NEWLINE
                );
                if f.write_all(header.as_bytes()).is_ok() {
                    state.file = Some(f);
                    return;
                }
            }
        }
    }
}

/// `StreamWriter.WriteLine`: the platform line break.
const NEWLINE: &str = if cfg!(windows) { "\r\n" } else { "\n" };

impl LogSink for FileLog {
    fn log(&self, level: Level, category: &str, message: &str) {
        let mut state = self.state.lock().unwrap();
        self.ensure(&mut state);
        if let Some(f) = state.file.as_mut() {
            let line = format!(
                "{} [{}] {}: {}{}",
                chrono::Local::now().format("%H:%M:%S%.3f"),
                level.tag(),
                short_category(category),
                message,
                NEWLINE
            );
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// Entries kept in memory (tests).
#[derive(Default)]
pub struct MemoryLog {
    pub lines: Mutex<Vec<(Level, String, String)>>,
}

impl MemoryLog {
    /// The messages logged so far.
    pub fn messages(&self) -> Vec<String> {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, m)| m.clone())
            .collect()
    }
}

impl LogSink for MemoryLog {
    fn log(&self, level: Level, category: &str, message: &str) {
        self.lines
            .lock()
            .unwrap()
            .push((level, category.to_string(), message.to_string()));
    }
}

static PROCESS_LOG: OnceLock<Arc<FileLog>> = OnceLock::new();

/// Sets this process's file log (once, at startup); later calls keep the first.
pub fn set_process_log(log: Arc<FileLog>) {
    let _ = PROCESS_LOG.set(log);
}

/// This process's file log, if one was set.
pub fn process_log() -> Option<Arc<FileLog>> {
    PROCESS_LOG.get().cloned()
}

/// [`process_log`] as a sink.
pub fn process_sink() -> Option<Arc<dyn LogSink>> {
    process_log().map(|l| l as Arc<dyn LogSink>)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_category_keeps_the_last_segment() {
        assert_eq!(short_category("OpenOrca.Usage"), "Usage");
        assert_eq!(short_category("orca_core::agent::turn"), "turn");
        assert_eq!(short_category("Slots"), "Slots");
    }

    #[test]
    fn file_log_writes_the_csharp_line_format() {
        let home = tempfile::tempdir().unwrap();
        let log = FileLog::open(home.path());
        log.log(
            Level::Info,
            "OpenOrca.Usage",
            "Slot usage lead on provider: x",
        );
        log.log(Level::Warn, "Slots", "careful");
        let files: Vec<_> = std::fs::read_dir(home.path().join("logs"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1);
        let name = files[0].file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("openorca-") && name.ends_with(".log"),
            "{name}"
        );
        let text = std::fs::read_to_string(&files[0]).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("--- OpenOrca log opened at "));
        let re = regex::Regex::new(
            r"^\d\d:\d\d:\d\d\.\d{3} \[INF\] Usage: Slot usage lead on provider: x$",
        )
        .unwrap();
        assert!(re.is_match(lines[1]), "{}", lines[1]);
        assert!(lines[2].ends_with(" [WRN] Slots: careful"));
    }
}
