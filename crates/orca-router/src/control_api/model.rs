//! The admin API's wire shapes (camelCase JSON), consumed by the Abhoth web UI's admin pages
//! and the supervisor dashboard. The control plane fills them from its own types; nothing here
//! carries a secret value (keys are references or absent).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `GET /admin/api/state`: everything the Overview shows.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StateView {
    pub rev: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_error: Option<String>,
    pub mode: String,
    pub servers: Vec<ServerView>,
    pub guards: Vec<GuardView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<JobSummary>,
    pub layouts: Vec<LayoutView>,
    /// Unix milliseconds.
    pub updated: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServerView {
    pub id: String,
    pub title: String,
    /// "ok" | "degraded" (SSH down) | "unknown".
    pub reachability: String,
    pub exec: String,
    pub active_layout: Option<String>,
    /// The active layout's definition differs from what runs (4.9).
    pub drift: bool,
    pub hostctl_ok: bool,
    pub cards: Vec<CardView>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CardView {
    pub id: String,
    pub server: String,
    pub gpu_uuid: String,
    pub label: String,
    pub managed: bool,
    /// "online" | "offline" | "excluded" | "missing" | "discovered".
    pub admin_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admin_note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<EngineView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<Telemetry>,
    pub lanes: u32,
    pub inflight: u32,
    pub queued: u32,
    pub leases: u32,
    pub on_demand: bool,
    /// Processes on this card that no layout owns.
    pub foreign: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EngineView {
    pub engine: String,
    pub profile: String,
    pub model: String,
    pub served_id: String,
    /// The lifecycle state (5.1): off, preparing, starting, warming, ready, draining, stopping,
    /// failed, unknown.
    pub state: String,
    /// Unix milliseconds the state was entered.
    pub since: i64,
    pub healthy: bool,
}

/// One card's `nvidia-smi` sample (6.5).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Telemetry {
    pub name: String,
    pub mem_used_mib: u64,
    pub mem_total_mib: u64,
    pub util_pct: u32,
    pub temp_c: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_temp_c: Option<u32>,
    pub power_w: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power_limit_w: Option<f64>,
    pub sm_mhz: u32,
    pub sm_max_mhz: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_clock_mhz: Option<u32>,
    /// Active clock-event reasons ("idle", "app_clocks", "sw_power_cap", ...).
    pub clock_reasons: Vec<String>,
    /// The clock check (6.6): "ok" | "warn: …".
    pub clock_check: String,
    pub age_ms: u64,
    pub stale: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GuardView {
    pub id: String,
    pub kind: String,
    pub active: bool,
    pub stale: bool,
    pub blocks: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub retry_seconds: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LayoutView {
    pub id: String,
    pub title: String,
    pub server: String,
    pub mode: String,
    /// card -> "engine / profile / model", "off" or "same".
    pub cards: BTreeMap<String, String>,
    pub active: bool,
    /// "config" | "user" | "composed" | "on_demand".
    pub source: String,
}

/// `GET /admin/api/plan?server=&layout=` (5.3, 11.4).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub server: String,
    pub from: Option<String>,
    pub to: String,
    pub cards: Vec<PlanCard>,
    pub steps: Vec<PlanStep>,
    pub preconditions: Vec<Check>,
    pub blockers: Vec<Blocker>,
    pub restores: Vec<Restore>,
    pub drains: Vec<Drain>,
    pub routes: Vec<RouteChange>,
    pub slots_before: Vec<String>,
    pub slots_after: Vec<String>,
    pub estimate_seconds: u64,
    /// No blocker and every precondition passes.
    pub runnable: bool,
    /// "already live" etc.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlanCard {
    pub card: String,
    pub before: String,
    pub after: String,
    /// keep | stop | start | restart
    pub action: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    pub n: u32,
    pub title: String,
    pub detail: String,
    pub estimate_seconds: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub name: String,
    /// pass | fail | unknown
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Blocker {
    /// guard | lease | job | offline
    pub kind: String,
    pub id: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    pub retry_seconds: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Restore {
    pub file: String,
    pub bytes: u64,
    pub seconds: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Drain {
    pub card: String,
    pub inflight: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RouteChange {
    pub route: String,
    pub before: Vec<String>,
    pub after: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct JobSummary {
    pub id: String,
    pub server: String,
    pub kind: String,
    pub target: String,
    pub state: String,
    pub step: u32,
    pub steps: u32,
    pub title: String,
    pub started: i64,
    pub estimate_seconds: u64,
}

/// `GET /admin/api/jobs/{id}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: String,
    pub server: String,
    /// switch | offline | online | restart | reconcile
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub target: String,
    pub requested_by: String,
    /// queued | running | done | refused | rolled-back | failed | interrupted
    pub state: String,
    pub started: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub steps: Vec<JobStep>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct JobStep {
    pub n: u32,
    pub title: String,
    /// pending | running | ok | failed | skipped
    pub state: String,
    pub seconds: f64,
    /// Captured output (scrubbed of secrets), capped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AuditRow {
    pub id: i64,
    pub time: i64,
    /// A principal label (never a key).
    pub who: String,
    pub action: String,
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LeaseView {
    pub key: String,
    pub slot: String,
    pub card: String,
    /// main | agent | guard
    pub kind: String,
    pub holder: String,
    pub principal: String,
    pub expires_in_seconds: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LeasesView {
    pub frozen: bool,
    pub leases: Vec<LeaseView>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RouteView {
    pub name: String,
    /// Targets in order, as text (`pool:live`, `card:A`, `model:x`, `upstream:u/model`,
    /// with " (fallback)").
    pub targets: Vec<String>,
    /// reject | fallback | on_demand
    pub cold: String,
    pub fallback: Vec<String>,
    pub access: String,
    pub adapters: Vec<String>,
    pub hidden: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub same_as: Option<String>,
    /// What would serve it now ("card A (ninfer)", "cold").
    pub serving_now: Vec<String>,
    pub allow_user_switch: bool,
}

/// `PATCH /admin/api/routes/{name}`: only what the Routes page edits.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RouteEdit {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cold: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_user_switch: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigDoc {
    pub rev: String,
    pub path: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One validation message (4.8), placed by TOML path and line.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Problem {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub message: String,
    /// error | warning
    pub severity: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CheckResult {
    pub ok: bool,
    pub problems: Vec<Problem>,
    /// Unified diff against the current file.
    pub diff: String,
    /// Routes affected, "active layout changed: apply" etc.
    pub consequences: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<Plan>,
    /// The rev the text would have.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
}
