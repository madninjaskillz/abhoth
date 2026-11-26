//! What the HA publisher needs from the telemetry hub, in one plain struct.
//!
//! This is the HA module's *view*; the hub (orca-supervisor) fills it from its own snapshot
//! through an adapter, so this crate depends on nothing else. Missing readings are `None`
//! (published as JSON `null`, which HA shows as "unknown"), never 0 (D-SV4).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HaSnapshot {
    pub server: ServerInfo,
    #[serde(default)]
    pub cards: Vec<CardState>,
    #[serde(default)]
    pub fans: Vec<FanState>,
    #[serde(default)]
    pub engines: Vec<EngineState>,
    #[serde(default)]
    pub host: HostState,
    #[serde(default)]
    pub control: ControlState,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerInfo {
    /// Slug used in topics and unique ids (`deepthought`).
    pub id: String,
    /// Device name in HA (`DeepThought`).
    pub name: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub manufacturer: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub config_rev: Option<String>,
    /// hostd: `ok`, `stale`, `down`, `absent`.
    #[serde(default)]
    pub hostd: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CardState {
    /// Letter, `A`..
    pub id: String,
    /// `V100-SXM3-32GB`.
    #[serde(default)]
    pub gpu: Option<String>,
    /// False when the card is missing (card availability goes offline).
    #[serde(default = "yes")]
    pub present: bool,
    /// Online for serving (the card online/offline control).
    #[serde(default)]
    pub online: bool,
    /// Clocks pinned by policy (card A): no clock-cap control.
    #[serde(default)]
    pub pinned: bool,
    /// The card has its own fan and no HBM sensor etc. are just `None` readings.
    #[serde(default)]
    pub core_c: Option<f64>,
    #[serde(default)]
    pub hbm_c: Option<f64>,
    #[serde(default)]
    pub power_w: Option<f64>,
    #[serde(default)]
    pub power_limit_w: Option<f64>,
    #[serde(default)]
    pub sm_clock_mhz: Option<f64>,
    #[serde(default)]
    pub util_pct: Option<f64>,
    #[serde(default)]
    pub vram_used_gb: Option<f64>,
    #[serde(default)]
    pub vram_total_gb: Option<f64>,
    /// Clock locked by policy right now (pin or idle clock-down).
    #[serde(default)]
    pub clock_locked: Option<bool>,
    /// Throttle reasons; empty = not throttling.
    #[serde(default)]
    pub throttle_reasons: Vec<String>,
    /// Active HA/admin clock cap in MHz, `None` = no cap.
    #[serde(default)]
    pub clock_cap_mhz: Option<u32>,
    /// The engine serving on this card.
    #[serde(default)]
    pub engine: Option<String>,
    /// `off`, `starting`, `ready`, `failed`, ...
    #[serde(default)]
    pub engine_state: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Human summary as ha-bridge's "LLM Card X": `Model (engine, N slots)` or `off`.
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub tok_s: Option<f64>,
    #[serde(default)]
    pub running: Option<u32>,
    #[serde(default)]
    pub queued: Option<u32>,
}

fn yes() -> bool {
    true
}

impl CardState {
    pub fn power_pct(&self) -> Option<f64> {
        match (self.power_w, self.power_limit_w) {
            (Some(p), Some(l)) if l > 0.0 => Some(round1(p / l * 100.0)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FanState {
    /// `0`, `1`, ... (the ESP fan index) or a name.
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Cards this fan cools.
    #[serde(default)]
    pub cools: Vec<String>,
    #[serde(default)]
    pub duty_pct: Option<f64>,
    #[serde(default)]
    pub rpm: Option<f64>,
    /// The control temperature hostd pushes.
    #[serde(default)]
    pub control_temp_c: Option<f64>,
    /// Which sensor won (`gpu.B.hbm`, or a fallback).
    #[serde(default)]
    pub source: Option<String>,
    /// Seconds since the last push to the device.
    #[serde(default)]
    pub push_age_s: Option<f64>,
    #[serde(default)]
    pub failsafe: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EngineState {
    /// Engine id from config (`strata-a`, `comfyui`, `sa3`).
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    /// `llm`, `image`, `audio`, ...
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub cards: Vec<String>,
    /// `off`, `starting`, `ready`, `idle`, `failed`.
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub tok_s: Option<f64>,
    #[serde(default)]
    pub healthy: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostState {
    #[serde(default)]
    pub cpu_pct: Option<f64>,
    #[serde(default)]
    pub ram_total_gb: Option<f64>,
    /// Used including ARC.
    #[serde(default)]
    pub ram_used_gb: Option<f64>,
    #[serde(default)]
    pub ram_arc_gb: Option<f64>,
    /// Used excluding ARC ("consumed").
    #[serde(default)]
    pub ram_consumed_gb: Option<f64>,
    #[serde(default)]
    pub ram_free_gb: Option<f64>,
    #[serde(default)]
    pub net_in_mbps: Option<f64>,
    #[serde(default)]
    pub net_out_mbps: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ControlState {
    /// Live layout id.
    #[serde(default)]
    pub layout: Option<String>,
    #[serde(default)]
    pub layout_title: Option<String>,
    /// Layouts that can be applied (the select's options).
    #[serde(default)]
    pub layouts: Vec<String>,
    /// `idle | queued | switching | failed | refused`.
    #[serde(default)]
    pub switch_state: Option<String>,
    #[serde(default)]
    pub last_switch: Option<serde_json::Value>,
    /// `off | on | idle | running`.
    #[serde(default)]
    pub on_demand: Option<String>,
    #[serde(default)]
    pub leases_held: Option<u32>,
    #[serde(default)]
    pub slots_busy: Option<u32>,
    #[serde(default)]
    pub slots_total: Option<u32>,
    #[serde(default)]
    pub bench_running: Option<bool>,
    #[serde(default)]
    pub calendar_block: Option<bool>,
    /// The active calendar event, if any.
    #[serde(default)]
    pub event_active: Option<String>,
    /// Any open alert.
    #[serde(default)]
    pub problem: Option<bool>,
    /// Open alert identities (attribute of `problem`).
    #[serde(default)]
    pub open_alerts: Vec<String>,
    /// `auto | quiet | full`.
    #[serde(default)]
    pub fan_mode: Option<String>,
}

impl HaSnapshot {
    pub fn total_tok_s(&self) -> Option<f64> {
        let v: Vec<f64> = self.cards.iter().filter_map(|c| c.tok_s).collect();
        (!v.is_empty()).then(|| round1(v.iter().sum()))
    }

    pub fn total_power_w(&self) -> Option<f64> {
        let v: Vec<f64> = self.cards.iter().filter_map(|c| c.power_w).collect();
        (!v.is_empty()).then(|| round1(v.iter().sum()))
    }

    /// ha-bridge's "Model" string: `A: ... | B: ...` over every card.
    pub fn models_summary(&self) -> String {
        let s = self
            .cards
            .iter()
            .map(|c| format!("{}: {}", c.id, c.summary.as_deref().or(c.model.as_deref()).unwrap_or("off")))
            .collect::<Vec<_>>()
            .join(" | ");
        s.chars().take(250).collect()
    }

    pub fn card(&self, id: &str) -> Option<&CardState> {
        self.cards.iter().find(|c| c.id.eq_ignore_ascii_case(id))
    }
}

pub(crate) fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}
