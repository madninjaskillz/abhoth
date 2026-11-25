//! `TerminalPanel`: a fixed input panel at the bottom of the terminal, kept out of the
//! conversation's way with an ANSI scroll region. From the bottom:
//!
//! ```text
//!   Rows above: agent bars (the Lead, a bridge's progress.js bars, sub-agents)
//!   Row H-5: info line (thinking / tool progress, paste notices)
//!   Row H-4: ──────────────── (top rule)
//!   Row H-3: ❯ input here
//!   Row H-2: ──────────────── (bottom rule)
//!   Row H-1: ● Normal · …     (status line)
//! ```
//!
//! The panel is shared (cheap clones): the agent footer repaints its rows from its own thread.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::ui::Console;
use crate::ui::console::cell_width;

use super::agent_footer::max_agent_rows_for;
use super::state::{InputMode, ReplState};

/// `StatusInfo`: the session facts on the status line (any may be missing).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatusInfo {
    pub profile: Option<String>,
    pub context_percent: Option<i32>,
    pub directory: Option<String>,
    pub branch: Option<String>,
    pub uncensored: bool,
}

/// Writes ANSI text to the panel's info line (any thread).
pub type InfoLineWriter = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
struct PanelState {
    active: bool,
    height: usize,
    width: usize,
    saved_row: usize,
    input_active: bool,
    /// Rows reserved for the agent bars, and what they show.
    agent_rows: usize,
    agent_lines: Vec<String>,
}

impl PanelState {
    /// 1-based last row of the scroll region.
    fn region_bottom(&self) -> usize {
        self.height.saturating_sub(5 + self.agent_rows).max(1)
    }

    fn agent_rows_text(&self) -> String {
        let (h, n) = (self.height, self.agent_rows);
        let mut s = String::new();
        for i in 0..n {
            let line = self.agent_lines.get(i).map_or("", String::as_str);
            s.push_str(&format!("\x1b[{};1H\x1b[2K{line}\x1b[0m", h - 4 - n + i));
        }
        s
    }
}

#[derive(Clone)]
pub struct TerminalPanel {
    console: Console,
    st: Arc<Mutex<PanelState>>,
}

impl TerminalPanel {
    pub fn new(console: Console) -> Self {
        Self {
            console,
            st: Arc::new(Mutex::new(PanelState::default())),
        }
    }

