//! JSON text as System.Text.Json writes it with `JavaScriptEncoder.UnsafeRelaxedJsonEscaping`:
//! compact (`JsonNode.ToJsonString`) and Pi's `JSON.stringify(args, null, 2)` shape (two-space
//! indent, `\n`). Only `"`, `\`, control characters and characters outside the Basic
//! Multilingual Plane (as UTF-16 surrogate pairs) are escaped.

use serde_json::Value;

/// Compact JSON with relaxed escaping.
pub fn to_compact(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, None, 0);
    out
}

/// Indented JSON (two spaces, `\n` line breaks) with relaxed escaping.
pub fn to_pretty(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, Some(2), 0);
    out
}

fn newline_indent(out: &mut String, indent: Option<usize>, level: usize) {
    if let Some(n) = indent {
        out.push('\n');
        for _ in 0..n * level {
            out.push(' ');
        }
    }
}

fn write_value(out: &mut String, value: &Value, indent: Option<usize>, level: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&number_text(n)),
        Value::String(s) => write_string(out, s),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline_indent(out, indent, level + 1);
                write_value(out, item, indent, level + 1);
            }
            newline_indent(out, indent, level);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline_indent(out, indent, level + 1);
                write_string(out, k);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write_value(out, v, indent, level + 1);
            }
            newline_indent(out, indent, level);
            out.push('}');
        }
    }
}

/// A number as .NET writes a double: integers without a fraction, otherwise the shortest
/// round-trip form.
pub fn number_text(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    if f.fract() == 0.0 && f.abs() < 1e15 {
        return format!("{f:.0}");
    }
    format!("{f}")
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c if (c as u32) > 0xFFFF => {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{u:04X}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A whole double as a JSON number value (`JsonValue.Create(double)`).
pub fn number_value(f: f64) -> Value {
    if f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 {
        Value::from(f as i64)
    } else {
        serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_and_pretty_match_system_text_json_relaxed() {
        let v: Value =
            serde_json::from_str(r#"{"a":"x\"y<&>é😀","b":[1,2.5],"c":{},"d":[]}"#).unwrap();
        assert_eq!(
            to_compact(&v),
            "{\"a\":\"x\\\"y<&>é\x5CuD83D\x5CuDE00\",\"b\":[1,2.5],\"c\":{},\"d\":[]}"
        );
        assert_eq!(
            to_pretty(&v),
            "{\n  \"a\": \"x\\\"y<&>é\\uD83D\\uDE00\",\n  \"b\": [\n    1,\n    2.5\n  ],\n  \"c\": {},\n  \"d\": []\n}"
        );
    }
}
