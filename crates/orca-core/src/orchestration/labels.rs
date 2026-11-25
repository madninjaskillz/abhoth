//! Short bar labels (port of `AgentLabels.cs`): at most four words — the label the model gave,
//! else one derived from the task ("Find all usages of X in src" → "Find all usages"), else
//! the agent type.
//!
//! C# works on UTF-16 units and drops surrogates, so a character outside the BMP is dropped
//! here too; every length below is then a char count equal to the UTF-16 count.

use super::types::{eq_ci, upper_invariant};

/// Most words in a label.
pub const MAX_WORDS: usize = 4;
/// Longest label, in chars.
pub const MAX_CHARS: usize = 32;
const MAX_WORD_CHARS: usize = 16;

const LEADING_FILLER: [&str; 17] = [
    "please", "you", "are", "your", "task", "is", "to", "can", "could", "would", "kindly", "i",
    "want", "need", "a", "an", "the",
];

const TRAILING_STOP: [&str; 19] = [
    "a", "an", "the", "of", "to", "in", "on", "for", "with", "and", "or", "at", "by", "from",
    "into", "that", "this", "its", "their",
];

/// The label to show for an agent.
pub fn derive(label: Option<&str>, task: Option<&str>, agent_type: Option<&str>) -> String {
    let given = normalize(label);
    if !given.is_empty() {
        return given;
    }
    let from_task = from_task(task);
    if !from_task.is_empty() {
        return from_task;
    }
    let ty = normalize(agent_type);
    if ty.is_empty() {
        "Agent".to_string()
    } else {
        capitalize(&ty)
    }
}

/// Clean a label: printable single-width text, at most four words / 32 chars.
pub fn normalize(label: Option<&str>) -> String {
    let Some(label) = label.filter(|l| !l.trim().is_empty()) else {
        return String::new();
    };
    let words: Vec<String> = words(label).into_iter().take(MAX_WORDS).collect();
    cap(&words.join(" "))
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => format!("{}{}", upper_invariant(c), chars.as_str()),
        None => String::new(),
    }
}

fn from_task(task: Option<&str>) -> String {
    let Some(task) = task.filter(|t| !t.trim().is_empty()) else {
        return String::new();
    };
    // First line / sentence only.
    let no_cr = task.replace('\r', "");
    let mut first: &str = no_cr.split('\n').find(|l| !l.is_empty()).unwrap_or("");
    // Sentence end: punctuation followed by a space or the end ("alpha.txt" stays whole).
    // (C# starts at UTF-16 index 1; a low surrogate is never punctuation, so skipping the
    // first char is the same.)
    let chars: Vec<(usize, char)> = first.char_indices().collect();
    for i in 1..chars.len() {
        let (pos, c) = chars[i];
        if matches!(c, '.' | ':' | ';' | '?' | '!')
            && (i + 1 == chars.len() || chars[i + 1].1 == ' ')
        {
            first = &first[..pos];
            break;
        }
    }

    let mut words: Vec<String> = words(first)
        .into_iter()
        .map(|w| {
            w.trim_matches(|c| matches!(c, '"' | '\'' | '`' | ',' | '(' | ')' | '[' | ']'))
                .to_string()
        })
        .filter(|w| !w.is_empty())
        .collect();
    while words.len() > 1 && LEADING_FILLER.iter().any(|f| eq_ci(f, &words[0])) {
        words.remove(0);
    }
    // "Survey alpha.txt and report its marker" → the first clause names the job best.
    if words.len() > 2
        && let Some(clause) = words
            .iter()
            .skip(2)
            .position(|w| matches!(w.as_str(), "and" | "then" | "so" | "to"))
    {
        words.truncate(clause + 2);
    }
    words.truncate(MAX_WORDS);
    while words.len() > 1
        && TRAILING_STOP
            .iter()
            .any(|f| eq_ci(f, &words[words.len() - 1]))
    {
        words.pop();
    }
    if words.is_empty() {
        return String::new();
    }
    cap(&capitalize(&words.join(" ")))
}

fn words(text: &str) -> Vec<String> {
    let mut sb = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_whitespace() || ch.is_control() {
            sb.push(' ');
        } else if (ch as u32) <= 0xFFFF && !is_wide(ch) {
            // Single-width only: the label is drawn over a bar. (Non-BMP = surrogates in C#.)
            sb.push(ch);
        }
    }
    sb.split(' ')
        .filter(|w| !w.is_empty())
        .map(|w| {
            if w.chars().count() > MAX_WORD_CHARS {
                let head: String = w.chars().take(MAX_WORD_CHARS - 1).collect();
                format!("{head}…")
            } else {
                w.to_string()
            }
        })
        .collect()
}

/// East Asian wide / fullwidth characters (two terminal cells).
fn is_wide(c: char) -> bool {
    matches!(c,
        '\u{1100}'..='\u{115F}'
            | '\u{2E80}'..='\u{A4CF}'
            | '\u{AC00}'..='\u{D7A3}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{FE30}'..='\u{FE4F}'
            | '\u{FF00}'..='\u{FF60}'
            | '\u{FFE0}'..='\u{FFE6}')
}

fn cap(s: &str) -> String {
    if s.chars().count() <= MAX_CHARS {
        s.to_string()
    } else {
        let head: String = s.chars().take(MAX_CHARS - 1).collect();
        format!("{}…", head.trim_end())
    }
}