    fn st(&self) -> MutexGuard<'_, PanelState> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_active(&self) -> bool {
        self.st().active
    }

    pub fn width(&self) -> usize {
        let st = self.st();
        if st.active {
            st.width
        } else {
            self.console.width()
        }
    }

    /// `MaxAgentRows`: how many bar rows fit at this height.
    pub fn max_agent_rows(&self) -> usize {
        max_agent_rows_for(self.st().height)
    }

    /// `AgentRowCount`: rows reserved for the bars now.
    pub fn agent_row_count(&self) -> usize {
        self.st().agent_rows
    }

    /// 1-based row of the prompt line.
    pub fn prompt_row(height: usize) -> usize {
        height.saturating_sub(2)
    }

    fn supported(&self) -> bool {
        self.console.is_terminal()
            && crate::ui::prompt::interactive()
            && self.console.height() >= 11
    }

    /// `Setup`: the scroll region over the upper rows and the panel drawn under it.
    pub fn setup(&self, state: &ReplState, status: &StatusInfo) {
        if !self.supported() {
            return;
        }
        let mut st = self.st();
        st.height = self.console.height();
        st.width = self.console.width();
        st.agent_rows = st.agent_rows.min(max_agent_rows_for(st.height));
        st.active = true;
        let mut s = format!("\x1b[{};{}r", 1, st.region_bottom());
        s.push_str(&panel_text(&st, state, status));
        s.push_str("\x1b[1;1H");
        self.console.raw(&s);
    }

    /// `Redraw`: the panel rows again (mode or output level changed), cursor kept.
    pub fn redraw(&self, state: &ReplState, status: &StatusInfo) {
        let st = self.st();
        if !st.active {
            return;
        }
        let s = format!("\x1b7{}\x1b8", panel_text(&st, state, status));
        self.console.raw(&s);
    }

    /// `EnterInput`: remember the output row, refresh the status line, cursor to the prompt line.
    pub fn enter_input(&self, state: &ReplState, status: &StatusInfo) {
        if !self.is_active() {
            return;
        }
        self.relayout_if_resized(state, status);
        let row = crossterm::cursor::position()
            .map(|(_, r)| r as usize)
            .unwrap_or(0);
        let mut st = self.st();
        st.saved_row = row;
        st.input_active = true;
        let line = build_status_line(state, Some(status), st.width);
        let prompt = Self::prompt_row(st.height);
        self.console.raw(&format!(
            "\x1b7\x1b[{};1H\x1b[2K{line}\x1b8\x1b[{prompt};1H\x1b[2K",
            st.height
        ));
    }

    /// `ExitInput`: the cursor back on the output row (never below the scroll region).
    pub fn exit_input(&self) {
        let mut st = self.st();
        if !st.active {
            return;
        }
        st.input_active = false;
        let row = output_row_after_input(st.saved_row, st.region_bottom());
        self.console.raw(&format!("\x1b[{};1H", row + 1));
    }

    fn relayout_if_resized(&self, state: &ReplState, status: &StatusInfo) {
        let (h, w) = (self.console.height(), self.console.width());
        let mut st = self.st();
        if h == st.height && w == st.width {
            return;
        }
        if h < 11 {
            return; // too small to lay out; keep the old layout until it grows again
        }
        let row = crossterm::cursor::position()
            .map(|(_, r)| r as usize)
            .unwrap_or(0);
        st.height = h;
        st.width = w;
        st.agent_rows = st.agent_rows.min(max_agent_rows_for(h));
        // The bars were laid out for the old width: blank until the footer's next tick.
        st.agent_lines = vec![String::new(); st.agent_rows];
        let bottom = st.region_bottom();
        let out_row = output_row_after_input(row, bottom);
        let mut s = format!("\x1b[1;{bottom}r");
        // Blank everything under the output row: the old panel may have moved there.
        for r in (out_row + 2)..=h {
            s.push_str(&format!("\x1b[{r};1H\x1b[2K"));
        }
        s.push_str(&panel_text(&st, state, status));
        s.push_str(&format!("\x1b[{};1H", out_row + 1));
        self.console.raw(&s);
    }

    /// `Teardown`: full-screen scrolling again, the panel rows cleared.
    pub fn teardown(&self) {
        let mut st = self.st();
        if !st.active {
            return;
        }
        st.active = false;
        let mut s = String::from("\x1b[r");
        for r in (st.height.saturating_sub(4 + st.agent_rows))..=st.height {
            s.push_str(&format!("\x1b[{r};1H\x1b[2K"));
        }
        s.push_str(&format!("\x1b[{};1H", st.region_bottom()));
        self.console.raw(&s);
    }

    /// `SetAgentLines`: show `lines` in the bar rows, growing or shrinking the reserved area
    /// (growing scrolls the conversation up so nothing is overwritten; with `allow_grow` false
    /// the panel keeps its height). Only changed rows repaint.
    pub fn set_agent_lines(&self, lines: &[String], allow_grow: bool) {
        let mut st = self.st();
        if !st.active {
            return;
        }
        let mut wanted = lines.len().min(max_agent_rows_for(st.height));
        let old = st.agent_rows;
        if wanted > old && !allow_grow {
            wanted = old;
        }
        let h = st.height;
        let mut s = String::new();
        if wanted > old {
            let k = wanted - old;
            // LF at the bottom margin scrolls the region up; then shrink the region.
            s.push_str(&format!("\x1b7\x1b[{};1H{}", h - 5 - old, "\n".repeat(k)));
            s.push_str(&format!("\x1b[1;{}r\x1b8", h - 5 - wanted));
            if st.input_active {
                st.saved_row = st.saved_row.saturating_sub(k);
            } else {
                s.push_str(&format!("\x1b[{k}A")); // the cursor's line moved up with the content
            }
        } else if wanted < old {
            // Release rows back to the scroll region and blank them.
            s.push_str(&format!("\x1b7\x1b[1;{}r", h - 5 - wanted));
            for row in (h - 4 - old)..=(h - 5 - wanted) {
                s.push_str(&format!("\x1b[{row};1H\x1b[2K"));
            }
            s.push_str("\x1b8");
        }
        st.agent_rows = wanted;
        let new_lines: Vec<String> = lines.iter().take(wanted).cloned().collect();
        if wanted == old && new_lines == st.agent_lines {
            return; // nothing changed: no repaint, no flicker
        }
        let previous = std::mem::replace(&mut st.agent_lines, new_lines);
        s.push_str("\x1b7");
        if wanted == old && previous.len() == wanted {
            for (i, line) in st.agent_lines.iter().enumerate() {
                if previous[i] != *line {
                    s.push_str(&format!(
                        "\x1b[{};1H\x1b[2K{line}\x1b[0m",
                        h - 4 - wanted + i
                    ));
                }
            }
        } else {
            s.push_str(&st.agent_rows_text());
        }
        s.push_str("\x1b8");
        self.console.raw(&s);
    }

    /// Paint the bar rows again (something below the conversation erased them).
    pub fn repaint_agent_rows(&self) {
        let st = self.st();
        if !st.active || st.agent_rows == 0 {
            return;
        }
        self.console
            .raw(&format!("\x1b7{}\x1b8", st.agent_rows_text()));
    }

    /// `WriteInfoLine`: text on the pinned info line (cursor kept).
    pub fn write_info_line(&self, ansi: &str) {
        let st = self.st();
        if !st.active {
            return;
        }
        let row = st.height.saturating_sub(4);
        self.console
            .raw(&format!("\x1b7\x1b[{row};1H\x1b[2K{ansi}\x1b8"));
    }

    pub fn clear_info_line(&self) {
        self.write_info_line("");
    }
}

