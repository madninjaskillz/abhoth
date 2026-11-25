//! The footer's progress bars (`AgentPanelLayout` + `AgentPanelRenderer`): the Lead's grey
//! "L" bar, the bars a bridged CLI drew with progress.js right under it, then one bar per
//! visible agent — `[A] ▓▓ Scan auth code ▓▓▓░░░ 0:42 37% ░`. A light timer (elapsed time)
//! plus an immediate refresh on agent events keeps them current; only changed rows repaint.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use orca_core::orchestration::palette;
use orca_core::orchestration::types::{AgentOrigin, AgentStatus};
use orca_core::orchestration::{AgentContext, AgentEvent, AgentOrchestrator, activity, labels};
use orca_core::progress::ProgressFileBar;
use orca_core::slots::SlotRuntime;

use super::panel::{TerminalPanel, fit_width};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const GRAY: &str = "\x1b[90m";
const TEXT_ON_EMPTY_BG: i32 = 236;
const EMPTY_BAR: i32 = 238;
const TICK: Duration = Duration::from_millis(250);

/// `AgentRowModel`: what one agent's bar shows.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRow {
    pub number: i32,
    pub agent_type: String,
    pub status: AgentStatus,
    pub origin: AgentOrigin,
    pub activity: Option<String>,
    pub reported_progress: Option<f64>,
    pub iteration: i32,
    pub max_iterations: i32,
    pub elapsed: Duration,
    pub completed_at: Option<SystemTime>,
    pub error: Option<String>,
    pub pending_messages: usize,
    pub id: String,
    pub label: String,
    pub color_slot: i32,
    pub slot: Option<String>,
    pub model: Option<String>,
}

impl AgentRow {
    pub fn is_active(&self) -> bool {
        matches!(self.status, AgentStatus::Pending | AgentStatus::Running)
    }

    pub fn letter(&self) -> char {
        palette::letter_of(self.color_slot)
    }

    /// `AgentRowModel.From`.
    pub fn from(
        c: &AgentContext,
        now: SystemTime,
        slot_model: (Option<String>, Option<String>),
    ) -> Self {
        let st = c.state();
        let elapsed = st
            .started_at
            .and_then(|s| st.completed_at.unwrap_or(now).duration_since(s).ok())
            .unwrap_or_default();
        let activity = match st.current_activity.as_deref() {
            None | Some("thinking") | Some("writing")
                if st
                    .progress_note
                    .as_deref()
                    .is_some_and(|n| !n.trim().is_empty()) =>
            {
                st.progress_note.clone()
            }
            _ => st.current_activity.clone(),
        };
        let row = Self {
            number: c.number,
            agent_type: c.agent_type.clone(),
            status: st.status,
            origin: c.origin,
            activity,
            reported_progress: st.reported_progress,
            iteration: st.iteration_count,
            max_iterations: st.max_iterations,
            elapsed,
            completed_at: st.completed_at,
            error: st.error.clone(),
            pending_messages: 0,
            id: c.id.clone(),
            label: st.label.clone(),
            color_slot: c.color_slot,
            slot: slot_model.0,
            model: slot_model.1,
        };
        drop(st);
        Self {
            pending_messages: c.pending_message_count(),
            ..row
        }
    }
}

/// `LeadRowModel`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LeadRow {
    pub progress: Option<f64>,
    pub note: Option<String>,
    pub hint: Option<String>,
    pub slot: Option<String>,
    pub mode: Option<String>,
    pub model: Option<String>,
}

/// `TerminalPanel.MaxAgentRowsFor`: none on tiny terminals, a summary row on short ones, up to
/// six when there is room.
pub fn max_agent_rows_for(height: usize) -> usize {
    if height < 14 {
        0
    } else if height < 20 {
        1
    } else {
        ((height - 12) / 3).min(6)
    }
}

/// `AgentPanelLayout.Visible`: active agents plus recently finished ones, by number.
pub fn visible(mut agents: Vec<AgentRow>, now: SystemTime) -> Vec<AgentRow> {
    agents.retain(|a| {
        a.is_active()
            || a.completed_at.is_some_and(|done| {
                now.duration_since(done).unwrap_or_default() < palette::hold_after_finish(a.origin)
            })
    });
    agents.sort_by_key(|a| a.number);
    agents
}

