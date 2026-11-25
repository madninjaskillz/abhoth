//! Ported from `AgentPanelLayoutTests.cs` (layout) and `TerminalPanelTests` (row budget).

use super::*;
use crate::ui::console::strip_ansi;

fn agent(n: i32, status: AgentStatus) -> AgentRow {
    AgentRow {
        number: n,
        agent_type: "explore".into(),
        status,
        origin: AgentOrigin::Model,
        activity: Some("read_file src/a.rs".into()),
        reported_progress: Some(0.37),
        iteration: 3,
        max_iterations: 15,
        elapsed: Duration::from_secs(42),
        completed_at: None,
        error: None,
        pending_messages: 0,
        id: format!("id{n}"),
        label: format!("Scan part {n}"),
        color_slot: n - 1,
        slot: None,
        model: None,
    }
}

fn lead() -> LeadRow {
    LeadRow {
        progress: Some(0.5),
        ..Default::default()
    }
}

#[test]
fn row_budget_follows_the_terminal_height() {
    assert_eq!(max_agent_rows_for(13), 0);
    assert_eq!(max_agent_rows_for(14), 1);
    assert_eq!(max_agent_rows_for(19), 1);
    assert_eq!(max_agent_rows_for(24), 4);
    assert_eq!(max_agent_rows_for(60), 6);
}

#[test]
fn rows_never_reach_the_last_column() {
    for width in [20, 40, 80, 120] {
        let rows = render(
            Some(&lead()),
            &[agent(1, AgentStatus::Running)],
            width,
            6,
            &[],
        );
        assert_eq!(rows.len(), 2);
        for r in rows {
            assert!(
                crate::ui::console::cell_width(&strip_ansi(&r)) < width,
                "{width}"
            );
        }
    }
}

#[test]
fn lead_bar_comes_first_then_agents_with_their_progress() {
    let rows = render(
        Some(&lead()),
        &[
            agent(1, AgentStatus::Running),
            agent(2, AgentStatus::Completed),
        ],
        100,
        6,
        &[],
    );
    let plain: Vec<String> = rows.iter().map(|r| strip_ansi(r)).collect();
    assert!(plain[0].contains(" L ") && plain[0].contains("Lead") && plain[0].contains("50%"));
    assert!(plain[1].contains(" A ") && plain[1].contains("Scan part 1"));
    assert!(plain[1].contains("0:42  37%"), "{}", plain[1]);
    assert!(plain[1].contains("\u{b7} read_file src/a.rs"));
    assert!(plain[2].contains("\u{2713} done") && plain[2].contains("3 it"));
}

#[test]
fn a_short_terminal_gets_one_summary_row() {
    let rows = render(
        Some(&lead()),
        &[
            agent(1, AgentStatus::Running),
            agent(2, AgentStatus::Failed),
        ],
        100,
        1,
        &[],
    );
    assert_eq!(rows.len(), 1);
    let plain = strip_ansi(&rows[0]);
    assert!(plain.contains("Lead 50%") && plain.contains("37%") && plain.contains('\u{2717}'));
}

#[test]
fn too_many_agents_overflow_into_a_more_row() {
    let agents: Vec<AgentRow> = (1..=5).map(|n| agent(n, AgentStatus::Running)).collect();
    let rows = render(Some(&lead()), &agents, 100, 4, &[]);
    assert_eq!(rows.len(), 4);
    let last = strip_ansi(&rows[3]);
    assert!(last.contains("+3 more (C D E)"), "{last}");
}

#[test]
fn bridge_bars_sit_under_the_lead() {
    let bar = ProgressFileBar {
        key: "build".into(),
        label: "Build the app".into(),
        pct: 37.0,
        colour: "purple".into(),
        started: 1,
        updated: 2,
        source: None,
    };
    let engine = LeadRow {
        slot: Some("claude-code".into()),
        ..lead()
    };
    let rows = render(
        Some(&engine),
        &[agent(1, AgentStatus::Running)],
        100,
        6,
        &[bar],
    );
    let plain: Vec<String> = rows.iter().map(|r| strip_ansi(r)).collect();
    assert_eq!(plain.len(), 3);
    assert!(plain[0].contains("claude-code \u{b7} 50%"), "{}", plain[0]);
    assert!(
        plain[1].contains(" D ") && plain[1].contains("Build the app") && plain[1].contains("37%")
    );
    assert!(plain[2].contains("Scan part 1"));
}

#[test]
fn lead_mode_shows_before_the_note() {
    let l = LeadRow {
        mode: Some("delegate".into()),
        note: Some("splitting the work".into()),
        ..lead()
    };
    let plain = strip_ansi(&lead_row(&l, 100, false));
    assert!(
        plain.contains("orchestrating \u{b7} splitting the work"),
        "{plain}"
    );
}

#[test]
fn models_shorten_before_the_label_does() {
    assert_eq!(short_model("qwen3.8-flash-next"), "flash-next");
    assert_eq!(short_model("gpt-oss"), "gpt-oss");
    assert_eq!(
        fit_where(Some("A2"), Some("qwen3.8-flash-next"), "40%", 100),
        "A2 \u{b7} qwen3.8-flash-next \u{b7} 40%"
    );
    assert_eq!(
        fit_where(Some("A2"), Some("qwen3.8-flash-next"), "40%", 23),
        "A2 \u{b7} flash-next \u{b7} 40%"
    );
    assert_eq!(
        fit_where(Some("A2"), Some("qwen3.8-flash-next"), "40%", 8),
        "A2 \u{b7} 40%"
    );
}

#[test]
fn finished_agents_linger_briefly() {
    let now = SystemTime::now();
    let mut done = agent(1, AgentStatus::Completed);
    done.completed_at = Some(now - Duration::from_secs(1));
    let mut old = agent(2, AgentStatus::Completed);
    old.completed_at = Some(now - Duration::from_secs(60));
    let shown = visible(vec![old, done, agent(3, AgentStatus::Running)], now);
    assert_eq!(shown.iter().map(|a| a.number).collect::<Vec<_>>(), [1, 3]);
}
