//! Port of DeepThought's LiteLLM callback `effort_hook.py` (2026-11-22/24): clamp
//! `reasoning_effort` to what the chosen deployment's engine accepts, and handle a client that
//! switched thinking off.
//!
//! - **Clamp** (`model_info.effort_levels`): a level the backend accepts passes; otherwise the
//!   highest accepted level below the request (never "none" unless asked for), or, if none is
//!   below, the lowest accepted level above it. ninfer `[none, low, medium, xhigh]`: minimal ->
//!   low, high -> medium, max -> xhigh. Unknown words and non-strings pass (the backend decides).
//! - **Thinking off** (`model_info.thinking_off_effort`): when `enable_thinking` is `false` at
//!   the top level, in `chat_template_kwargs`, in `extra_body` or in
//!   `extra_body.chat_template_kwargs`: `"drop"` removes `reasoning_effort` everywhere (ninfer
//!   400s the combination); a level sets the top-level `reasoning_effort` to it (Inkling: its
//!   template ignores enable_thinking, effort none = 0). Applied instead of the clamp.

use serde_json::{Map, Value};

/// The effort words in order (effort_hook.ORDER).
pub const ORDER: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

fn rank(v: &str) -> Option<usize> {
    ORDER.iter().position(|x| *x == v)
}

/// `clamp(value, levels)` from the hook.
pub fn clamp(value: &Value, levels: &[String]) -> Value {
    let Some(s) = value.as_str() else {
        return value.clone();
    };
    let v = s.trim().to_lowercase();
    if levels.contains(&v) {
        return value.clone();
    }
    let Some(i) = rank(&v) else {
        return value.clone();
    };
    if v == "none" {
        return value.clone();
    }
    let mut ranked: Vec<(usize, &str)> = levels
        .iter()
        .filter(|x| x.as_str() != "none")
        .filter_map(|x| rank(x).map(|r| (r, x.as_str())))
        .collect();
    ranked.sort();
    let below = ranked.iter().rfind(|(r, _)| *r < i);
    let above = ranked.iter().find(|(r, _)| *r > i);
    match below.or(above) {
        Some((_, x)) => Value::String((*x).to_string()),
        None => value.clone(),
    }
}

/// Where the hook looks: the body, `extra_body`, `chat_template_kwargs`,
/// `extra_body.chat_template_kwargs` (in that order). Paths as key lists.
const HOLDERS: [&[&str]; 4] = [
    &[],
    &["extra_body"],
    &["chat_template_kwargs"],
    &["extra_body", "chat_template_kwargs"],
];

fn holder<'a>(body: &'a Map<String, Value>, path: &[&str]) -> Option<&'a Map<String, Value>> {
    let mut cur = body;
    for k in path {
        cur = cur.get(*k)?.as_object()?;
    }
    Some(cur)
}

fn holder_mut<'a>(
    body: &'a mut Map<String, Value>,
    path: &[&str],
) -> Option<&'a mut Map<String, Value>> {
    let mut cur = body;
    for k in path {
        cur = cur.get_mut(*k)?.as_object_mut()?;
    }
    Some(cur)
}

/// Did the client switch thinking off (`enable_thinking: false` in any holder)?
pub fn thinking_off(body: &Map<String, Value>) -> bool {
    HOLDERS.iter().any(|p| {
        holder(body, p).is_some_and(|h| h.get("enable_thinking") == Some(&Value::Bool(false)))
    })
}

