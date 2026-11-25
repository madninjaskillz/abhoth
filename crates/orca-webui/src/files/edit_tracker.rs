//! `EditTracker`: follows a project's live events and keeps the "being edited" markers for its
//! file tree. A file-changing tool call in flight marks its paths `editing`; when it finishes
//! they become `edited` until the next turn starts.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use orca_core::live::{LiveEvent, LiveEventBus, Subscription, types};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::paths;

/// Which files the project's agent is writing now, and which it wrote this turn (full paths).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EditMarkers {
    pub editing: Vec<String>,
    pub edited: Vec<String>,
}

/// Tools that change files (built-in and Pi names).
pub const FILE_TOOLS: &[&str] = &[
    "write_file",
    "edit_file",
    "multi_edit",
    "apply_patch",
    "delete_file",
    "move_file",
    "rename_file",
    "copy_file",
    "create_directory",
    "mkdir",
    "write",
    "edit",
];

const PATH_KEYS: &[&str] = &[
    "path",
    "file_path",
    "filepath",
    "file",
    "filename",
    "source",
    "destination",
    "target",
    "dest",
    "new_path",
    "old_path",
];

fn is_file_tool(name: &str) -> bool {
    FILE_TOOLS.iter().any(|t| t.eq_ignore_ascii_case(name))
}

fn is_path_key(name: &str) -> bool {
    PATH_KEYS.iter().any(|k| k.eq_ignore_ascii_case(name))
}

#[derive(Default)]
struct State {
    cwd: String,
    in_flight: Vec<(String, Vec<String>)>,
    unpaired: HashMap<String, VecDeque<String>>,
    edited: Vec<String>,
    synthetic_ids: u64,
}

impl State {
    fn build(&self) -> EditMarkers {
        let mut editing: Vec<String> = Vec::new();
        for (_, paths) in &self.in_flight {
            for p in paths {
                if !editing.iter().any(|e| paths::eq(e, p)) {
                    editing.push(p.clone());
                }
            }
        }
        let edited = self
            .edited
            .iter()
            .filter(|p| !editing.iter().any(|e| paths::eq(e, p)))
            .cloned()
            .collect();
        EditMarkers { editing, edited }
    }

    fn add_edited(&mut self, p: String) {
        if !self.edited.iter().any(|e| paths::eq(e, &p)) {
            self.edited.push(p);
        }
    }

    fn full(&self, path: &str) -> Option<String> {
        let p = path.trim().trim_matches('"');
        if p.is_empty() {
            return None;
        }
        let joined = if Path::new(p).is_absolute() {
            p.to_string()
        } else {
            Path::new(&self.cwd).join(p).to_string_lossy().to_string()
        };
        Some(paths::normalize(&joined).to_string_lossy().to_string())
    }

    /// Returns the new markers when they changed.
    fn on_event(&mut self, ev: &LiveEvent) -> Option<EditMarkers> {
        match ev.kind.as_str() {
            types::TURN_START => {
                if !self.edited.is_empty() || !self.in_flight.is_empty() {
                    self.edited.clear();
                    self.in_flight.clear();
                    self.unpaired.clear();
                    return Some(self.build());
                }
                None
            }
            types::TOOL_CALL => {
                let (id, name, args) = read_call(&ev.data);
                let name = name.filter(|n| is_file_tool(n))?;
                let full: Vec<String> = paths_from_args(&name, args.as_deref())
                    .iter()
                    .filter_map(|p| self.full(p))
                    .collect();
                if full.is_empty() {
                    return None;
                }
                let id = id.unwrap_or_else(|| {
                    self.synthetic_ids += 1;
                    format!("x{}", self.synthetic_ids)
                });
                self.in_flight.retain(|(k, _)| *k != id);
                self.in_flight.push((id.clone(), full));
                self.unpaired
                    .entry(name.to_lowercase())
                    .or_default()
                    .push_back(id);
                Some(self.build())
            }
            types::TOOL_RESULT => {
                let (id, name, _) = read_call(&ev.data);
                let name = name.filter(|n| is_file_tool(n))?;
                let id = match id.filter(|i| self.in_flight.iter().any(|(k, _)| k == i)) {
                    Some(i) => i,
                    None => self.unpaired.get_mut(&name.to_lowercase())?.pop_front()?,
                };
                let pos = self.in_flight.iter().position(|(k, _)| *k == id)?;
                let (_, done) = self.in_flight.remove(pos);
                for p in done {
                    self.add_edited(p);
                }
                Some(self.build())
            }
            types::TURN_END => {
                // A cancelled turn can leave calls without results: they are not being edited any more.
                if self.in_flight.is_empty() {
                    return None;
                }
                let all: Vec<String> = self.in_flight.drain(..).flat_map(|(_, p)| p).collect();
                for p in all {
                    self.add_edited(p);
                }
                self.unpaired.clear();
                Some(self.build())
            }
            _ => None,
        }
    }
}

fn read_call(data: &Value) -> (Option<String>, Option<String>, Option<String>) {
    let s = |k: &str| data.get(k).and_then(Value::as_str).map(str::to_string);
    (s("id"), s("name"), s("args"))
}

fn json_path_key() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#""(\w+)"\s*:\s*"((?:[^"\\]|\\.)*)""#).unwrap())
}

