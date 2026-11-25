//! The colour/letter scheme of the footer's progress bars (port of `AgentPalette.cs`).
//!
//! An agent's slot is its letter: A, B, C … in order, unique for the session (a later agent
//! never reuses a finished agent's letter, so "Agent C" always means the same agent to the
//! Lead). The first eight letters keep their fixed colours — blue A, green B, yellow C,
//! purple D, orange E, red F, brown G, white H — and later letters cycle through the same
//! eight (I is blue again). The Lead (main agent) has its own neutral grey "L" bar, so agent
//! letters skip L. Everything that shows an agent's tag (the bar, the agent view, "Agent A" in
//! chat) draws it from here so the colours never drift.
//!
//! Slots are `i32` as in C# (any value, normalised modulo the colour / letter count).

use std::time::Duration;

use super::types::{AgentOrigin, upper_invariant};

/// Colour names, by colour index.
pub const NAMES: [&str; 8] = [
    "blue", "green", "yellow", "purple", "orange", "red", "brown", "white",
];

/// Agent letters in assignment order: A–Z without the Lead's L (25 letters).
pub const LETTERS: [char; 25] = [
    'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T',
    'U', 'V', 'W', 'X', 'Y', 'Z',
];

/// xterm-256 colour of each colour (index = slot % [`COUNT`]).
pub const XTERM: [i32; 8] = [33, 40, 220, 135, 208, 196, 130, 252];

/// Number of colours.
pub const COUNT: i32 = NAMES.len() as i32;

/// Number of distinct letters (slots) before letters repeat in a very long session.
pub const LETTER_COUNT: i32 = LETTERS.len() as i32;

/// The Lead's bar: neutral grey, letter L — outside the agent letters.
pub const LEAD_LETTER: char = 'L';
/// The Lead bar's xterm-256 colour.
pub const LEAD_XTERM: i32 = 244;
/// The Lead bar's name.
pub const LEAD_NAME: &str = "Lead";

/// How long a finished agent keeps its colour (it stays on the footer that long).
pub const MODEL_HOLD: Duration = Duration::from_secs(4);

/// User-launched agents linger longer — the user wants to notice the result.
pub const USER_HOLD: Duration = Duration::from_secs(30);

/// How long an agent of `origin` keeps its colour after finishing.
pub fn hold_after_finish(origin: AgentOrigin) -> Duration {
    if origin == AgentOrigin::User {
        USER_HOLD
    } else {
        MODEL_HOLD
    }
}

/// The letter of a slot.
pub fn letter_of(slot: i32) -> char {
    LETTERS[normalize_letter(slot) as usize]
}

/// The xterm-256 colour of a slot.
pub fn xterm_of(slot: i32) -> i32 {
    XTERM[normalize_color(slot) as usize]
}

/// The colour name of a slot.
pub fn name_of(slot: i32) -> &'static str {
    NAMES[normalize_color(slot) as usize]
}

/// The colour index (0 = blue … 7 = white) of a slot.
pub fn color_of(slot: i32) -> i32 {
    normalize_color(slot)
}

/// Slot for a letter (A–Z except L, any case), or -1.
pub fn slot_of(letter: char) -> i32 {
    let up = upper_invariant(letter);
    LETTERS
        .iter()
        .position(|&l| l == up)
        .map_or(-1, |i| i as i32)
}

/// The colour slot not in `in_use` with the lowest index among the first [`COUNT`]; when all
/// are taken, wrap around. (Colour-only assignment; agents use [`assign_letter`].)
pub fn assign(in_use: &[i32]) -> i32 {
    let used: Vec<i32> = in_use.iter().map(|&s| normalize_color(s)).collect();
    (0..COUNT)
        .find(|i| !used.contains(i))
        .unwrap_or_else(|| (in_use.len() as i32) % COUNT)
}

/// The next agent letter (slot) for the session: the first letter no agent of this session
/// has had (`ever_used`) whose colour no agent on the footer holds (`colors_held`, as slots);
/// failing that, the first unused letter; and once all [`LETTER_COUNT`] letters are spent, the
/// least recently assigned one (`ever_used` in assignment order) that no agent on the footer
/// holds.
pub fn assign_letter(ever_used: &[i32], colors_held: &[i32]) -> i32 {
    let used: Vec<i32> = ever_used.iter().map(|&s| normalize_letter(s)).collect();
    let held: Vec<i32> = colors_held.iter().map(|&s| normalize_color(s)).collect();
    let mut first_unused = -1;
    for i in 0..LETTER_COUNT {
        if used.contains(&i) {
            continue;
        }
        if !held.contains(&normalize_color(i)) {
            return i;
        }
        if first_unused < 0 {
            first_unused = i;
        }
    }
    if first_unused >= 0 {
        return first_unused;
    }
    // Every letter used: recycle the oldest letter not on the footer right now.
    let held_letters: Vec<i32> = colors_held.iter().map(|&s| normalize_letter(s)).collect();
    if let Some(&s) = used.iter().find(|s| !held_letters.contains(s)) {
        return s;
    }
    normalize_letter(ever_used.len() as i32)
}

fn normalize_color(slot: i32) -> i32 {
    slot.rem_euclid(COUNT)
}

fn normalize_letter(slot: i32) -> i32 {
    slot.rem_euclid(LETTER_COUNT)
}
