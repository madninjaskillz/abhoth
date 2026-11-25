//! `BenchResultRow`: one line of `results.jsonl` (the bench folder's results file), one run of
//! one task on one arm. The runner writes these keys (snake_case, nulls left out or null);
//! `GET /api/bench/results` returns the rows as written. Old rows lack newer fields, so every
//! field but the identity ones is optional.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BenchResultRow {
    // Identity: what ran.
    /// Run id, unique per run.
    #[serde(deserialize_with = "null_as_empty")]
    pub id: String,
    #[serde(deserialize_with = "null_as_empty")]
    pub suite: String,
    #[serde(deserialize_with = "null_as_empty")]
    pub arm: String,
    #[serde(deserialize_with = "null_as_empty")]
    pub lead_mode: String,
    #[serde(deserialize_with = "null_as_empty")]
    pub sets: Vec<String>,
    #[serde(deserialize_with = "null_as_empty")]
    pub task: String,
    pub rep: i32,
    pub profile: Option<String>,
    pub layout: Option<String>,
    pub settings: Option<Value>,
    pub batch: Option<String>,
    pub target: Option<String>,
    pub target_endpoint: Option<String>,
    pub model: Option<String>,

    // Outcome.
    /// "passed", "failed", "timeout", "error" or "stopped".
    #[serde(deserialize_with = "null_as_empty")]
    pub status: String,
    pub pass: Option<bool>,
    pub reason: Option<String>,
    pub tests_passed: Option<i32>,
    pub tests_total: Option<i32>,
    pub timeout: Option<bool>,
    pub quiet_window_stop: Option<bool>,
    pub note: Option<String>,

    // Time.
    pub started: Option<String>,
    pub ended: Option<String>,
    #[serde(rename = "wall_s")]
    pub wall_seconds: Option<f64>,

    // Tokens and speed.
    pub requests: Option<i64>,
    pub prompt_tokens: Option<i64>,
    pub cached_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub content_tokens: Option<i64>,
    pub tool_tokens: Option<i64>,
    pub decode_tps: Option<f64>,
    pub lead_tps: Option<f64>,
    pub helpers: Option<i32>,
    pub helper_routes: Option<Vec<String>>,
    pub slots: Option<Vec<String>>,
    pub copy_tps: Option<BTreeMap<String, f64>>,
    pub bridge_turns: Option<i32>,
    pub cost_usd: Option<f64>,

    // Environment.
    #[serde(rename = "mean_w")]
    pub mean_watts: Option<f64>,
    pub orca_version: Option<String>,

    /// Added on read: the archive-* folder an older row was moved to.
    pub archive: Option<String>,
}

/// The JSON keys declared above.
pub const KEYS: &[&str] = &[
    "id",
    "suite",
    "arm",
    "lead_mode",
    "sets",
    "task",
    "rep",
    "profile",
    "layout",
    "settings",
    "batch",
    "target",
    "target_endpoint",
    "model",
    "status",
    "pass",
    "reason",
    "tests_passed",
    "tests_total",
    "timeout",
    "quiet_window_stop",
    "note",
    "started",
    "ended",
    "wall_s",
    "requests",
    "prompt_tokens",
    "cached_tokens",
    "output_tokens",
    "reasoning_tokens",
    "content_tokens",
    "tool_tokens",
    "decode_tps",
    "lead_tps",
    "helpers",
    "helper_routes",
    "slots",
    "copy_tps",
    "bridge_turns",
    "cost_usd",
    "mean_w",
    "orca_version",
    "archive",
];

fn null_as_empty<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

impl BenchResultRow {
    /// A results.jsonl line as a typed row; `None` when it isn't one (not an object, wrong types).
    /// A null where the identity fields expect text reads as empty, as C#'s deserializer does
    /// for a missing one.
    pub fn parse(row: &Value) -> Option<Self> {
        if !row.is_object() {
            return None;
        }
        serde_json::from_value(row.clone()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_match_the_struct() {
        let v = serde_json::to_value(BenchResultRow::default()).unwrap();
        let names: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(names, KEYS);
    }
}