/// Apply the hook's rules for one deployment. Returns true when the body changed.
pub fn apply(
    body: &mut Map<String, Value>,
    levels: Option<&[String]>,
    thinking_off_effort: Option<&str>,
) -> bool {
    let levels = levels.filter(|l| !l.is_empty());
    let off = thinking_off_effort.filter(|o| !o.is_empty());
    if levels.is_none() && off.is_none() {
        return false;
    }
    let mut changed = false;
    if let Some(off) = off
        && thinking_off(body)
    {
        for (i, p) in HOLDERS.iter().enumerate() {
            let Some(h) = holder_mut(body, p) else {
                continue;
            };
            if off == "drop" {
                if let Some(v) = h.remove("reasoning_effort")
                    && !v.is_null()
                {
                    changed = true;
                }
            } else if i == 0 && h.get("reasoning_effort").and_then(Value::as_str) != Some(off) {
                h.insert("reasoning_effort".into(), Value::String(off.to_string()));
                changed = true;
            }
        }
        return changed;
    }
    if let Some(levels) = levels {
        for p in HOLDERS {
            let Some(h) = holder_mut(body, p) else {
                continue;
            };
            if let Some(v) = h.get("reasoning_effort").filter(|v| !v.is_null()) {
                let new = clamp(v, levels);
                if new != *v {
                    h.insert("reasoning_effort".into(), new);
                    changed = true;
                }
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ninfer() -> Vec<String> {
        ["none", "low", "medium", "xhigh"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn clamp_table() {
        let l = ninfer();
        let cases = [
            (json!("minimal"), json!("low")),
            (json!("low"), json!("low")),
            (json!("medium"), json!("medium")),
            (json!("high"), json!("medium")),
            (json!("xhigh"), json!("xhigh")),
            (json!("max"), json!("xhigh")),
            (json!("none"), json!("none")),
            (json!("HIGH"), json!("medium")),
            (json!(" high "), json!("medium")),
            (json!("Low"), json!("Low")), // not accepted as written, but "low" is: unchanged
            (json!("turbo"), json!("turbo")),
            (json!(3), json!(3)),
            (json!(null), json!(null)),
        ];
        for (input, want) in cases {
            assert_eq!(clamp(&input, &l), want, "clamp({input})");
        }
    }

    #[test]
    fn clamp_goes_up_when_nothing_is_below() {
        let l: Vec<String> = vec!["medium".into(), "high".into()];
        assert_eq!(clamp(&json!("minimal"), &l), json!("medium"));
        assert_eq!(clamp(&json!("low"), &l), json!("medium"));
        assert_eq!(clamp(&json!("max"), &l), json!("high"));
        // only "none" accepted: nothing to map to, pass through
        assert_eq!(clamp(&json!("high"), &["none".to_string()]), json!("high"));
    }

    fn run(body: Value, levels: Option<Vec<String>>, off: Option<&str>) -> (Value, bool) {
        let mut m = body.as_object().unwrap().clone();
        let changed = apply(&mut m, levels.as_deref(), off);
        (Value::Object(m), changed)
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn hook_table() {
        let l = Some(ninfer());
        // (body, levels, thinking_off_effort, expected body, changed)
        let cases: Vec<(Value, Option<Vec<String>>, Option<&str>, Value, bool)> = vec![
            // no rules: untouched
            (
                json!({"reasoning_effort": "high"}),
                None,
                None,
                json!({"reasoning_effort": "high"}),
                false,
            ),
            // clamp top level
            (
                json!({"reasoning_effort": "high"}),
                l.clone(),
                Some("drop"),
                json!({"reasoning_effort": "medium"}),
                true,
            ),
            (
                json!({"reasoning_effort": "low"}),
                l.clone(),
                Some("drop"),
                json!({"reasoning_effort": "low"}),
                false,
            ),
            // clamp in extra_body too
            (
                json!({"extra_body": {"reasoning_effort": "max"}}),
                l.clone(),
                None,
                json!({"extra_body": {"reasoning_effort": "xhigh"}}),
                true,
            ),
            // Jarvis: thinking off in chat_template_kwargs + an effort: ninfer drops it
            (
                json!({"reasoning_effort": "low", "chat_template_kwargs": {"enable_thinking": false}}),
                l.clone(),
                Some("drop"),
                json!({"chat_template_kwargs": {"enable_thinking": false}}),
                true,
            ),
            // drop also inside the holders
            (
                json!({"chat_template_kwargs": {"enable_thinking": false, "reasoning_effort": "low"},
                    "extra_body": {"reasoning_effort": "high"}}),
                l.clone(),
                Some("drop"),
                json!({"chat_template_kwargs": {"enable_thinking": false}, "extra_body": {}}),
                true,
            ),
            // PiG "off": top-level enable_thinking false
            (
                json!({"enable_thinking": false, "reasoning_effort": "medium"}),
                l.clone(),
                Some("drop"),
                json!({"enable_thinking": false}),
                true,
            ),
            // thinking off, no effort sent: nothing to drop
            (
                json!({"chat_template_kwargs": {"enable_thinking": false}}),
                l.clone(),
                Some("drop"),
                json!({"chat_template_kwargs": {"enable_thinking": false}}),
                false,
            ),
            // Inkling: thinking off forces the top-level effort to "none"
            (
                json!({"chat_template_kwargs": {"enable_thinking": false}}),
                l.clone(),
                Some("none"),
                json!({"chat_template_kwargs": {"enable_thinking": false}, "reasoning_effort": "none"}),
                true,
            ),
            (
                json!({"reasoning_effort": "high", "chat_template_kwargs": {"enable_thinking": false}}),
                l.clone(),
                Some("none"),
                json!({"reasoning_effort": "none", "chat_template_kwargs": {"enable_thinking": false}}),
                true,
            ),
            (
                json!({"reasoning_effort": "none", "chat_template_kwargs": {"enable_thinking": false}}),
                l.clone(),
                Some("none"),
                json!({"reasoning_effort": "none", "chat_template_kwargs": {"enable_thinking": false}}),
                false,
            ),
            // a level mode does not touch the holders' efforts (hook: only `h is kwargs`)
            (
                json!({"extra_body": {"chat_template_kwargs": {"enable_thinking": false}, "reasoning_effort": "max"}}),
                l.clone(),
                Some("none"),
                json!({"extra_body": {"chat_template_kwargs": {"enable_thinking": false}, "reasoning_effort": "max"},
                    "reasoning_effort": "none"}),
                true,
            ),
            // thinking on (true) is not off: clamp applies
            (
                json!({"reasoning_effort": "high", "chat_template_kwargs": {"enable_thinking": true}}),
                l.clone(),
                Some("none"),
                json!({"reasoning_effort": "medium", "chat_template_kwargs": {"enable_thinking": true}}),
                true,
            ),
            // "false" as a string is not off
            (
                json!({"reasoning_effort": "high", "enable_thinking": "false"}),
                l.clone(),
                Some("drop"),
                json!({"reasoning_effort": "medium", "enable_thinking": "false"}),
                true,
            ),
            // off mode only (no levels): clamp skipped, thinking-off still applies
            (
                json!({"reasoning_effort": "high"}),
                None,
                Some("drop"),
                json!({"reasoning_effort": "high"}),
                false,
            ),
            (
                json!({"reasoning_effort": "high", "enable_thinking": false}),
                None,
                Some("drop"),
                json!({"enable_thinking": false}),
                true,
            ),
            // null effort is left alone by the clamp
            (
                json!({"reasoning_effort": null}),
                l.clone(),
                None,
                json!({"reasoning_effort": null}),
                false,
            ),
            // preserve_thinking with the effort (OpenOrca slots extraBody) passes
            (
                json!({"reasoning_effort": "low", "chat_template_kwargs": {"preserve_thinking": true}}),
                l.clone(),
                Some("drop"),
                json!({"reasoning_effort": "low", "chat_template_kwargs": {"preserve_thinking": true}}),
                false,
            ),
        ];
        for (i, (body, levels, off, want, want_changed)) in cases.into_iter().enumerate() {
            let (got, changed) = run(body, levels, off);
            assert_eq!(got, want, "case {i}");
            assert_eq!(changed, want_changed, "case {i} changed");
        }
    }
}