fn panel_text(st: &PanelState, state: &ReplState, status: &StatusInfo) -> String {
    let h = st.height;
    let rule = "─".repeat(st.width);
    format!(
        "{}\x1b[{};1H\x1b[2K\x1b[{};1H\x1b[2K\x1b[90m{rule}\x1b[0m\x1b[{};1H\x1b[2K\x1b[{};1H\x1b[2K\x1b[90m{rule}\x1b[0m\x1b[{h};1H\x1b[2K{}",
        st.agent_rows_text(),
        h - 4,
        h - 3,
        h - 2,
        h - 1,
        build_status_line(state, Some(status), st.width)
    )
}

/// `OutputRowAfterInput`: 0-based row to return to, never below the region.
pub fn output_row_after_input(saved_row: usize, region_bottom: usize) -> usize {
    saved_row.min(region_bottom.saturating_sub(1))
}

fn fg(n: u8) -> String {
    format!("\x1b[38;5;{n}m")
}

/// `BuildStatusLine`: mode and profile, context use, directory and branch on the left; output
/// level and key hints on the right. Lowest-priority segments go first so it never wraps.
pub fn build_status_line(state: &ReplState, info: Option<&StatusInfo>, width: usize) -> String {
    const RESET: &str = "\x1b[0m";
    const BOLD: &str = "\x1b[1m";
    let dim = fg(244);
    let (mode_color, mode_name) = match state.mode {
        InputMode::Plan => (fg(45), "Plan"),
        InputMode::Ask => (fg(213), "Ask"),
        InputMode::Normal => (fg(33), "Normal"),
    };
    let verbosity = state.verbosity_label();
    let profile_tag = info
        .and_then(|i| i.profile.as_deref())
        .filter(|p| !p.is_empty())
        .map(|p| format!(" · {p}"))
        .unwrap_or_default();
    let uncensored_tag = if info.is_some_and(|i| i.uncensored) {
        " · uncensored"
    } else {
        ""
    };
    // (priority: lower = kept longer, plain text for measuring, ANSI text)
    let mut left: Vec<(u8, String, String)> = vec![(
        0,
        format!("● {mode_name}{profile_tag}{uncensored_tag}"),
        format!(
            "{mode_color}● {mode_name}{RESET}{}{profile_tag}{RESET}{}{uncensored_tag}{RESET}",
            fg(141),
            fg(203)
        ),
    )];
    if let Some(dir) = info
        .and_then(|i| i.directory.as_deref())
        .filter(|d| !d.is_empty())
    {
        left.push((
            4,
            format!("📁 {dir}"),
            format!("{BOLD}{}📁{RESET} {BOLD}{}{dir}{RESET}", fg(80), fg(51)),
        ));
    }
    if let Some(branch) = info
        .and_then(|i| i.branch.as_deref())
        .filter(|b| !b.is_empty())
    {
        left.push((
            5,
            format!("🌿 {branch}"),
            format!("{}🌿 {branch}{RESET}", fg(118)),
        ));
    }
    if let Some(pct) = info.and_then(|i| i.context_percent) {
        const BAR: usize = 10;
        let filled =
            ((pct as f64 / 100.0 * BAR as f64).round() as i64).clamp(0, BAR as i64) as usize;
        let color = if pct >= 85 {
            203
        } else if pct >= 60 {
            214
        } else {
            118
        };
        let bar = format!(
            "{}{}{RESET}{}{}{RESET}",
            fg(color),
            "▓".repeat(filled),
            fg(238),
            "░".repeat(BAR - filled)
        );
        left.push((
            1,
            format!("🧠 {} ctx {pct}%", "▓".repeat(BAR)),
            format!(
                "{}🧠{RESET} {bar} {BOLD}{}ctx {pct}%{RESET}",
                fg(color),
                fg(color)
            ),
        ));
    }
    let mut right: Vec<(u8, String, String)> = vec![
        (
            3,
            format!("ctrl+o {verbosity}"),
            format!("{dim}ctrl+o{RESET} {}{verbosity}{RESET}", fg(179)),
        ),
        (
            6,
            "shift+tab mode".into(),
            format!("{dim}shift+tab mode{RESET}"),
        ),
        (7, "/help".into(), format!("{dim}/help{RESET}")),
    ];
    const SEP: &str = " │ ";
    let measure = |segs: &[(u8, String, String)]| {
        segs.iter().map(|s| cell_width(&s.1)).sum::<usize>()
            + segs.len().saturating_sub(1) * SEP.chars().count()
    };
    let budget = width.saturating_sub(3).max(10);
    while measure(&left)
        + if right.is_empty() {
            0
        } else {
            measure(&right) + 2
        }
        > budget
    {
        let max_left = left
            .iter()
            .enumerate()
            .filter(|(_, s)| s.0 > 0)
            .max_by_key(|(_, s)| s.0)
            .map(|(i, s)| (i, s.0));
        let max_right = right
            .iter()
            .enumerate()
            .filter(|(_, s)| s.0 > 0)
            .max_by_key(|(_, s)| s.0)
            .map(|(i, s)| (i, s.0));
        match (max_left, max_right) {
            (None, None) => break,
            (Some((i, p)), Some((_, q))) if p >= q => {
                left.remove(i);
            }
            (Some((i, _)), None) => {
                left.remove(i);
            }
            (_, Some((j, _))) => {
                right.remove(j);
            }
        }
    }
    let sep_ansi = format!("\x1b[2m{}{SEP}{RESET}", fg(240));
    let left_ansi: Vec<&str> = left.iter().map(|s| s.2.as_str()).collect();
    let left_ansi = left_ansi.join(&sep_ansi);
    let line = if right.is_empty() {
        format!("  {left_ansi}")
    } else {
        let gap = budget
            .saturating_sub(measure(&left) + measure(&right))
            .max(2);
        let right_ansi: Vec<&str> = right.iter().map(|s| s.2.as_str()).collect();
        format!(
            "  {left_ansi}{}{}",
            " ".repeat(gap),
            right_ansi.join(&sep_ansi)
        )
    };
    fit_width(&line, width.saturating_sub(1).max(1))
}

