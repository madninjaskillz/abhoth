//! Applies the configured sampler and thinking parameters to a request (`RequestParameters`).
//! Unset config sends nothing, so the server's defaults stand.

use indexmap::IndexMap;
use serde_json::{Value, json};

use super::f32_json;
use super::types::ChatOptions;
use crate::config::{OrcaConfig, ThinkingConfig, sampling_presets, thinking_levels};

/// Fill sampling and thinking parameters for `model`. With `keep_temperature`, a temperature
/// already on `options` is kept (the cold post-runaway recovery attempt).
pub fn apply(
    options: &mut ChatOptions,
    config: &OrcaConfig,
    model: Option<&str>,
    keep_temperature: bool,
) {
    apply_sampling(options, config, model, keep_temperature);
    apply_thinking(options, &config.thinking);
}

pub fn apply_sampling(
    options: &mut ChatOptions,
    config: &OrcaConfig,
    model: Option<&str>,
    keep_temperature: bool,
) {
    let sampling = sampling_presets::resolve(&config.lm_studio, model);
    if !keep_temperature {
        options.temperature = sampling
            .temperature
            .or(options.temperature)
            .or(Some(config.lm_studio.temperature));
    }
    if let Some(v) = sampling.top_p {
        options.top_p = Some(v);
    }
    if let Some(v) = sampling.top_k {
        options.top_k = Some(v);
    }
    if let Some(v) = sampling.presence_penalty {
        options.presence_penalty = Some(v);
    }
    if let Some(v) = sampling.min_p {
        options.set("min_p", f32_json(v));
    }
    if let Some(v) = sampling.repetition_penalty {
        options.set("repeat_penalty", f32_json(v));
        options.set("repetition_penalty", f32_json(v));
    }
}

pub fn apply_thinking(options: &mut ChatOptions, thinking: &ThinkingConfig) {
    if let Some(effort) = thinking
        .reasoning_effort
        .as_deref()
        .filter(|e| !e.is_empty())
    {
        options.set("reasoning_effort", json!(effort));
    }
    let Some(level) = thinking_levels::resolve(thinking) else {
        return;
    };
    // llama.cpp: the chat template reads enable_thinking; llama-server ends the reasoning block
    // once thinking_budget_tokens is spent.
    options.set(
        "chat_template_kwargs",
        json!({"enable_thinking": level.enabled}),
    );
    if let Some(budget) = level.budget_tokens {
        options.set("thinking_budget_tokens", json!(budget));
    }
}

/// `reasoning_effort` for one actor (agent.reasoning.lead / .worker). Blank leaves the request
/// alone; "off" removes the effort and switches thinking off.
pub fn apply_effort(options: &mut ChatOptions, effort: Option<&str>) {
    let Some(effort) = effort.map(str::trim).filter(|e| !e.is_empty()) else {
        return;
    };
    if !effort.eq_ignore_ascii_case("off") {
        options.set("reasoning_effort", json!(effort));
        return;
    }
    options.additional.shift_remove("reasoning_effort");
    thinking_off(options);
}

/// One request without thinking (adaptive reasoning); keeps `reasoning_effort`.
pub fn skip_thinking(options: &mut ChatOptions) {
    thinking_off(options);
}

fn thinking_off(options: &mut ChatOptions) {
    let mut kwargs: IndexMap<String, Value> = match options.additional.get("chat_template_kwargs") {
        Some(Value::Object(o)) => o.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        _ => IndexMap::new(),
    };
    kwargs.insert("enable_thinking".into(), Value::Bool(false));
    options.set("chat_template_kwargs", thinking_switch_first(&kwargs));
}

/// The same `chat_template_kwargs` with `enable_thinking` as the FIRST key. Strata applies the
/// keys in order and `enable_thinking = false` resets the ones before it, so a
/// `preserve_thinking` in front of it would be lost (a prompt-cache miss on every switch).
pub fn thinking_switch_first(kwargs: &IndexMap<String, Value>) -> Value {
    let mut ordered = serde_json::Map::new();
    if let Some(v) = kwargs.get("enable_thinking") {
        ordered.insert("enable_thinking".into(), v.clone());
    }
    for (k, v) in kwargs {
        if !ordered.contains_key(k) {
            ordered.insert(k.clone(), v.clone());
        }
    }
    Value::Object(ordered)
}
