//! `TerminalPanel`: a fixed input panel at the bottom of the terminal, kept out of the
//! conversation's way with an ANSI scroll region. From the bottom:
//!
//! ```text
//!   Row H-5: info line (thinking / tool progress, paste notices)
//!   Row H-4: ──────────────── (top rule)
//!   Row H-3: ❯ input here
//!   Row H-2: ──────────────── (bottom rule)
//!   Row H-1: ● Normal · …     (status line)
//! ```
//!
//! The agent bar rows of the C# panel (sub-agents) are not part of this port yet.

use std::path::Path;

use crate::ui::Console;
use crate::ui::console::cell_width;

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

pub struct TerminalPanel {
    console: Console,
    active: bool,
    height: usize,
    width: usize,
    saved_row: usize,
}

impl TerminalPanel {
    pub fn new(console: Console) -> Self {
        Self {
            console,
            active: false,
            height: 0,
            width: 0,
            saved_row: 0,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn width(&self) -> usize {
        if self.active {
            self.width
        } else {
            self.console.width()
        }
    }

    /// 1-based last row of the scroll region.
    fn region_bottom(&self) -> usize {
        self.height.saturating_sub(5).max(1)
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
    pub fn setup(&mut self, state: &ReplState, status: &StatusInfo) {
        if !self.supported() {
            return;
        }
        self.height = self.console.height();
        self.width = self.console.width();
        self.active = true;
        let mut s = format!("\x1b[{};{}r", 1, self.region_bottom());
        s.push_str(&self.panel_text(state, status));
        s.push_str("\x1b[1;1H");
        self.console.raw(&s);
    }

    /// `Redraw`: the panel rows again (mode or output level changed), cursor kept.
    pub fn redraw(&mut self, state: &ReplState, status: &StatusInfo) {
        if !self.active {
            return;
        }
        let s = format!("\x1b7{}\x1b8", self.panel_text(state, status));
        self.console.raw(&s);
    }

    /// `EnterInput`: remember the output row, refresh the status line, cursor to the prompt line.
    pub fn enter_input(&mut self, state: &ReplState, status: &StatusInfo) {
        if !self.active {
            return;
        }
        self.relayout_if_resized(state, status);
        self.saved_row = crossterm::cursor::position()
            .map(|(_, r)| r as usize)
            .unwrap_or(0);
        let line = build_status_line(state, Some(status), self.width);
        let prompt = Self::prompt_row(self.height);
        self.console.raw(&format!(
            "\x1b7\x1b[{};1H\x1b[2K{line}\x1b8\x1b[{prompt};1H\x1b[2K",
            self.height
        ));
    }

    /// `ExitInput`: the cursor back on the output row (never below the scroll region).
    pub fn exit_input(&mut self) {
        if !self.active {
            return;
        }
        let row = output_row_after_input(self.saved_row, self.region_bottom());
        self.console.raw(&format!("\x1b[{};1H", row + 1));
    }

    fn relayout_if_resized(&mut self, state: &ReplState, status: &StatusInfo) {
        let (h, w) = (self.console.height(), self.console.width());
        if h == self.height && w == self.width {
            return;
        }
        let row = crossterm::cursor::position()
            .map(|(_, r)| r as usize)
            .unwrap_or(0);
        self.height = h;
        self.width = w;
        let bottom = self.region_bottom();
        let out_row = output_row_after_input(row, bottom);
        let mut s = format!("\x1b[1;{bottom}r");
        // Blank everything under the output row: the old panel may have moved there.
        for r in (out_row + 2)..=h {
            s.push_str(&format!("\x1b[{r};1H\x1b[2K"));
        }
        s.push_str(&self.panel_text(state, status));
        s.push_str(&format!("\x1b[{};1H", out_row + 1));
        self.console.raw(&s);
    }

    /// `Teardown`: full-screen scrolling again, the panel rows cleared.
    pub fn teardown(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let mut s = String::from("\x1b[r");
        for r in (self.height.saturating_sub(4))..=self.height {
            s.push_str(&format!("\x1b[{r};1H\x1b[2K"));
        }
        s.push_str(&format!("\x1b[{};1H", self.region_bottom()));
        self.console.raw(&s);
    }

    /// `WriteInfoLine`: text on the pinned info line (cursor kept).
    pub fn write_info_line(&self, ansi: &str) {
        if !self.active {
            return;
        }
        let row = self.height.saturating_sub(4);
        self.console
            .raw(&format!("\x1b7\x1b[{row};1H\x1b[2K{ansi}\x1b8"));
    }

    /// Writes to the info line from any thread (the bridge's progress bars); `None` when the
    /// panel is off.
    pub fn info_line_writer(&self) -> Option<InfoLineWriter> {
        if !self.active {
            return None;
        }
        let console = self.console.clone();
        let row = self.height.saturating_sub(4);
        Some(std::sync::Arc::new(move |ansi: &str| {
            console.raw(&format!("7[{row};1H[2K{ansi}8"));
        }))
    }

    pub fn clear_info_line(&self) {
        self.write_info_line("");
    }

    fn panel_text(&self, state: &ReplState, status: &StatusInfo) -> String {
        let h = self.height;
        let rule = "─".repeat(self.width);
        format!(
            "\x1b[{};1H\x1b[2K\x1b[{};1H\x1b[2K\x1b[90m{rule}\x1b[0m\x1b[{};1H\x1b[2K\x1b[{};1H\x1b[2K\x1b[90m{rule}\x1b[0m\x1b[{h};1H\x1b[2K{}",
            h - 4,
            h - 3,
            h - 2,
            h - 1,
            build_status_line(state, Some(status), self.width)
        )
    }
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
fn fit_width(ansi: &str, max: usize) -> String {
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
