//! Translates a bridge CLI's tool names and arguments into OpenOrca's (`BridgeTools`), so tool
//! cards render like native ones, permission rules apply and file-edit markers light up.

use serde_json::{Map, Value};

use super::json::{raw_text, to_compact_dotnet};
use super::paths;

/// The name OpenOrca's in-process MCP server has in Claude Code.
pub const HOST_MCP_SERVER: &str = "openorca";

/// How Claude Code names the host MCP server's tools ("mcp__openorca__ask_user").
pub const HOST_MCP_PREFIX: &str = "mcp__openorca__";

fn claude_name(name: &str) -> Option<&'static str> {
    Some(match name {
        "Bash" | "PowerShell" => "bash",
        "Read" => "read_file",
        "Write" => "write_file",
        "Edit" | "NotebookEdit" => "edit_file",
        "MultiEdit" => "multi_edit",
        "Glob" => "glob",
        "Grep" => "grep",
        "LS" => "list_directory",
        "WebFetch" => "web_fetch",
        "WebSearch" => "web_search",
        "TodoWrite" => "task_list",
        "Task" | "Agent" => "spawn_agent",
        _ => return None,
    })
}

/// OpenOrca tool names that change files (`FileWriteTools`, case-insensitive).
pub const FILE_WRITE_TOOLS: [&str; 4] = ["write_file", "edit_file", "multi_edit", "apply_patch"];

const READ_ONLY_TOOLS: [&str; 7] = [
    "read_file",
    "glob",
    "grep",
    "list_directory",
    "web_search",
    "task_list",
    "ask_user",
];

pub fn is_file_write_tool(name: &str) -> bool {
    FILE_WRITE_TOOLS
        .iter()
        .any(|t| t.eq_ignore_ascii_case(name))
}

/// OpenOrca's name for a Claude Code tool ("Edit" -> "edit_file"); unknown tools keep theirs.
pub fn from_claude(name: &str) -> String {
    if let Some(mapped) = claude_name(name) {
        return mapped.to_string();
    }
    name.strip_prefix(HOST_MCP_PREFIX)
        .unwrap_or(name)
        .to_string()
}

/// The permission risk level OpenOrca's auto-approve settings use for a tool.
pub fn risk_of(open_orca_name: &str) -> &'static str {
    if READ_ONLY_TOOLS
        .iter()
        .any(|t| t.eq_ignore_ascii_case(open_orca_name))
    {
        "ReadOnly"
    } else if is_file_write_tool(open_orca_name) {
        "Moderate"
    } else {
        "Dangerous"
    }
}

/// `AsString(JsonNode)`: a string value's text, any other node as JSON, `None` for null.
fn as_string(node: Option<&Value>) -> Option<String> {
    match node? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(to_compact_dotnet(other)),
    }
}

/// `JsonObject[name]`: missing and JSON null are both null.
fn node<'a>(obj: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    obj.get(name).filter(|v| !v.is_null())
}

/// ask_user arguments as OpenOrca's tool takes them ({question, options}). Models trained on
/// Claude Code's AskUserQuestion send {questions:[{question, options:[{label}]}]}, sometimes
/// as a JSON string; the first question is used.
pub fn normalize_ask_user_args(args_json: &str) -> String {
    let Ok(node_value) = serde_json::from_str::<Value>(args_json) else {
        return args_json.to_string();
    };
    let Some(obj) = node_value.as_object() else {
        return args_json.to_string();
    };
    if obj.contains_key("question") {
        return args_json.to_string();
    }
    let mut questions = node(obj, "questions").cloned();
    if let Some(Value::String(text)) = &questions {
        match serde_json::from_str::<Value>(text) {
            Ok(v) => questions = Some(v),
            Err(_) => return args_json.to_string(),
        }
    }
    let first = match questions.as_ref().and_then(Value::as_array) {
        Some(list) if !list.is_empty() => match list[0].as_object() {
            Some(first) => first.clone(),
            None => return args_json.to_string(),
        },
        _ => return args_json.to_string(),
    };
    let mut options = Vec::new();
    if let Some(Value::Array(opts)) = node(&first, "options") {
        for o in opts {
            let label = match o {
                Value::Object(oo) => as_string(node(oo, "label").or_else(|| node(oo, "text"))),
                other => as_string(Some(other)),
            };
            if let Some(label) = label.filter(|l| !l.trim().is_empty()) {
                options.push(Value::String(label));
            }
        }
    }
    let mut out = Map::new();
    out.insert(
        "question".into(),
        Value::String(
            as_string(node(&first, "question").or_else(|| node(&first, "header")))
                .unwrap_or_default(),
        ),
    );
    out.insert("options".into(), Value::Array(options));
    to_compact_dotnet(&Value::Object(out))
}

