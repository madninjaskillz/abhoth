//! Degenerate repetition in a streamed reply (port of `RepetitionDetector.cs`).
//!
//! The last [`WINDOW`] chars are one short unit (at most [`MAX_PERIOD`] chars) repeated over
//! and over, e.g. "1.2.3-1.2.3-1.2.3-…". A local model stuck like that runs on to the output
//! token cap (minutes) and its reply is lost anyway. Content and reasoning are watched
//! separately. Like C#, all counts are UTF-16 units (the buffers hold UTF-16).

/// UTF-16 units of the tail that must be periodic (at least 12 repeats of the longest unit).
pub const WINDOW: usize = 2400;

/// Longest repeated unit looked for, in UTF-16 units.
pub const MAX_PERIOD: usize = 200;

/// The tail is checked once per this many new UTF-16 units.
pub const CHECK_EVERY: usize = 256;

#[derive(Debug, Default, Clone)]
struct Stream {
    buffer: Vec<u16>,
    since_check: usize,
}

/// Watches a streamed reply (content and reasoning separately) for degenerate repetition.
#[derive(Debug, Default, Clone)]
pub struct RepetitionDetector {
    content: Stream,
    reasoning: Stream,
    found: Option<String>,
}

impl RepetitionDetector {
    /// A detector that has seen nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// The repeated unit once found (stays set), else `None`.
    pub fn found(&self) -> Option<&str> {
        self.found.as_deref()
    }

    /// Add streamed reply text; returns true once repetition is found.
    pub fn add_content(&mut self, text: Option<&str>) -> bool {
        Self::add(&mut self.found, &mut self.content, text)
    }

    /// Add streamed reasoning text; returns true once repetition is found.
    pub fn add_reasoning(&mut self, text: Option<&str>) -> bool {
        Self::add(&mut self.found, &mut self.reasoning, text)
    }

    fn add(found: &mut Option<String>, stream: &mut Stream, text: Option<&str>) -> bool {
        if found.is_some() {
            return true;
        }
        let Some(text) = text.filter(|t| !t.is_empty()) else {
            return false;
        };
        let units: Vec<u16> = text.encode_utf16().collect();
        stream.since_check += units.len();
        stream.buffer.extend_from_slice(&units);
        if stream.buffer.len() > WINDOW * 2 {
            let excess = stream.buffer.len() - WINDOW;
            stream.buffer.drain(..excess);
        }
        if stream.since_check < CHECK_EVERY || stream.buffer.len() < WINDOW {
            return false;
        }
        stream.since_check = 0;
        *found = find_repeat_units(&stream.buffer, WINDOW, MAX_PERIOD);
        found.is_some()
    }
}

/// The shortest unit (at most `max_period` UTF-16 units) whose repetition makes up the last
/// `window` UTF-16 units of `text`, or `None`.
pub fn find_repeat(text: &str, window: usize, max_period: usize) -> Option<String> {
    let units: Vec<u16> = text.encode_utf16().collect();
    find_repeat_units(&units, window, max_period)
}

fn find_repeat_units(text: &[u16], window: usize, max_period: usize) -> Option<String> {
    if text.len() < window || window <= max_period {
        return None;
    }
    let tail = &text[text.len() - window..];
    (1..=max_period)
        .find(|&p| (p..tail.len()).all(|i| tail[i] == tail[i - p]))
        .map(|p| String::from_utf16_lossy(&tail[tail.len() - p..]))
}
