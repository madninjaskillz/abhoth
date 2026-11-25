//! `AgentBadge`: chat lines that mention "Agent A" get the agent's coloured letter block in
//! front, so they match its footer bar at a glance: `[A] Agent A: found 3 failing tests`.

use std::sync::{Arc, RwLock};

use orca_core::orchestration::palette;

use crate::commands::agent_cmds::badge_markup;
use crate::ui::markup::escape;

/// Whether a letter belongs to an agent the user can still see or recall.
pub type KnownLetter = Arc<dyn Fn(char) -> bool + Send + Sync>;

static IS_KNOWN_LETTER: RwLock<Option<KnownLetter>> = RwLock::new(None);

/// `AgentBadge.IsKnownLetter`: wired to the orchestrator at startup; `None` decorates nothing.
pub fn set_known_letter(known: Option<KnownLetter>) {
    *IS_KNOWN_LETTER.write().unwrap_or_else(|e| e.into_inner()) = known;
}

fn known() -> Option<KnownLetter> {
    IS_KNOWN_LETTER
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The "Agent X" mentions in `text` (byte range, letter): `(?<!\w)Agent [A-KM-Z](?!\w)`.
fn mentions(text: &str) -> Vec<(usize, usize, char)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find("Agent ").map(|i| from + i) {
        from = i + "Agent ".len();
        if text[..i].chars().next_back().is_some_and(is_word) {
            continue;
        }
        let mut rest = text[from..].chars();
        let Some(letter) = rest.next() else { break };
        if !letter.is_ascii_uppercase() || letter == 'L' {
            continue;
        }
        if rest.next().is_some_and(is_word) {
            continue;
        }
        out.push((i, from + 1, letter));
    }
    out
}

fn safe_known(known: &KnownLetter, letter: char) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| known(letter))).unwrap_or(false)
}

/// `DecorateMarkup`: `text` as markup, with the badge before each "Agent X" that names a known
/// agent. Plain segments get `style` (e.g. "dim"); the badge never inherits it. `escape_fn`
/// turns plain text into markup (default: escape it).
pub fn decorate_markup(
    text: &str,
    style: Option<&str>,
    escape_fn: Option<&dyn Fn(&str) -> String>,
) -> String {
    let esc = |s: &str| escape_fn.map_or_else(|| escape(s), |f| f(s));
    let seg = |s: &str| match (s.is_empty(), style) {
        (true, _) => String::new(),
        (false, None) => esc(s),
        (false, Some(st)) => format!("[{st}]{}[/]", esc(s)),
    };
    let Some(known) = known().filter(|_| text.contains("Agent ")) else {
        return seg(text);
    };
    let mut out = String::new();
    let mut pos = 0;
    for (start, end, letter) in mentions(text) {
        let slot = palette::slot_of(letter);
        if slot < 0 || !safe_known(&known, letter) {
            continue;
        }
        out.push_str(&seg(&text[pos..start]));
        out.push_str(&badge_markup(slot));
        out.push(' ');
        out.push_str(&seg(&text[start..end]));
        pos = end;
    }
    out.push_str(&seg(&text[pos..]));
    out
}

/// `CountBadges`: how many mentions [`decorate_markup`] would badge (each adds 4 cells).
pub fn count_badges(text: &str) -> usize {
    let Some(known) = known().filter(|_| text.contains("Agent ")) else {
        return 0;
    };
    mentions(text)
        .into_iter()
        .filter(|(_, _, l)| safe_known(&known, *l))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test touches the global so parallel tests don't race on it.
    #[test]
    fn mentions_of_known_agents_get_their_badge() {
        set_known_letter(None);
        assert_eq!(
            decorate_markup("Agent A done", Some("dim"), None),
            "[dim]Agent A done[/]"
        );
        assert_eq!(count_badges("Agent A"), 0);

        set_known_letter(Some(Arc::new(|l| l == 'A' || l == 'B')));
        let a = badge_markup(palette::slot_of('A'));
        assert_eq!(
            decorate_markup("Agent A: found [x]", None, None),
            format!("{a} Agent A: found [[x]]")
        );
        assert_eq!(
            decorate_markup("see Agent A, not Agent C", Some("dim"), None),
            format!("[dim]see [/]{a} [dim]Agent A[/][dim], not Agent C[/]")
        );
        // Words around the mention, the Lead and lowercase letters are left alone.
        assert_eq!(decorate_markup("SubAgent A", None, None), "SubAgent A");
        assert_eq!(decorate_markup("Agent AB", None, None), "Agent AB");
        assert_eq!(decorate_markup("Agent L", None, None), "Agent L");
        assert_eq!(count_badges("Agent A and Agent B, Agent C"), 2);
        set_known_letter(None);
    }
}
