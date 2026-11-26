//! The admin pages' forms over `router.toml` (D151). A form reads the main file as data
//! (`GET /admin/api/config/json`) and sends back what it changed as edits
//! (`POST /admin/api/config/edit`): each edit sets or removes one value at a path. The edits are
//! applied to the file's text with `toml_edit`, so comments, order and the untouched parts stay as
//! they were; the result is checked by the router's own validator and comes back with its diff.
//! Nothing is written here: the page saves the returned text through the usual save (If-Match,
//! backup, atomic write, audit), so a form save and a text save are the same thing.

use serde::Deserialize;
use serde_json::{Map, Value, json};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, TableLike};

use super::model::ConfigDoc;

/// One change: the value at `path` becomes `value` (`null` removes it). Objects are merged into
/// an existing table key by key (keys the object leaves out are removed), so send the whole
/// object you edited.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Edit {
    pub path: Vec<String>,
    #[serde(default)]
    pub value: Value,
}

/// `{"edits": [...]}` from a request body.
pub fn parse_edits(body: &Value) -> Result<Vec<Edit>, String> {
    let edits: Vec<Edit> = serde_json::from_value(body.get("edits").cloned().unwrap_or_default())
        .map_err(|_| "expected {edits: [{path: [..], value}]}".to_string())?;
    if edits.is_empty() {
        return Err("no edits".into());
    }
    for e in &edits {
        if e.path.is_empty() || e.path.iter().any(|p| p.is_empty()) {
            return Err("an edit needs a path of non-empty keys".into());
        }
        if e.path.len() > 8 {
            return Err("an edit's path is too deep".into());
        }
    }
    Ok(edits)
}

/// The main file as data for the forms: `{rev, path, error, data, includes, parseError}`.
pub fn config_json(doc: &ConfigDoc) -> Value {
    let (data, parse_error) = match toml::from_str::<toml::Value>(&doc.text) {
        Ok(v) => (serde_json::to_value(v).unwrap_or(Value::Null), None),
        Err(e) => (Value::Null, Some(e.to_string())),
    };
    let includes = data.get("include").cloned().unwrap_or(json!([]));
    json!({
        "rev": doc.rev,
        "path": doc.path,
        "error": doc.error,
        "data": data,
        "includes": includes,
        "parseError": parse_error,
    })
}

/// Apply `edits` to `text` in order; Err when the text does not parse or an edit can't be
/// placed (e.g. a path through a value that is not a table).
pub fn apply_edits(text: &str, edits: &[Edit]) -> Result<String, String> {
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e| format!("router.toml does not parse: {e}"))?;
    for e in edits {
        apply(doc.as_table_mut(), &e.path, &e.value)
            .map_err(|m| format!("{}: {m}", e.path.join(".")))?;
    }
    Ok(doc.to_string())
}

fn apply(root: &mut Table, path: &[String], value: &Value) -> Result<(), String> {
    let (last, parents) = path.split_last().expect("checked non-empty");
    // Walk to the parent, making tables on the way (unless removing).
    let mut cur: &mut dyn TableLike = root;
    let mut inline_parent = false;
    for (depth, k) in parents.iter().enumerate() {
        if cur.get(k).is_none() {
            if value.is_null() {
                return Ok(()); // nothing to remove
            }
            let mut t = Table::new();
            t.set_implicit(true);
            // Top-level sections are tables of their own; deeper ones dotted (`a.b = ...`).
            if depth > 0 {
                t.set_dotted(true);
            }
            cur.insert(k, Item::Table(t));
        }
        let next = cur.get_mut(k).expect("just made");
        inline_parent = next.is_inline_table();
        cur = match next {
            Item::Table(t) => t as &mut dyn TableLike,
            Item::Value(toml_edit::Value::InlineTable(t)) => t as &mut dyn TableLike,
            _ => return Err(format!("{k} is not a table")),
        };
    }
    if value.is_null() {
        cur.remove(last);
        return Ok(());
    }
    match cur.get_mut(last) {
        Some(existing) if value.is_object() && existing.is_table_like() => {
            let inline = existing.is_inline_table();
            let t = existing.as_table_like_mut().expect("table-like");
            merge(t, value.as_object().expect("object"), inline)
        }
        Some(existing) => {
            let hint = existing.as_value().cloned();
            let mut v = to_value(value, hint.as_ref())?;
            if let Some(old) = existing.as_value() {
                *v.decor_mut() = old.decor().clone();
            }
            *existing = Item::Value(v);
            Ok(())
        }
        None => {
            let item = if inline_parent {
                Item::Value(to_value(value, None)?)
            } else {
                new_item(value, parents.len() <= 1)?
            };
            cur.insert(last, item);
            Ok(())
        }
    }
}

