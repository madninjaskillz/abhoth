//! Port of Pi's tool-argument handling (`PiArguments`; pi-ai utils/validation.ts): optional-null
//! removal, TypeBox `Value.Convert` coercion ("5" -> 5, scalar -> [scalar], ...), and
//! TypeBox-style validation errors rendered as
//! `Validation failed for tool "x":\n  - path: message\n\nReceived arguments:\n{json}`.
//! Covers the JSON Schema subset Pi's tools use (object/array/string/number/integer/boolean
//! with properties, required and items).

use serde_json::{Map, Value};

use crate::json;
use crate::truncate::js_number;

/// Validates `arguments_json` against `schema`, returning the coerced arguments (compact,
/// relaxed escaping) or Pi's validation error text.
pub fn validate(tool_name: &str, schema: &Value, arguments_json: &str) -> Result<String, String> {
    let mut instance: Value = match serde_json::from_str(arguments_json) {
        Ok(v) => v,
        Err(e) => {
            return Err(format!(
                "Validation failed for tool \"{tool_name}\":\n  - invalid JSON: {e}\n\nReceived arguments:\n{arguments_json}"
            ));
        }
    };

    normalize_optional_nulls(schema, &mut instance);
    // Pi ignores Convert's return value for the root: only interior mutations survive.
    let _ = convert(schema, &mut instance);

    let mut failures = Vec::new();
    collect_errors(schema, &instance, "", &mut failures);
    if !failures.is_empty() {
        return Err(format!(
            "Validation failed for tool \"{tool_name}\":\n{}\n\nReceived arguments:\n{}",
            failures.join("\n"),
            pretty(arguments_json)
        ));
    }
    Ok(json::to_compact(&instance))
}

/// `JSON.stringify(args, null, 2)` of the arguments as the model sent them.
pub fn pretty(json_text: &str) -> String {
    match serde_json::from_str::<Value>(json_text) {
        Ok(v) => json::to_pretty(&v),
        Err(_) => json_text.to_string(),
    }
}

fn type_of(schema: &Value) -> Option<&str> {
    schema.get("type").and_then(Value::as_str)
}

fn required(schema: &Value) -> Vec<&str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn properties(schema: &Value) -> Option<&Map<String, Value>> {
    schema.get("properties").and_then(Value::as_object)
}

fn items(schema: &Value) -> Option<&Value> {
    schema.get("items")
}

fn accepts_null(schema: &Value) -> bool {
    matches!(type_of(schema), None | Some("null"))
}

fn normalize_optional_nulls(schema: &Value, value: &mut Value) {
    if let Value::Array(list) = value {
        if let Some(item_schema) = items(schema) {
            for item in list.iter_mut() {
                normalize_optional_nulls(item_schema, item);
            }
        }
        return;
    }
    let (Value::Object(obj), Some(props)) = (value, properties(schema)) else {
        return;
    };
    let req = required(schema);
    for (key, child_schema) in props {
        let Some(current) = obj.get_mut(key) else {
            continue;
        };
        if current.is_null() && !req.contains(&key.as_str()) && !accepts_null(child_schema) {
            obj.shift_remove(key);
            continue;
        }
        normalize_optional_nulls(child_schema, current);
    }
}

