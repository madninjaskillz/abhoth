//! The file log for this process's `tracing` events (`builder.Logging.AddProvider(new
//! FileLoggerProvider(LogLevel.Debug))` with the "OpenOrca" filter at Debug and everything else
//! at Warning): each event becomes a `HH:mm:ss.fff [LVL] category: message` line in
//! `<config home>/logs/openorca-{date}.log`, the category being the last segment of its target.

use std::fmt::Write as _;
use std::sync::Arc;

use orca_core::logging::{FileLog, Level, LogSink};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer};

/// The `tracing` layer that writes to a [`FileLog`].
pub struct FileLogLayer {
    log: Arc<FileLog>,
}

impl FileLogLayer {
    pub fn new(log: Arc<FileLog>) -> Self {
        Self { log }
    }
}

/// Our own crates at Debug, the rest (HTTP stack, ...) at Warning.
fn enabled(target: &str, level: Level) -> bool {
    if target.starts_with("orca") {
        level >= Level::Debug
    } else {
        level >= Level::Warn
    }
}

fn level_of(level: &tracing::Level) -> Level {
    match *level {
        tracing::Level::TRACE => Level::Trace,
        tracing::Level::DEBUG => Level::Debug,
        tracing::Level::INFO => Level::Info,
        tracing::Level::WARN => Level::Warn,
        tracing::Level::ERROR => Level::Error,
    }
}

#[derive(Default)]
struct Message {
    text: String,
    fields: String,
}

impl Visit for Message {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.text.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.text, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for FileLogLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let level = level_of(meta.level());
        if !enabled(meta.target(), level) {
            return;
        }
        let mut m = Message::default();
        event.record(&mut m);
        m.text.push_str(&m.fields);
        self.log.log(level, meta.target(), &m.text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn events_become_file_log_lines() {
        let home = tempfile::tempdir().unwrap();
        let log = FileLog::open(home.path());
        let subscriber = tracing_subscriber::registry().with(FileLogLayer::new(log.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "orca_core::agent::turn", "hello {}", 1);
            tracing::warn!(target: "orca_core::agent::permissions", pattern = "x", "bad");
            tracing::info!(target: "hyper::client", "noise");
        });
        let file = std::fs::read_dir(log.dir())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let text = std::fs::read_to_string(file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[1].ends_with(" [INF] turn: hello 1"), "{}", lines[1]);
        assert!(
            lines[2].ends_with(" [WRN] permissions: bad pattern=x"),
            "{}",
            lines[2]
        );
    }
}