/// `ProgressText`: "40%" when reported, else iterations ("3/15"), else "…".
pub fn progress_text(a: &AgentRow) -> String {
    match a.reported_progress {
        Some(p) => format!("{}%", (p * 100.0).round() as i32),
        None if a.max_iterations > 0 => format!("{}/{}", a.iteration, a.max_iterations),
        None => "\u{2026}".into(),
    }
}

/// `FormatElapsed`: m:ss, or h:mm:ss from an hour.
pub fn format_elapsed(t: Duration) -> String {
    let s = t.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// "orchestrating" / "hands-on" / "answering" for a lead_mode choice.
pub fn mode_label(mode: Option<&str>) -> Option<&'static str> {
    match mode? {
        "delegate" => Some("orchestrating"),
        "hands-on" => Some("hands-on"),
        "answer" => Some("answering"),
        _ => None,
    }
}

fn one_line(s: &str) -> String {
    s.replace(['\r', '\n'], " ").trim().to_string()
}

/// Bar text is laid out per cell: control characters become spaces, wide ones are dropped.
fn single_width(s: &str) -> String {
    s.chars()
        .filter_map(|c| {
            if c.is_control() {
                Some(' ')
            } else if crate::ui::console::cell_width(c.encode_utf8(&mut [0; 4])) == 1 {
                Some(c)
            } else {
                None
            }
        })
        .collect()
}

fn badge_ansi(xterm: i32, letter: char) -> String {
    format!("\x1b[1m\x1b[38;5;16m\x1b[48;5;{xterm}m {letter} {RESET}")
}

fn agent_badge(slot: i32) -> String {
    badge_ansi(palette::xterm_of(slot), palette::letter_of(slot))
}

fn lead_badge() -> String {
    badge_ansi(palette::LEAD_XTERM, palette::LEAD_LETTER)
}

fn slot_tag(slot: Option<&str>) -> String {
    slot.filter(|s| !s.is_empty())
        .map(|s| format!("{s} \u{b7} "))
        .unwrap_or_default()
}

fn where_tag(slot: Option<&str>, model: Option<&str>) -> String {
    format!(
        "{}{}",
        slot_tag(slot),
        model
            .filter(|m| !m.is_empty())
            .map(|m| format!("{m} \u{b7} "))
            .unwrap_or_default()
    )
}

/// `ShortModel`: "qwen3.8-flash-next" → "flash-next"; other names unchanged.
pub fn short_model(model: &str) -> String {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^[A-Za-z]+[0-9][0-9.]*[A-Za-z]?-(.+)$").unwrap()
    });
    RE.captures(model)
        .map_or_else(|| model.to_string(), |c| c[1].to_string())
}

fn clen(s: &str) -> usize {
    s.chars().count()
}

/// `FitWhere`: as much of slot and model before `tail` as fits in `room` cells.
pub fn fit_where(slot: Option<&str>, model: Option<&str>, tail: &str, room: i64) -> String {
    if let Some(model) = model.filter(|m| !m.is_empty()) {
        let full = where_tag(slot, Some(model)) + tail;
        if clen(&full) as i64 <= room {
            return full;
        }
        let short = short_model(model);
        let shorter = where_tag(slot, Some(&short)) + tail;
        if clen(&shorter) as i64 <= room {
            return shorter;
        }
        let cut = room - clen(&(where_tag(slot, Some("")) + tail)) as i64 - 3;
        if cut >= 4 {
            return where_tag(slot, Some(&activity::truncate(&short, cut as usize))) + tail;
        }
    }
    slot_tag(slot) + tail
}