/// TypeBox `Value.Convert` for the schema kinds Pi's tools use. Mutates containers in place
/// and returns the replacement for a converted scalar (or a scalar wrapped into an array).
fn convert(schema: &Value, value: &mut Value) -> Option<Value> {
    match type_of(schema) {
        Some(t @ ("number" | "integer")) => {
            to_number(value).map(|n| json::number_value(if t == "integer" { n.trunc() } else { n }))
        }
        Some("boolean") => to_boolean(value).map(Value::Bool),
        Some("string") => to_string_value(value).map(Value::String),
        Some("array") => {
            let item_schema = items(schema);
            let convert_items = |list: &mut Vec<Value>| {
                if let Some(item_schema) = item_schema {
                    for item in list.iter_mut() {
                        if let Some(c) = convert(item_schema, item) {
                            *item = c;
                        }
                    }
                }
            };
            if let Value::Array(list) = value {
                convert_items(list);
                None
            } else {
                let mut list = vec![value.clone()];
                convert_items(&mut list);
                Some(Value::Array(list))
            }
        }
        Some("object") => {
            if let (Value::Object(obj), Some(props)) = (value, properties(schema)) {
                for (key, child_schema) in props {
                    if let Some(current) = obj.get_mut(key)
                        && let Some(c) = convert(child_schema, current)
                    {
                        *current = c;
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// `^[+-]?(?:[0-9]+\.?[0-9]*|\.[0-9]+)(?:[eE][+-]?[0-9]+)?$`
fn is_decimal(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - int_start;
    if int_digits > 0 {
        if i < b.len() && b[i] == b'.' {
            i += 1;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        }
    } else {
        if i >= b.len() || b[i] != b'.' {
            return false;
        }
        i += 1;
        let frac_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == frac_start {
            return false;
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let exp_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return false;
        }
    }
    i == b.len()
}

/// .NET `string.Trim()` whitespace.
pub(crate) fn dotnet_trim(s: &str) -> &str {
    s.trim_matches(char::is_whitespace)
}

fn to_number(value: &Value) -> Option<f64> {
    match value {
        Value::Null => Some(0.0),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let trimmed = dotnet_trim(s);
            if trimmed.is_empty() {
                return Some(0.0);
            }
            let tb = trimmed.as_bytes();
            if tb.len() > 2
                && tb[0] == b'0'
                && (tb[1] == b'x' || tb[1] == b'X')
                && let Ok(hex) = i64::from_str_radix(&trimmed[2..], 16)
            {
                return Some(hex as f64);
            }
            if is_decimal(trimmed)
                && let Ok(d) = trimmed.parse::<f64>()
                && d.is_finite()
            {
                return Some(d);
            }
            match s.to_lowercase().as_str() {
                "true" => Some(1.0),
                "false" => Some(0.0),
                _ => None,
            }
        }
        _ => None,
    }
}

fn to_boolean(value: &Value) -> Option<bool> {
    match value {
        Value::Null => Some(false),
        Value::Bool(b) => Some(*b),
        Value::Number(n) => {
            let d = n.as_f64()?;
            if d == 0.0 {
                Some(false)
            } else if d == 1.0 {
                Some(true)
            } else {
                None
            }
        }
        Value::String(s) => match s.to_lowercase().as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn to_string_value(value: &Value) -> Option<String> {
    match value {
        Value::Null => Some("null".into()),
        Value::Bool(true) => Some("true".into()),
        Value::Bool(false) => Some("false".into()),
        Value::Number(n) => Some(js_number(n.as_f64().unwrap_or(0.0))),
        _ => None,
    }
}

fn matches_type(value: &Value, t: &str) -> bool {
    match t {
        "number" => value.is_number(),
        "integer" => value.as_f64().is_some_and(|d| d.trunc() == d),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

fn argument_path(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

fn collect_errors(schema: &Value, value: &Value, path: &str, lines: &mut Vec<String>) {
    let shown = if path.is_empty() { "root" } else { path };
    if let Some(t) = type_of(schema)
        && !matches_type(value, t)
    {
        lines.push(format!("  - {shown}: must be {t}"));
    }

    if let Value::Object(obj) = value {
        let missing: Vec<&str> = required(schema)
            .into_iter()
            .filter(|k| !obj.contains_key(*k))
            .collect();
        if let Some(first) = missing.first() {
            lines.push(format!(
                "  - {}: must have required properties {}",
                argument_path(path, first),
                missing.join(", ")
            ));
        }
        if let Some(props) = properties(schema) {
            for (key, child_schema) in props {
                if let Some(item) = obj.get(key) {
                    let escaped = key.replace('~', "~0").replace('/', "~1");
                    collect_errors(child_schema, item, &argument_path(path, &escaped), lines);
                }
            }
        }
    }

    if let (Value::Array(list), Some(item_schema)) = (value, items(schema)) {
        for (i, item) in list.iter().enumerate() {
            collect_errors(
                item_schema,
                item,
                &argument_path(path, &i.to_string()),
                lines,
            );
        }
    }
}

/// Pi's prepareEditArguments: models that send edits as a JSON string, as a single edit object,
/// or in the legacy top-level {oldText, newText} shape reach validation as {path, edits:[...]}.
/// Anything else passes through unchanged.
pub fn prepare_edit_arguments(raw: &str) -> String {
    let Ok(Value::Object(mut args)) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string();
    };

    let mut changed = false;
    match args.get("edits") {
        Some(Value::String(s)) => {
            if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                if parsed.is_array() {
                    args.insert("edits".into(), parsed);
                    changed = true;
                } else if is_single_edit_input(&parsed) {
                    args.insert("edits".into(), Value::Array(vec![parsed]));
                    changed = true;
                }
            }
        }
        Some(edits) if is_single_edit_input(edits) => {
            let single = edits.clone();
            args.insert("edits".into(), Value::Array(vec![single]));
            changed = true;
        }
        _ => {}
    }

    if let (Some(Value::String(old)), Some(Value::String(new))) =
        (args.get("oldText"), args.get("newText"))
    {
        let mut entry = Map::new();
        entry.insert("oldText".into(), Value::String(old.clone()));
        entry.insert("newText".into(), Value::String(new.clone()));
        let mut list = match args.get("edits") {
            Some(Value::Array(existing)) => existing.clone(),
            _ => Vec::new(),
        };
        list.push(Value::Object(entry));
        args.insert("edits".into(), Value::Array(list));
        args.shift_remove("oldText");
        args.shift_remove("newText");
        changed = true;
    }

    if changed {
        json::to_compact(&Value::Object(args))
    } else {
        raw.to_string()
    }
}

fn is_single_edit_input(value: &Value) -> bool {
    value.get("oldText").is_some_and(Value::is_string)
        && value.get("newText").is_some_and(Value::is_string)
        && value.is_object()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_pattern() {
        for ok in ["5", "-5", "+5.", "5.5", ".5", "1e3", "1.5E-2"] {
            assert!(is_decimal(ok), "{ok}");
        }
        for bad in ["", ".", "e3", "1e", "abc", "5 5", "0x10"] {
            assert!(!is_decimal(bad), "{bad}");
        }
    }

    #[test]
    fn coerces_hex_and_booleans() {
        let schema: Value = serde_json::from_str(
            r#"{"type":"object","properties":{"n":{"type":"number"},"b":{"type":"boolean"}}}"#,
        )
        .unwrap();
        assert_eq!(
            validate("t", &schema, r#"{"n":"0x10","b":"TRUE"}"#).unwrap(),
            r#"{"n":16,"b":true}"#
        );
    }
}
