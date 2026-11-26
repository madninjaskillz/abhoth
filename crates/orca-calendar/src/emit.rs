//! Writing entries back as `calendar.toml` text in the style of SUPERVISOR.md 9.8: one table
//! per entry, effects and detect as inline tables, the key policy as a sub-table.

use serde_json::Value;

use crate::entry::EntryConfig;

fn quote(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn bare_key(k: &str) -> String {
    if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        k.to_string()
    } else {
        quote(k)
    }
}

/// A JSON value as an inline TOML value.
pub fn inline(v: &Value) -> String {
    match v {
        Value::Null => "\"\"".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => quote(s),
        Value::Array(a) => format!("[{}]", a.iter().map(inline).collect::<Vec<_>>().join(", ")),
        Value::Object(o) => {
            let parts: Vec<String> = o
                .iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| format!("{} = {}", bare_key(k), inline(v)))
                .collect();
            if parts.is_empty() {
                "{}".into()
            } else {
                format!("{{ {} }}", parts.join(", "))
            }
        }
    }
}

/// One entry as `[calendar.entries.<id>]` (+ `.key`).
pub fn entry(id: &str, e: &EntryConfig) -> String {
    let v = serde_json::to_value(e).unwrap_or(Value::Null);
    let Value::Object(mut o) = v else { return String::new() };
    let key = o.remove("key");
    let mut out = format!("[calendar.entries.{}]\n", bare_key(id));
    for (k, val) in &o {
        if val.is_null() {
            continue;
        }
        if k == "enabled" && val == &Value::Bool(true) {
            continue;
        }
        if k == "priority" && val == "normal" {
            continue;
        }
        out.push_str(&format!("{} = {}\n", bare_key(k), inline(val)));
    }
    if let Some(Value::Object(k)) = key {
        out.push_str(&format!("\n[calendar.entries.{}.key]\n", bare_key(id)));
        for (kk, val) in &k {
            out.push_str(&format!("{} = {}\n", bare_key(kk), inline(val)));
        }
    }
    out
}

/// Several entries, separated by blank lines, under an optional header comment.
pub fn entries<'a>(header: &str, list: impl IntoIterator<Item = (&'a String, &'a EntryConfig)>) -> String {
    let mut out = String::new();
    for l in header.lines() {
        out.push_str(&format!("# {l}\n"));
    }
    for (id, e) in list {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&entry(id, e));
    }
    out
}