/// `Bar`: gutter, badge, then the bar with the bold label and the softer activity on its left
/// and `right` on its right; text is dark over the fill and coloured over the empty part.
#[allow(clippy::too_many_arguments)]
pub fn bar(
    badge: &str,
    colour: i32,
    selected: bool,
    label: &str,
    soft: Option<&str>,
    right: &str,
    fraction: f64,
    width: usize,
    slot: Option<&str>,
    model: Option<&str>,
    prefix: &str,
) -> String {
    let cols = width.saturating_sub(1);
    let gutter = if selected {
        format!("{BOLD}\x1b[97m\u{276f}{RESET}")
    } else {
        " ".into()
    };
    let bar_len = cols as i64 - 5;
    if bar_len < 12 {
        return fit_width(&format!("{gutter}{badge} {BOLD}{right}{RESET}"), cols);
    }
    let bar_len = bar_len as usize;
    let filled = (fraction.clamp(0.0, 1.0) * bar_len as f64).round() as usize;
    let mut cells: Vec<char> = (0..bar_len)
        .map(|i| if i < filled { '\u{2593}' } else { '\u{2591}' })
        .collect();
    let mut kind = vec![0u8; bar_len];
    let mut put = |text: &str, at: usize, k: u8| {
        for (i, c) in text.chars().enumerate() {
            if at + i >= bar_len {
                break;
            }
            cells[at + i] = c;
            kind[at + i] = k;
        }
    };

    let label_sw = single_width(label);
    let label_full = clen(&label_sw).min(labels::MAX_CHARS) as i64 + 2;
    let where_room = bar_len as i64 - label_full - 1 - 3 - 2 - clen(&single_width(prefix)) as i64;
    let slot_sw = slot.map(single_width);
    let model_sw = model.map(single_width);
    let right = format!(
        "{prefix}{}",
        fit_where(slot_sw.as_deref(), model_sw.as_deref(), right, where_room)
    );
    let right_text = format!(" {} ", single_width(&right));
    let right_at = bar_len as i64 - clen(&right_text) as i64 - 1;
    let left_room = (right_at - 2 - 1).max(0) as usize;
    let label_room = labels::MAX_CHARS.min(left_room.saturating_sub(2)).max(1);
    let label_text = format!(" {} ", activity::truncate(&label_sw, label_room));
    let label_len = clen(&label_text);
    put(&label_text, 1, 1);
    let soft_room = left_room as i64 - label_len as i64 - 2;
    if let Some(soft) = soft.filter(|s| !s.trim().is_empty())
        && soft_room >= 6
    {
        put(
            &format!(
                "\u{b7} {} ",
                activity::truncate(&single_width(soft), (soft_room - 3) as usize)
            ),
            1 + label_len,
            2,
        );
    }
    if right_at > label_len as i64 + 1 {
        put(&right_text, right_at as usize, 1);
    }

    let mut out = format!("{gutter}{badge} ");
    let mut run_style = String::new();
    let mut run = String::new();
    for i in 0..bar_len {
        let over = i < filled;
        let mut style = match (kind[i], over) {
            (0, true) => format!("\x1b[38;5;{colour}m"),
            (0, false) => format!("\x1b[38;5;{EMPTY_BAR}m"),
            (1, true) => format!("{BOLD}\x1b[38;5;16m\x1b[48;5;{colour}m"),
            (1, false) => format!("{BOLD}\x1b[38;5;{colour}m\x1b[48;5;{TEXT_ON_EMPTY_BG}m"),
            (_, true) => format!("\x1b[38;5;16m\x1b[48;5;{colour}m"),
            (_, false) => format!("\x1b[38;5;250m\x1b[48;5;{TEXT_ON_EMPTY_BG}m"),
        };
        if selected && kind[i] == 1 && i < 1 + label_len {
            style.push_str("\x1b[4m");
        }
        if style != run_style && !run.is_empty() {
            out.push_str(&run_style);
            out.push_str(&run);
            out.push_str(RESET);
            run.clear();
        }
        run_style = style;
        run.push(cells[i]);
    }
    if !run.is_empty() {
        out.push_str(&run_style);
        out.push_str(&run);
        out.push_str(RESET);
    }
    out
}