/// Merge `obj` into an existing table: keys it lacks are removed, values replaced keeping their
/// formatting, tables merged recursively.
fn merge(t: &mut dyn TableLike, obj: &Map<String, Value>, inline: bool) -> Result<(), String> {
    let gone: Vec<String> = t
        .iter()
        .map(|(k, _)| k.to_string())
        .filter(|k| !obj.contains_key(k))
        .collect();
    for k in gone {
        t.remove(&k);
    }
    for (k, v) in obj {
        if v.is_null() {
            t.remove(k);
            continue;
        }
        match t.get_mut(k) {
            Some(existing) if v.is_object() && existing.is_table_like() => {
                let inl = existing.is_inline_table();
                merge(
                    existing.as_table_like_mut().expect("table-like"),
                    v.as_object().expect("object"),
                    inl,
                )?;
            }
            Some(existing) => {
                let hint = existing.as_value().cloned();
                let mut nv = to_value(v, hint.as_ref())?;
                if let Some(old) = existing.as_value() {
                    *nv.decor_mut() = old.decor().clone();
                }
                *existing = Item::Value(nv);
            }
            None => {
                let item = if inline {
                    Item::Value(to_value(v, None)?)
                } else {
                    new_item(v, false)?
                };
                t.insert(k, item);
            }
        }
    }
    Ok(())
}

/// A new item: an object at the top level (`servers.gpu`) is a `[table]`; an object of objects
/// below it is dotted (`profiles.4lane = { ... }`); other objects are inline tables.
fn new_item(value: &Value, top: bool) -> Result<Item, String> {
    match value {
        Value::Object(m) if top || m.values().any(Value::is_object) => {
            let mut t = Table::new();
            if !top {
                t.set_dotted(true);
            }
            for (k, v) in m {
                if v.is_null() {
                    continue;
                }
                let item = if v.is_object()
                    && v.as_object()
                        .is_some_and(|o| o.values().any(Value::is_object))
                {
                    new_item(v, false)?
                } else {
                    Item::Value(to_value(v, None)?)
                };
                t.insert(k, item);
            }
            if top && t.is_empty() {
                t.set_implicit(false);
            }
            Ok(Item::Table(t))
        }
        v => Ok(Item::Value(to_value(v, None)?)),
    }
}