/// Cut ANSI text to `max` visible cells (escape sequences kept, reset at the end when cut).
pub(crate) fn fit_width(ansi: &str, max: usize) -> String {
    let plain = crate::ui::console::strip_ansi(ansi);
    if cell_width(&plain) <= max {
        return ansi.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    let mut chars = ansi.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            for t in chars.by_ref() {
                out.push(t);
                if t.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > max {
            break;
        }
        w += cw;
        out.push(c);
    }
    out.push_str("\x1b[0m");
    out
}

/// `StatusInfoProvider.ReadGitBranch`: the branch (or short commit when detached) of the
/// repository containing `start`, read from `.git/HEAD` (worktrees: a `gitdir:` file).
pub fn read_git_branch(start: &Path) -> Option<String> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let dot_git = d.join(".git");
        let git_dir = if dot_git.is_dir() {
            Some(dot_git)
        } else if dot_git.is_file() {
            let line = std::fs::read_to_string(&dot_git).ok()?;
            line.trim()
                .strip_prefix("gitdir:")
                .map(|p| orca_abstractions::full_path(d, p.trim()))
        } else {
            None
        };
        if let Some(g) = git_dir {
            let head = std::fs::read_to_string(g.join("HEAD")).ok()?;
            let head = head.trim();
            if let Some(b) = head.strip_prefix("ref: refs/heads/") {
                return Some(b.to_string());
            }
            return (head.len() >= 7).then(|| head[..7].to_string());
        }
        dir = d.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::console::strip_ansi;

    #[test]
    fn status_line_never_exceeds_width() {
        let state = ReplState::default();
        let info = StatusInfo {
            profile: Some("pi".into()),
            context_percent: Some(42),
            directory: Some("openorca".into()),
            branch: Some("main".into()),
            uncensored: false,
        };
        for width in [30, 56, 80, 120] {
            let line = build_status_line(&state, Some(&info), width);
            assert!(cell_width(&strip_ansi(&line)) < width, "{width}");
        }
        let wide = strip_ansi(&build_status_line(&state, Some(&info), 160));
        assert!(wide.contains("● Normal · pi"));
        assert!(wide.contains("ctx 42%"));
        assert!(wide.contains("ctrl+o normal"));
    }

    #[test]
    fn output_row_is_clamped_to_region() {
        assert_eq!(output_row_after_input(3, 20), 3);
        assert_eq!(output_row_after_input(30, 20), 19);
    }

    #[test]
    fn git_branch_from_head() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".git")).unwrap();
        std::fs::write(d.path().join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        std::fs::create_dir_all(d.path().join("sub")).unwrap();
        assert_eq!(
            read_git_branch(&d.path().join("sub")).as_deref(),
            Some("feature/x")
        );
        std::fs::write(d.path().join(".git/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(read_git_branch(d.path()).as_deref(), Some("0123456"));
    }
}
