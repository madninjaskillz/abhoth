//! Dotted-path edits of a config document (`ConfigOverrides`): "slots.copies.0.slots",
//! "agent.peerMode". Segments are camelCase JSON names; a number indexes an array. Missing
//! objects on the way are created; a JSON null value removes the key.

use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct OverrideError(pub String);

fn split(path: &str) -> Result<Vec<&str>, OverrideError> {
    let parts: Vec<&str> = path.split('.').map(str::trim).collect();
    if parts.is_empty() || parts.iter().any(|p| p.is_empty()) {
        return Err(OverrideError(format!("Bad setting path '{path}'.")));
    }
    Ok(parts)
}

fn index_of(s: &str) -> Option<usize> {
    // int.TryParse: optional sign and surrounding whitespace; negatives are not indexes.
    s.trim()
        .parse::<i64>()
        .ok()
        .filter(|i| *i >= 0)
        .map(|i| i as usize)
}

fn is_int(s: &str) -> bool {
    s.trim().parse::<i32>().is_ok()
}

fn not_here(path: &str, key: &str) -> OverrideError {
    OverrideError(format!(
        "'{path}': '{key}' is not an object key or array index here."
    ))
}

fn child<'a>(
    node: &'a mut Value,
    key: &str,
    next: &str,
    path: &str,
) -> Result<&'a mut Value, OverrideError> {
    let want_array = is_int(next);
    let fresh = || {
        if want_array {
            Value::Array(Vec::new())
        } else {
            Value::Object(Map::new())
        }
    };
    match node {
        Value::Object(o) => {
            let slot = o.entry(key.to_string()).or_insert(Value::Null);
            if !matches!(slot, Value::Object(_) | Value::Array(_)) {
                *slot = fresh();
            }
            Ok(slot)
        }
        Value::Array(a) => {
            let Some(idx) = index_of(key) else {
                return Err(not_here(path, key));
            };
            while a.len() <= idx {
                a.push(Value::Object(Map::new()));
            }
            if !matches!(a[idx], Value::Object(_) | Value::Array(_)) {
                a[idx] = fresh();
            }
            Ok(&mut a[idx])
        }
        _ => Err(not_here(path, key)),
    }
}

/// Set `path` in `root` to a copy of `value` (`None`/JSON null removes an object key).
pub fn set(root: &mut Value, path: &str, value: Option<&Value>) -> Result<(), OverrideError> {
    let parts = split(path)?;
    let mut node = root;
    for i in 0..parts.len() - 1 {
        node = child(node, parts[i], parts[i + 1], path)?;
    }
    let last = parts[parts.len() - 1];
    let copy = value.filter(|v| !v.is_null()).cloned();
    match node {
        Value::Object(o) => match copy {
            None => {
                o.shift_remove(last);
            }
            Some(v) => {
                o.insert(last.to_string(), v);
            }
        },
        Value::Array(a) => {
            let Some(idx) = index_of(last) else {
                return Err(not_here(path, last));
            };
            while a.len() <= idx {
                a.push(Value::Object(Map::new()));
            }
            a[idx] = copy.unwrap_or(Value::Null);
        }
        _ => return Err(not_here(path, last)),
    }
    Ok(())
}

/// "a.b.c=value": the value is parsed as JSON when it is valid JSON, else taken as a string.
pub fn set_from_line(root: &mut Value, line: &str) -> Result<(), OverrideError> {
    match line.find('=') {
        Some(eq) if eq > 0 => {
            let value = parse_value(&line[eq + 1..]);
            set(root, line[..eq].trim(), Some(&value))
        }
        _ => Err(OverrideError(format!("A setting needs key=value: {line}"))),
    }
}

/// JSON when it parses (numbers, true/false, null, objects, arrays, quoted strings), else the raw text.
pub fn parse_value(raw: &str) -> Value {
    if raw.is_empty() {
        return Value::String(String::new());
    }
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// The node at `path`, or `None` when any part is missing (or the value is null).
pub fn get<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut node = root;
    for part in split(path).ok()? {
        node = match node {
            Value::Object(o) => o.get(part)?,
            Value::Array(a) => a.get(index_of(part)?)?,
            _ => return None,
        };
        if node.is_null() {
            return None;
        }
    }
    Some(node)
}

/// Whether `path` exists (even with a null value).
pub fn has(root: &Value, path: &str) -> bool {
    let Ok(parts) = split(path) else {
        return false;
    };
    let parent = if parts.len() == 1 {
        Some(root)
    } else {
        get(root, &parts[..parts.len() - 1].join("."))
    };
    let last = parts[parts.len() - 1];
    match parent {
        Some(Value::Object(o)) => o.contains_key(last),
        Some(Value::Array(a)) => index_of(last).is_some_and(|i| i < a.len()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Ported from ConfigOverridesTests.

    #[test]
    fn set_creates_paths_and_indexes_arrays() {
        let mut root = json!({});
        set(&mut root, "agent.peerMode", Some(&json!(true))).unwrap();
        set_from_line(&mut root, "slots.copies=[{\"name\":\"A\",\"slots\":3}]").unwrap();
        set_from_line(&mut root, "slots.copies.0.slots=1").unwrap();
        set_from_line(&mut root, "slots.model=qwen3.8-flash-next").unwrap();
        assert_eq!(root["agent"]["peerMode"], true);
        assert_eq!(root["slots"]["copies"][0]["slots"], 1);
        assert_eq!(root["slots"]["model"], "qwen3.8-flash-next");
        assert!(has(&root, "slots.copies.0.name"));
        assert!(!has(&root, "slots.copies.1"));
    }

    #[test]
    fn set_null_removes_the_key() {
        let mut root = json!({"slots":{"registryUrl":"http://x","model":"m"}});
        set(&mut root, "slots.registryUrl", None).unwrap();
        assert!(!has(&root, "slots.registryUrl"));
        assert!(has(&root, "slots.model"));
    }

    #[test]
    fn set_copies_the_value() {
        let value = json!({"a": 1});
        let mut r1 = json!({});
        let mut r2 = json!({});
        set(&mut r1, "x", Some(&value)).unwrap();
        set(&mut r2, "x", Some(&value)).unwrap();
        assert_eq!(r2["x"]["a"], 1);
    }

    #[test]
    fn bad_paths_and_lines_are_errors() {
        let mut root = json!({"a": [1]});
        assert_eq!(
            set(&mut root, "a..b", Some(&json!(1))).unwrap_err().0,
            "Bad setting path 'a..b'."
        );
        assert_eq!(
            set(&mut root, "a.x", Some(&json!(1))).unwrap_err().0,
            "'a.x': 'x' is not an object key or array index here."
        );
        assert_eq!(
            set_from_line(&mut root, "=1").unwrap_err().0,
            "A setting needs key=value: =1"
        );
        assert_eq!(parse_value(""), json!(""));
        assert_eq!(parse_value("12"), json!(12));
    }
}