/// A JSON value as a TOML value. `hint`: the value it replaces (a float stays a float when the
/// page sends `32` for `32.0`).
fn to_value(v: &Value, hint: Option<&toml_edit::Value>) -> Result<toml_edit::Value, String> {
    Ok(match v {
        Value::Null => return Err("null is only for removing a value".into()),
        Value::Bool(b) => (*b).into(),
        Value::Number(n) => {
            let wants_float = matches!(hint, Some(toml_edit::Value::Float(_)));
            match (n.as_i64(), n.as_f64()) {
                (Some(i), _) if !wants_float => i.into(),
                (_, Some(f)) => f.into(),
                _ => return Err(format!("{n} is not a number TOML can hold")),
            }
        }
        Value::String(s) => s.as_str().into(),
        Value::Array(a) => {
            let mut arr = Array::new();
            for x in a {
                arr.push(to_value(x, None)?);
            }
            toml_edit::Value::Array(arr)
        }
        Value::Object(m) => {
            let mut t = InlineTable::new();
            for (k, x) in m {
                if !x.is_null() {
                    t.insert(k, to_value(x, None)?);
                }
            }
            toml_edit::Value::InlineTable(t)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = r#"# the router (a comment that must stay)
version = 1

[servers.gpu]
title = "GPU host"   # trailing comment
host = "gpu-host"
exec = "ssh"
ssh = { user = "abhoth-exec", key = "secret:ssh_gpu" }

[cards.A]
server = "gpu"
vram_gib = 32.0

[layouts.x2]
title = "Both"
cards.A = { engine = "ninfer", profile = "4lane", model = "dense" }
cards.B = "same"
"#;

    fn edit(path: &[&str], value: Value) -> Edit {
        Edit {
            path: path.iter().map(|s| s.to_string()).collect(),
            value,
        }
    }

    fn parsed(t: &str) -> toml::Value {
        toml::from_str(t).unwrap()
    }

    #[test]
    fn a_field_change_keeps_comments_and_everything_else() {
        let out = apply_edits(
            TEXT,
            &[edit(&["servers", "gpu", "title"], json!("Big box"))],
        )
        .unwrap();
        assert!(out.contains("# the router (a comment that must stay)"));
        assert!(
            out.contains("title = \"Big box\"   # trailing comment"),
            "{out}"
        );
        assert_eq!(out.lines().count(), TEXT.lines().count());
    }

    #[test]
    fn a_whole_object_merges_and_drops_what_it_left_out() {
        let mut server = serde_json::to_value(&parsed(TEXT)["servers"]["gpu"]).unwrap();
        server["host"] = json!("gpu-2");
        server["ssh"]["port"] = json!(2222);
        server.as_object_mut().unwrap().remove("exec");
        let out = apply_edits(TEXT, &[edit(&["servers", "gpu"], server)]).unwrap();
        let v = parsed(&out);
        assert_eq!(v["servers"]["gpu"]["host"].as_str(), Some("gpu-2"));
        assert_eq!(v["servers"]["gpu"]["ssh"]["port"].as_integer(), Some(2222));
        assert_eq!(
            v["servers"]["gpu"]["ssh"]["user"].as_str(),
            Some("abhoth-exec")
        );
        assert!(v["servers"]["gpu"].get("exec").is_none());
        assert!(
            out.contains("# trailing comment"),
            "untouched keys keep their decor"
        );
    }

    #[test]
    fn floats_stay_floats_and_new_tables_read_well() {
        let card = json!({"server": "gpu", "vram_gib": 24, "label": "new"});
        let out = apply_edits(
            TEXT,
            &[
                edit(&["cards", "A", "vram_gib"], json!(16)),
                edit(&["cards", "B"], card),
                edit(
                    &["engines", "ninfer"],
                    json!({"title": "ninfer", "port": {"A": 8094}, "profiles": {"4lane": {"lanes": 4, "vram_gib": 7.5}}}),
                ),
            ],
        )
        .unwrap();
        assert!(out.contains("vram_gib = 16.0"), "{out}");
        assert!(out.contains("[cards.B]"), "{out}");
        assert!(out.contains("[engines.ninfer]"), "{out}");
        assert!(out.contains("port = { A = 8094 }"), "{out}");
        assert!(
            out.contains("profiles.4lane = { lanes = 4, vram_gib = 7.5 }"),
            "{out}"
        );
        let v = parsed(&out);
        assert_eq!(
            v["engines"]["ninfer"]["profiles"]["4lane"]["lanes"].as_integer(),
            Some(4)
        );
    }

    #[test]
    fn removing_and_layout_cards() {
        let out = apply_edits(
            TEXT,
            &[
                edit(&["cards", "A"], Value::Null),
                edit(
                    &["layouts", "x2", "cards", "B"],
                    json!({"engine": "ninfer", "profile": "2lane", "model": "small"}),
                ),
                edit(&["nothing", "here"], Value::Null),
            ],
        )
        .unwrap();
        let v = parsed(&out);
        assert!(v.get("cards").and_then(|c| c.get("A")).is_none());
        assert_eq!(
            v["layouts"]["x2"]["cards"]["B"]["profile"].as_str(),
            Some("2lane")
        );
        assert!(out.contains("cards.B = { engine = \"ninfer\""), "{out}");
    }

    #[test]
    fn bad_input_is_refused() {
        assert!(apply_edits("x = [", &[edit(&["a"], json!(1))]).is_err());
        assert!(apply_edits(TEXT, &[edit(&["version", "x"], json!(1))]).is_err());
        assert!(parse_edits(&json!({"edits": []})).is_err());
        assert!(parse_edits(&json!({"edits": [{"path": [], "value": 1}]})).is_err());
        assert!(parse_edits(&json!({"edits": [{"path": ["a", ""], "value": 1}]})).is_err());
        assert_eq!(
            parse_edits(&json!({"edits": [{"path": ["a"]}]})).unwrap()[0].value,
            Value::Null
        );
    }

    #[test]
    fn config_as_data() {
        let doc = ConfigDoc {
            rev: "r1".into(),
            path: "router.toml".into(),
            text: TEXT.into(),
            error: None,
        };
        let v = config_json(&doc);
        assert_eq!(v["data"]["cards"]["A"]["vram_gib"], json!(32.0));
        assert_eq!(v["rev"], "r1");
        assert!(v["parseError"].is_null());
    }
}