/// `LeadRow`.
pub fn lead_row(lead: &LeadRow, width: usize, selected: bool) -> String {
    let pct = (lead.progress.unwrap_or(0.0).clamp(0.0, 1.0) * 100.0).round() as i32;
    let mut soft = lead.note.clone().or_else(|| lead.hint.clone());
    if let Some(mode) = mode_label(lead.mode.as_deref()) {
        soft = Some(match soft {
            None => mode.to_string(),
            Some(s) => format!("{mode} \u{b7} {s}"),
        });
    }
    bar(
        &lead_badge(),
        palette::LEAD_XTERM,
        selected,
        palette::LEAD_NAME,
        soft.as_deref(),
        &format!("{pct}%"),
        f64::from(pct) / 100.0,
        width,
        lead.slot.as_deref(),
        lead.model.as_deref(),
        "",
    )
}

/// `ExternalRow`: a progress.js bar of a bridged CLI.
pub fn external_row(b: &ProgressFileBar, width: usize) -> String {
    let pct = b.pct.clamp(0.0, 100.0).round() as i32;
    let slot = b.color_slot();
    bar(
        &agent_badge(slot),
        palette::xterm_of(slot),
        false,
        &b.label,
        None,
        &format!("{pct}%"),
        f64::from(pct) / 100.0,
        width,
        None,
        None,
        "",
    )
}

/// `Row`: one agent's bar.
pub fn row(a: &AgentRow, width: usize, selected: bool) -> String {
    let label = if a.label.is_empty() {
        &a.agent_type
    } else {
        &a.label
    };
    let mail = if a.pending_messages > 0 {
        format!("\u{2709}{} ", a.pending_messages)
    } else {
        String::new()
    };
    let elapsed = format_elapsed(a.elapsed);
    let badge = agent_badge(a.color_slot);
    let colour = palette::xterm_of(a.color_slot);
    if a.is_active() {
        let right = format!("{elapsed}  {}", progress_text(a));
        let soft = a
            .activity
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(one_line);
        return bar(
            &badge,
            colour,
            selected,
            label,
            soft.as_deref(),
            &right,
            a.reported_progress.unwrap_or(0.0),
            width,
            a.slot.as_deref(),
            a.model.as_deref(),
            &mail,
        );
    }
    let word = match a.status {
        AgentStatus::Completed => "\u{2713} done",
        AgentStatus::CompletedPartial => "\u{26a0} partial",
        AgentStatus::Cancelled => "\u{2298} cancelled",
        _ => "\u{2717} failed",
    };
    let soft = match (&a.status, a.error.as_deref()) {
        (AgentStatus::Failed, Some(e)) if !e.trim().is_empty() => one_line(e),
        (AgentStatus::CompletedPartial, _) => "iteration limit".into(),
        _ => format!("{} it", a.iteration),
    };
    let fill = if a.status == AgentStatus::Completed {
        1.0
    } else {
        a.reported_progress.unwrap_or(0.0)
    };
    bar(
        &badge,
        colour,
        selected,
        label,
        Some(&soft),
        &format!("{elapsed}  {word}"),
        fill,
        width,
        None,
        None,
        "",
    )
}

/// `Summary`: one row for many bars, `[L] Lead 0% · [A] 40% · [B] 3/15 · [C] ✓`.
fn summary(lead: Option<&LeadRow>, visible: &[AgentRow], width: usize) -> String {
    let mut s = String::from(" ");
    if let Some(lead) = lead {
        let pct = (lead.progress.unwrap_or(0.0).clamp(0.0, 1.0) * 100.0).round() as i32;
        s.push_str(&format!("{} {BOLD}Lead{RESET} {pct}%", lead_badge()));
    }
    for a in visible {
        if s.len() > 1 {
            s.push_str(&format!(" {GRAY}\u{b7}{RESET} "));
        }
        let state = if a.is_active() {
            progress_text(a)
        } else {
            match a.status {
                AgentStatus::Completed => "\u{2713}".into(),
                AgentStatus::Cancelled => "\u{2298}".into(),
                _ => "\u{2717}".into(),
            }
        };
        s.push_str(&format!("{} {state}", agent_badge(a.color_slot)));
    }
    fit_width(&s, width.saturating_sub(1))
}