fn patch_file() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"(?m)^(?:\*\*\* (?:Add|Update|Delete) File:|\+\+\+ b/|\*\*\* Move to:)[ \t]*(.+?)\r?$",
        )
        .unwrap()
    })
}

/// `Regex.Unescape` for the JSON escapes a cut-off argument string can hold.
fn unescape(s: &str) -> String {
    serde_json::from_str::<String>(&format!("\"{s}\"")).unwrap_or_else(|_| s.to_string())
}

/// The paths a file tool's arguments name (relative paths as given).
pub fn paths_from_args(tool: &str, args: Option<&str>) -> Vec<String> {
    let Some(args) = args.filter(|a| !a.trim().is_empty()) else {
        return vec![];
    };
    let mut out = Vec::new();
    let Ok(doc) = serde_json::from_str::<Value>(args) else {
        // The event's arguments are cut at a few KB (a big write_file): find the path keys by hand.
        for m in json_path_key().captures_iter(args) {
            if is_path_key(&m[1]) {
                out.push(unescape(&m[2]));
            }
        }
        return out;
    };
    let Some(obj) = doc.as_object() else {
        return out;
    };
    for (name, value) in obj {
        match value {
            Value::String(s) if is_path_key(name) => {
                if !s.is_empty() {
                    out.push(s.clone());
                }
            }
            Value::Array(items) if name.eq_ignore_ascii_case("files") => {
                for item in items {
                    match item {
                        Value::String(s) if !s.is_empty() => out.push(s.clone()),
                        Value::Object(o) => {
                            for key in PATH_KEYS {
                                if let Some(Value::String(s)) = o.get(*key)
                                    && !s.is_empty()
                                {
                                    out.push(s.clone());
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Value::String(s) if tool.eq_ignore_ascii_case("apply_patch") => {
                for m in patch_file().captures_iter(s) {
                    out.push(m[1].trim().to_string());
                }
            }
            _ => {}
        }
    }
    out
}

type ChangedHandler = Arc<dyn Fn(EditMarkers) + Send + Sync>;

/// The tracker for one project's bus. Dropping it unsubscribes.
pub struct EditTracker {
    state: Arc<Mutex<State>>,
    on_changed: Arc<Mutex<Option<ChangedHandler>>>,
    _subscription: Subscription,
}

impl EditTracker {
    pub fn new(bus: &Arc<LiveEventBus>, working_directory: &str) -> Self {
        let state = Arc::new(Mutex::new(State {
            cwd: working_directory.to_string(),
            ..State::default()
        }));
        let on_changed: Arc<Mutex<Option<ChangedHandler>>> = Arc::new(Mutex::new(None));
        let (s2, h2) = (state.clone(), on_changed.clone());
        let sub = bus.subscribe(move |ev| {
            let changed = s2.lock().unwrap().on_event(ev);
            if let Some(m) = changed
                && let Some(h) = h2.lock().unwrap().clone()
            {
                h(m);
            }
        });
        Self {
            state,
            on_changed,
            _subscription: sub,
        }
    }

    /// Called (on the publisher's thread; only enqueue work) when the markers change.
    pub fn set_on_changed(&self, handler: impl Fn(EditMarkers) + Send + Sync + 'static) {
        *self.on_changed.lock().unwrap() = Some(Arc::new(handler));
    }

    pub fn snapshot(&self) -> EditMarkers {
        self.state.lock().unwrap().build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn markers_follow_calls_results_and_turns() {
        let bus = Arc::new(LiveEventBus::new());
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();
        let tracker = EditTracker::new(&bus, &cwd);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        tracker.set_on_changed(move |m| s2.lock().unwrap().push(m));
        bus.publish_turn_start(0, "web");
        bus.publish_tool_call("write_file", r#"{"path":"a.txt","content":"x"}"#);
        bus.publish_tool_call("read_file", r#"{"path":"b.txt"}"#);
        let full = paths::normalize(&format!("{cwd}/a.txt"))
            .to_string_lossy()
            .to_string();
        assert_eq!(tracker.snapshot().editing, vec![full.clone()]);
        bus.publish_tool_result("write_file", "ok", false, Some(1));
        assert_eq!(
            tracker.snapshot(),
            EditMarkers {
                editing: vec![],
                edited: vec![full]
            }
        );
        bus.publish_turn_start(1, "web");
        assert_eq!(tracker.snapshot(), EditMarkers::default());
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[test]
    fn paths_come_from_keys_arrays_patches_and_cut_off_json() {
        assert_eq!(
            paths_from_args("move_file", Some(r#"{"source":"a","destination":"b"}"#)),
            vec!["a", "b"]
        );
        assert_eq!(
            paths_from_args(
                "multi_edit",
                Some(&json!({"files": ["x", {"path": "y"}]}).to_string())
            ),
            vec!["x", "y"]
        );
        assert_eq!(
            paths_from_args(
                "apply_patch",
                Some(
                    &json!({"patch": "*** Update File: src/a.rs\n@@\n*** Add File: b.rs\n"})
                        .to_string()
                )
            ),
            vec!["src/a.rs", "b.rs"]
        );
        assert_eq!(
            paths_from_args("write_file", Some(r#"{"path":"dir\\f.txt","content":"aaa"#)),
            vec!["dir\\f.txt"]
        );
    }
}
