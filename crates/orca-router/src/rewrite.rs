//! The body sent upstream for one deployment (ROUTER.md 4.2 "rewrite"): the client's body with
//! `model` = the served id, the deployment's default params, the effort rules, and the fields
//! the engine rejects removed. Everything else passes in the client's order.

use serde_json::{Map, Value};

use crate::config::{Deployment, Settings};
use crate::effort;

/// Rewrite `body` (the client's, already parsed) for `dep`.
pub fn for_deployment(
    body: &Map<String, Value>,
    dep: &Deployment,
    settings: &Settings,
) -> Map<String, Value> {
    let mut out = body.clone();
    // `extra_body` is an SDK-side idea: the OpenAI SDKs (and LiteLLM) merge it into the top level,
    // so an engine never sees the key. Its keys win over the same top-level keys, and the effort
    // rules run after the merge (LiteLLM runs them before it, which can send ninfer an effort
    // together with enable_thinking=false: ROUTER.md D-R6).
    if let Some(Value::Object(extra)) = out.shift_remove("extra_body") {
        for (k, v) in extra {
            out.insert(k, v);
        }
    }
    out.insert("model".into(), Value::String(dep.served_id.clone()));
    // LiteLLM litellm_params: defaults the client did not set (a client value wins).
    for (k, v) in &dep.params {
        if !out.contains_key(k) {
            out.insert(k.clone(), v.clone());
        }
    }
    effort::apply(
        &mut out,
        dep.effort_levels().as_deref(),
        dep.thinking_off_effort().as_deref(),
    );
    for k in dep.drop_params.iter().chain(settings.strip_fields.iter()) {
        out.shift_remove(k);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dep(v: Value) -> Deployment {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn ninfer_alias() {
        let d = dep(
            json!({"name": "jarvis-brain", "cards": ["A", "B"], "servedId": "inkling-2-mini",
            "dropParams": ["response_format"],
            "modelInfo": {"engine": "ninfer", "effort_levels": ["none", "low", "medium", "xhigh"],
                          "thinking_off_effort": "none"}}),
        );
        let body = json!({"model": "jarvis-brain", "messages": [], "temperature": 0.7, "max_tokens": 400,
            "response_format": {"type": "json_object"}, "chat_template_kwargs": {"enable_thinking": false},
            "stream": true});
        let out = for_deployment(body.as_object().unwrap(), &d, &Settings::default());
        assert_eq!(
            Value::Object(out),
            json!({"model": "inkling-2-mini", "messages": [], "temperature": 0.7, "max_tokens": 400,
                "chat_template_kwargs": {"enable_thinking": false}, "stream": true, "reasoning_effort": "none"})
        );
    }

    #[test]
    fn strata_default_effort_is_only_a_default() {
        let d = dep(
            json!({"name": "flash-next", "relay": true, "servedId": "flash-next",
            "params": {"reasoning_effort": "low"}, "modelInfo": {"engine": "strata"}}),
        );
        let s = Settings::default();
        let a = for_deployment(json!({"model": "flash-next"}).as_object().unwrap(), &d, &s);
        assert_eq!(a["reasoning_effort"], "low");
        let b = for_deployment(
            json!({"model": "flash-next", "reasoning_effort": "high"})
                .as_object()
                .unwrap(),
            &d,
            &s,
        );
        assert_eq!(b["reasoning_effort"], "high");
    }

    #[test]
    fn extra_body_is_merged_before_the_effort_rules() {
        let d = dep(json!({"name": "x", "cards": ["A"], "servedId": "m",
            "modelInfo": {"effort_levels": ["none", "low", "medium", "xhigh"], "thinking_off_effort": "none"}}));
        let out = for_deployment(
            json!({"model": "x", "reasoning_effort": "low",
                   "extra_body": {"chat_template_kwargs": {"enable_thinking": false}, "reasoning_effort": "high"}})
            .as_object()
            .unwrap(),
            &d,
            &Settings::default(),
        );
        assert_eq!(
            Value::Object(out),
            json!({"model": "m", "reasoning_effort": "none", "chat_template_kwargs": {"enable_thinking": false}})
        );
    }

    #[test]
    fn metadata_and_user_are_stripped_by_default() {
        let d = dep(json!({"name": "x", "cards": ["A"], "servedId": "m"}));
        let out = for_deployment(
            json!({"model": "x", "metadata": {"session_id": "s"}, "user": "u", "uncensored": true})
                .as_object()
                .unwrap(),
            &d,
            &Settings::default(),
        );
        // unknown fields such as `uncensored` pass, as through LiteLLM
        assert_eq!(
            Value::Object(out),
            json!({"model": "m", "uncensored": true})
        );
    }

    #[test]
    fn strip_fields_apply_to_every_deployment() {
        let d = dep(json!({"name": "x", "cards": ["A"], "servedId": "m"}));
        let s = Settings {
            strip_fields: vec!["metadata".into()],
            ..Settings::default()
        };
        let out = for_deployment(
            json!({"model": "x", "metadata": {"a": 1}, "user": "u"})
                .as_object()
                .unwrap(),
            &d,
            &s,
        );
        assert_eq!(Value::Object(out), json!({"model": "m", "user": "u"}));
    }
}