/// `AgentPanelLayout.Render`: the footer rows, each at most `width` − 1 visible columns.
pub fn render(
    lead: Option<&LeadRow>,
    visible: &[AgentRow],
    width: usize,
    max_rows: usize,
    external: &[ProgressFileBar],
) -> Vec<String> {
    let mut rows = Vec::new();
    if max_rows == 0 || width < 12 || (lead.is_none() && visible.is_empty() && external.is_empty())
    {
        return rows;
    }
    if !external.is_empty() {
        let keep = usize::from(lead.is_some()) + usize::from(!visible.is_empty());
        let take = external.len().min(max_rows.saturating_sub(keep));
        if take > 0 {
            let inner = render(lead, visible, width, max_rows - take, &[]);
            let lead_rows = if lead.is_some() {
                inner.len().min(1)
            } else {
                0
            };
            rows.extend(inner.iter().take(lead_rows).cloned());
            rows.extend(external.iter().take(take).map(|b| external_row(b, width)));
            rows.extend(inner.into_iter().skip(lead_rows));
            return rows;
        }
    }
    if lead.is_none() && visible.is_empty() {
        return rows;
    }
    let bars = usize::from(lead.is_some()) + visible.len();
    if bars > 1 && max_rows == 1 {
        rows.push(summary(lead, visible, width));
        return rows;
    }
    if let Some(lead) = lead {
        rows.push(lead_row(lead, width, false));
    }
    let room = max_rows - rows.len();
    if visible.len() <= room {
        rows.extend(visible.iter().map(|a| row(a, width, false)));
        return rows;
    }
    if room == 1 {
        rows.push(summary(None, visible, width));
        return rows;
    }
    // Too many: active ones first, then an overflow row.
    let mut order: Vec<&AgentRow> = visible.iter().collect();
    order.sort_by_key(|a| (std::cmp::Reverse(a.is_active()), a.number));
    let mut shown: Vec<&AgentRow> = order.into_iter().take(room - 1).collect();
    shown.sort_by_key(|a| a.number);
    rows.extend(shown.iter().map(|a| row(a, width, false)));
    let hidden: Vec<String> = visible
        .iter()
        .filter(|a| !shown.iter().any(|s| s.id == a.id))
        .map(|a| a.letter().to_string())
        .collect();
    rows.push(fit_width(
        &format!(
            "  {GRAY}\u{2026} +{} more ({}) \u{b7} /agents to list{RESET}",
            visible.len() - shown.len(),
            hidden.join(" ")
        ),
        width.saturating_sub(1),
    ));
    rows
}

/// `LeadWhere`: with slots, the Lead's endpoint or slot and that copy's model ("—" while it
/// holds none, "waiting" while a request runs without one); without, the provider model.
pub fn lead_where(
    slots: Option<&SlotRuntime>,
    provider_model: Option<String>,
) -> (Option<String>, Option<String>) {
    let Some(slots) = slots else {
        return (None, provider_model);
    };
    match slots.lead_location().filter(|w| !w.is_empty()) {
        Some(w) => (Some(w), slots.lead_model().or(provider_model)),
        None => (
            Some(
                if slots.lead_request_in_flight() {
                    "waiting"
                } else {
                    "\u{2014}"
                }
                .into(),
            ),
            None,
        ),
    }
}

/// `AgentWhere`.
pub fn agent_where(
    c: &AgentContext,
    slots: Option<&SlotRuntime>,
    provider_model: Option<String>,
) -> (Option<String>, Option<String>) {
    if !c.is_active() {
        return (None, None);
    }
    let st = c.state();
    if let Some(p) = st.provider.clone().filter(|p| !p.is_empty()) {
        return (Some(p), st.provider_model.clone());
    }
    let Some(slots) = slots else {
        return (None, provider_model);
    };
    if let Some(slot) = st.slot.clone().filter(|s| !s.is_empty()) {
        let model = slots.model_for_slot(Some(&slot)).or(provider_model);
        return (Some(slot), model);
    }
    let waiting = st.status == AgentStatus::Pending;
    (
        Some(if waiting { "waiting" } else { "\u{2014}" }.into()),
        None,
    )
}

/// Set while a reply's live tail is on screen: the footer must not grow then (the tail is
/// erased by moving the cursor up over its rows).
pub static LIVE_TAIL: AtomicBool = AtomicBool::new(false);