/// The arguments with OpenOrca's keys added: "path" (relative to `cwd` when inside it) for
/// file_path / notebook_path, and a Bash description as "_reason". Original keys are kept.
/// `None` is a missing input (`JsonValueKind.Undefined`).
pub fn normalize_args(input: Option<&Value>, cwd: Option<&str>) -> String {
    let Some(input) = input else {
        return "{}".into();
    };
    let Value::Object(obj) = input else {
        return raw_text(input);
    };
    let mut obj = obj.clone();
    if !obj.contains_key("path")
        && let Some(file) = file_path_of(Some(input))
    {
        obj.insert("path".into(), Value::String(relative(&file, cwd)));
    }
    if !obj.contains_key("_reason")
        && let Some(Value::String(desc)) = obj.get("description")
    {
        let desc = desc.clone();
        obj.insert("_reason".into(), Value::String(desc));
    }
    to_compact_dotnet(&Value::Object(obj))
}

/// The file a tool call targets (file_path, notebook_path or path), or `None`.
pub fn file_path_of(input: Option<&Value>) -> Option<String> {
    let obj = input?.as_object()?;
    ["file_path", "notebook_path", "path"]
        .iter()
        .find_map(|k| {
            obj.get(*k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .map(str::to_string)
}

/// `path` relative to `cwd` when it lies inside it, else unchanged.
pub fn relative(path: &str, cwd: Option<&str>) -> String {
    let Some(cwd) = cwd.filter(|c| !c.is_empty()) else {
        return path.to_string();
    };
    // Plain prefix first: works for Windows paths on any OS (recorded fixtures, WSL hosts).
    let root = cwd.trim_end_matches(['\\', '/']);
    let drive_letter = root.len() >= 2 && root.as_bytes()[1] == b':';
    let ignore_case = cfg!(windows) || drive_letter;
    if path.len() > root.len() + 1 && path.is_char_boundary(root.len()) {
        let head = &path[..root.len()];
        let same = if ignore_case {
            head.to_lowercase() == root.to_lowercase()
        } else {
            head == root
        };
        if same && matches!(path.as_bytes()[root.len()], b'\\' | b'/') {
            return path[root.len() + 1..].to_string();
        }
    }
    if !paths::is_path_fully_qualified(path) {
        return path.to_string();
    }
    let rel = paths::get_relative_path(cwd, path);
    if rel.starts_with("..") || paths::is_path_fully_qualified(&rel) {
        path.to_string()
    } else {
        rel
    }
}

/// A unified-diff style body for an edit's result ("Edited path  +a −r" then "@@" hunks),
/// built from Claude Code's structuredPatch, or `None` when there is none.
pub fn diff_from_structured_patch(
    tool_use_result: Option<&Value>,
    display_path: &str,
) -> Option<String> {
    let patch = tool_use_result?
        .as_object()?
        .get("structuredPatch")?
        .as_array()
        .filter(|p| !p.is_empty())?;
    let mut body = String::new();
    let (mut added, mut removed) = (0, 0);
    for hunk in patch {
        if !hunk.is_object() {
            continue;
        }
        body.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            int_or_zero(hunk, "oldStart"),
            int_or_zero(hunk, "oldLines"),
            int_or_zero(hunk, "newStart"),
            int_or_zero(hunk, "newLines")
        ));
        let Some(lines) = hunk.get("lines").and_then(Value::as_array) else {
            continue;
        };
        for line in lines {
            let text = line.as_str().unwrap_or("");
            if text.starts_with('\\') {
                continue; // "\ No newline at end of file"
            }
            if text.starts_with('+') {
                added += 1;
            } else if text.starts_with('-') {
                removed += 1;
            }
            body.push_str(text);
            body.push('\n');
        }
    }
    Some(format!(
        "Edited {display_path}  +{added} \u{2212}{removed}\n{}",
        body.trim_end_matches('\n')
    ))
}

fn int_or_zero(obj: &Value, name: &str) -> i32 {
    super::json::int_of(obj, name).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_mcp_tool_names_map_to_open_orca_names() {
        assert_eq!(from_claude("mcp__openorca__ask_user"), "ask_user");
        assert_eq!(risk_of("ask_user"), "ReadOnly");
        assert_eq!(from_claude("mcp__other__x"), "mcp__other__x");
    }

    #[test]
    fn ask_user_args_normalized() {
        let cases = [
            (r#"{"question":"Q?","options":["A","B"]}"#, "Q?", "A,B"),
            (
                r#"{"questions":[{"question":"Q?","options":[{"label":"A"},{"label":"B"}]}]}"#,
                "Q?",
                "A,B",
            ),
            (
                r#"{"questions":"[{\"question\":\"Q?\",\"options\":[\"A\",\"B\"]}]"}"#,
                "Q?",
                "A,B",
            ),
        ];
        for (input, question, options) in cases {
            let v: Value = serde_json::from_str(&normalize_ask_user_args(input)).unwrap();
            assert_eq!(v["question"], question);
            let opts: Vec<&str> = v["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| o.as_str().unwrap())
                .collect();
            assert_eq!(opts.join(","), options);
        }
    }

    #[test]
    fn relative_inside_and_outside() {
        assert_eq!(
            relative(r"C:\work\proj\a\b.txt", Some(r"C:\work\proj")),
            r"a\b.txt"
        );
        assert_eq!(relative(r"c:\WORK\proj\x", Some(r"C:\work\proj\")), "x");
        assert_eq!(relative("rel/x", Some(r"C:\work\proj")), "rel/x");
        assert_eq!(relative(r"C:\other\x", None), r"C:\other\x");
        if cfg!(windows) {
            assert_eq!(
                relative(r"C:\other\x", Some(r"C:\work\proj")),
                r"C:\other\x"
            );
        }
    }

    #[test]
    fn normalize_args_adds_path_and_reason() {
        let input: Value = serde_json::from_str(
            r#"{"file_path":"C:\\work\\proj\\hello.txt","description":"Say <hi>"}"#,
        )
        .unwrap();
        assert_eq!(
            normalize_args(Some(&input), Some(r"C:\work\proj")),
            r#"{"file_path":"C:\\work\\proj\\hello.txt","description":"Say \u003Chi\u003E","path":"hello.txt","_reason":"Say \u003Chi\u003E"}"#
        );
        assert_eq!(normalize_args(None, None), "{}");
        assert_eq!(normalize_args(Some(&Value::from(3)), None), "3");
    }

    #[test]
    fn structured_patch_diff() {
        let details: Value = serde_json::from_str(
            r#"{"structuredPatch":[{"oldStart":1,"oldLines":1,"newStart":1,"newLines":2,"lines":["-hi","+hi","+there","\\ No newline at end of file"]}]}"#,
        )
        .unwrap();
        assert_eq!(
            diff_from_structured_patch(Some(&details), "hello.txt").unwrap(),
            "Edited hello.txt  +2 \u{2212}1\n@@ -1,1 +1,2 @@\n-hi\n+hi\n+there"
        );
        assert!(diff_from_structured_patch(None, "x").is_none());
    }
}