/// What the renderer reads each tick.
pub struct FooterSources {
    pub orchestrator: Option<Arc<AgentOrchestrator>>,
    pub slots: Option<Arc<SlotRuntime>>,
    /// lmStudio.models[0].
    pub provider_model: Option<String>,
    /// A bridge's name ("claude-code"), shown in place of the Lead's slot and model.
    pub engine_label: Arc<Mutex<Option<String>>>,
    /// Bars a bridged CLI drew with progress.js.
    pub external: Arc<Mutex<Vec<ProgressFileBar>>>,
}

/// `AgentPanelRenderer`: repaints the footer every 250 ms and at once on agent events.
pub struct FooterRenderer {
    wake: Arc<(Mutex<bool>, std::sync::Condvar)>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FooterRenderer {
    pub fn start(panel: TerminalPanel, sources: FooterSources) -> Self {
        let wake = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        if let Some(o) = &sources.orchestrator {
            let w = Arc::downgrade(&wake);
            o.subscribe(move |e| {
                // Structural changes repaint at once; per-tool chatter waits for the tick.
                if matches!(
                    e,
                    AgentEvent::Started { .. }
                        | AgentEvent::Completed { .. }
                        | AgentEvent::ToolCallStarted { .. }
                        | AgentEvent::ProgressReported { .. }
                ) && let Some(w) = w.upgrade()
                {
                    *w.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
                    w.1.notify_one();
                }
            });
        }
        let (w, s) = (wake.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("agent-footer".into())
            .spawn(move || {
                while !s.load(Ordering::SeqCst) {
                    tick(&panel, &sources);
                    let (lock, cv) = &*w;
                    let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                    let (mut woke, _) = cv
                        .wait_timeout_while(guard, TICK, |w| !*w)
                        .unwrap_or_else(|e| e.into_inner());
                    *woke = false;
                }
            })
            .ok();
        Self { wake, stop, thread }
    }

    /// [`Self::poke`] for other threads.
    pub fn poker(&self) -> Arc<dyn Fn() + Send + Sync> {
        let w = Arc::downgrade(&self.wake);
        Arc::new(move || {
            if let Some(w) = w.upgrade() {
                *w.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
                w.1.notify_one();
            }
        })
    }

    /// Repaint now (an engine switch, a bridge's bars changed).
    pub fn poke(&self) {
        *self.wake.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.wake.1.notify_one();
    }
}

impl Drop for FooterRenderer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.poke();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// One repaint (`AgentPanelRenderer.Tick`).
pub fn tick(panel: &TerminalPanel, s: &FooterSources) {
    if !panel.is_active() {
        return;
    }
    let now = SystemTime::now();
    let slots = s.slots.as_deref();
    let agents = s
        .orchestrator
        .as_ref()
        .map(|o| {
            o.get_all_agents()
                .iter()
                .map(|a| AgentRow::from(a, now, agent_where(a, slots, s.provider_model.clone())))
                .collect()
        })
        .unwrap_or_default();
    let shown = visible(agents, now);
    let allow_grow = !LIVE_TAIL.load(Ordering::SeqCst);
    let max_rows = if allow_grow {
        panel.max_agent_rows()
    } else {
        panel.agent_row_count()
    };
    // C# also offers "↓ or Ctrl+G: select a bar"; bar selection is not ported (D107).
    let engine = s
        .engine_label
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .filter(|e| !e.is_empty());
    let (slot, model) = match engine {
        Some(e) => (Some(e), None),
        None => lead_where(slots, s.provider_model.clone()),
    };
    let lead = LeadRow {
        progress: s.orchestrator.as_ref().and_then(|o| o.lead_progress()),
        note: s.orchestrator.as_ref().and_then(|o| o.lead_note()),
        hint: None,
        slot,
        mode: s.orchestrator.as_ref().and_then(|o| o.lead_request_mode()),
        model,
    };
    let external = s.external.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let lines = render(Some(&lead), &shown, panel.width(), max_rows, &external);
    panel.set_agent_lines(&lines, allow_grow);
}

#[cfg(test)]
#[path = "agent_footer_tests.rs"]
mod tests;
