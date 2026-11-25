//! Port of `ToolCallParser`: tool calls written as text by models that don't use native
//! function calling (`<tool_call>` tags and their variants, JSON in code fences, bare JSON,
//! `<function=name>` tags), JSON repair for common local-model slips, and the nudge heuristic
//! for replies that look like an action the model forgot to make with a tool.
//!
//! Regexes map one to one onto the C# ones (the `regex` crate's leftmost-first semantics give
//! the same matches as .NET backtracking for patterns without lookaround). The two C# patterns
//! with lookaround are hand-written: [`has_file_path`] (`FilePathRegex`, negative lookahead)
//! and [`convert_single_quotes`] (the single-quote lookbehind/lookahead in `TryRepairJson`).

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use regex::Regex;
use serde_json::{Map, Value};

use super::llm_tags::{
    ASSISTANT_BLOCK, ASSISTANT_TAG, THINK_BLOCK, THINK_UNCLOSED, TOOL_CALL_OPEN,
    contains_ordinal_ignore_case,
};
use super::run_evidence::looks_like_command_output;

/// A tool call recovered from text (C# `FunctionCallContent`).
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedToolCall {
    /// `parsed_` + 32 lowercase hex digits (C# `parsed_{Guid:N}`).
    pub id: String,
    /// Lowercased tool name.
    pub name: String,
    /// The arguments object in source key order. Empty when the call had no `arguments` /
    /// `parameters` object (C# passes `null` arguments there).
    pub arguments: Map<String, Value>,
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid tool-call regex")
}

/// Tagged tool calls (patterns 1-3 plus `<function_call>`).
static TAG_PATTERNS: LazyLock<[Regex; 4]> = LazyLock::new(|| {
    [
        re(r"(?is)<tool_call>\s*(.*?)\s*</tool_call>"),
        re(r"(?is)<\|tool_call\|>\s*(.*?)\s*<\|/tool_call\|>"),
        re(r"(?is)\[TOOL_CALL\]\s*(.*?)\s*\[/TOOL_CALL\]"),
        re(r"(?is)<function_call>\s*(.*?)\s*</function_call>"),
    ]
});
static CODE_FENCE_JSON: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?s)```(?:json)?\s*\n?\s*(\{.*?\})\s*\n?\s*```"));
static UNCLOSED_TOOL_CALL: LazyLock<Regex> = LazyLock::new(|| re(r"(?is)<tool_call>\s*(.*?)\s*$"));
/// One level of nested braces in the arguments (e.g. `{"opts": {"key": "val"}}`).
static BARE_JSON_TOOL_CALL: LazyLock<Regex> = LazyLock::new(|| {
    re(r#"(?s)\{\s*"name"\s*:\s*"[^"]+"[^{}]*"arguments"\s*:\s*\{(?:[^{}]|\{[^{}]*\})*\}\s*\}"#)
});
static WRAPPED_TOOL_CALL: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(?s)\{\s*"tool_call"\s*:\s*(\{.*?\})\s*\}"#));
static FUNCTION_EQUALS_TAG: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?is)<function=(\w+)>\s*(\{.*?\})\s*</function>"));

static CODE_BLOCK_TOOL_JSON: LazyLock<Regex> =
    LazyLock::new(|| re(r#"```(?:json)?\s*\n?\s*\{[^}]*"name"\s*:"#));
static ACTION_WORDS: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b(creat|writ|save|generat|implement|add|make|updat|modif)\w*\b"));
static FENCED_CODE: LazyLock<Regex> = LazyLock::new(|| re(r"(?s)```.*?```|```.*$"));
static FENCED_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?s)```([\w+#.-]*)[^\n]*\n(.*?)(?:```|\z)"));
static TRAILING_COMMA: LazyLock<Regex> = LazyLock::new(|| re(r",\s*([}\]])"));
static WORD_CHAR: LazyLock<Regex> = LazyLock::new(|| re(r"\A\w\z"));

fn is_null_or_white_space(s: &str) -> bool {
    s.trim().is_empty()
}

/// `ParseToolCallsFromText`. Searches the text outside `<think>` (and with `<assistant>` tags
/// removed) first and falls back to the full text.
pub fn parse_tool_calls_from_text(text: &str) -> Vec<ParsedToolCall> {
    let mut results = Vec::new();
    if is_null_or_white_space(text) {
        return results;
    }

    let stripped = THINK_BLOCK.replace_all(text, "").trim().to_string();
    let stripped = THINK_UNCLOSED.replace_all(&stripped, "").trim().to_string();
    let stripped = ASSISTANT_BLOCK
        .replace_all(&stripped, "")
        .trim()
        .to_string();
    let stripped = ASSISTANT_TAG.replace_all(&stripped, "").trim().to_string();

    let mut variants: Vec<&str> = Vec::with_capacity(2);
    if !is_null_or_white_space(&stripped) {
        variants.push(&stripped);
    }
    variants.push(text);

    let mut candidates: Vec<String> = Vec::new();

    for variant in &variants {
        for pattern in TAG_PATTERNS.iter() {
            for caps in pattern.captures_iter(variant) {
                candidates.push(caps[1].trim().to_string());
            }
        }
        for caps in CODE_FENCE_JSON.captures_iter(variant) {
            candidates.push(caps[1].trim().to_string());
        }
        for caps in FUNCTION_EQUALS_TAG.captures_iter(variant) {
            let func_name = &caps[1];
            let args_json = caps[2].trim();
            candidates.push(format!(
                r#"{{"name": "{func_name}", "arguments": {args_json}}}"#
            ));
        }
        // Unclosed <tool_call> (token limit mid-generation), only when nothing closed matched.
        if candidates.is_empty()
            && let Some(caps) = UNCLOSED_TOOL_CALL.captures(variant)
        {
            candidates.push(caps[1].trim().to_string());
        }
        // Found something outside <think>: don't search the full text (avoids duplicates).
        if !candidates.is_empty() {
            break;
        }
    }

    // Bare JSON that looks like a tool call, only when nothing else matched.
    if candidates.is_empty() {
        for variant in &variants {
            for m in BARE_JSON_TOOL_CALL.find_iter(variant) {
                candidates.push(m.as_str().trim().to_string());
            }
            for caps in WRAPPED_TOOL_CALL.captures_iter(variant) {
                candidates.push(caps[1].trim().to_string());
            }
            if !candidates.is_empty() {
                break;
            }
        }
    }

    // A candidate may hold several JSON objects (several calls in one <tool_call> block).
    for candidate in &candidates {
        for json in split_json_objects(candidate) {
            try_parse_tool_call(&json, &mut results);
        }
    }

    results
}

fn try_parse_tool_call(json: &str, results: &mut Vec<ParsedToolCall>) {
    let root = match serde_json::from_str::<Value>(json) {
        Ok(v) => v,
        Err(_) => {
            match try_repair_json(json).and_then(|r| serde_json::from_str::<Value>(&r).ok()) {
                Some(v) => v,
                None => return,
            }
        }
    };
    // C# calls TryGetProperty on the root, which throws for a non-object root; every
    // candidate reaching here starts with '{', so skipping is the practical equivalent.
    let Value::Object(root) = root else {
        return;
    };

    let mut name: Option<&str> = None;
    let mut arguments: Option<&Map<String, Value>> = None;

    if let Some(name_el) = root.get("name") {
        // C# GetString(): string -> value, null -> null; any other kind throws
        // InvalidOperationException. Treated as "no name" here (the call is skipped).
        name = name_el.as_str();
        if let Some(Value::Object(args)) = root.get("arguments") {
            arguments = Some(args);
        } else if let Some(Value::Object(params)) = root.get("parameters") {
            arguments = Some(params);
        }
    } else if let Some(Value::Object(func)) = root.get("function") {
        name = func.get("name").and_then(Value::as_str);
        if let Some(Value::Object(args)) = func.get("arguments") {
            arguments = Some(args);
        }
    }

    if let Some(name) = name
        && !name.is_empty()
    {
        results.push(ParsedToolCall {
            id: new_call_id(),
            // ToLowerInvariant is per UTF-16 unit, so no context-dependent (final sigma) rules.
            name: name.chars().flat_map(char::to_lowercase).collect(),
            arguments: arguments.map(parse_arguments).unwrap_or_default(),
        });
    }
}

/// `ParseArguments`. C# keeps strings/bools/null as CLR values and numbers/arrays/objects as
/// `JsonElement`s so re-serialization keeps their JSON types; a `serde_json::Value` already
/// does that, so this is a copy. Difference: C# re-serializes numbers with their source text,
/// serde_json normalizes them (`1e5` -> `100000.0`, `1.50` -> `1.5`); integers and plain
/// decimals round-trip unchanged.
pub fn parse_arguments(args: &Map<String, Value>) -> Map<String, Value> {
    args.clone()
}

fn new_call_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let half = |salt: u64| {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(n);
        h.write_u64(salt);
        h.write_u128(nanos);
        h.finish()
    };
    format!("parsed_{:016x}{:016x}", half(1), half(2))
}

/// `TryRepairJson`: fixes trailing commas, missing closing braces/brackets and single-quoted
/// JSON. Returns `None` when nothing changed, when the text ends inside a string (truncated
/// value: closing it would fabricate content), or when the repair still doesn't parse.
pub fn try_repair_json(json: &str) -> Option<String> {
    let original = json.trim();
    if original.is_empty() {
        return None;
    }
    let mut repaired = original.to_string();

    // Single -> double quotes only when there are no double quotes at all; otherwise
    // apostrophes inside real string values would get mangled.
    if !repaired.contains('"') {
        repaired = convert_single_quotes(&repaired);
    }

    repaired = TRAILING_COMMA.replace_all(&repaired, "$1").into_owned();

    let (unclosed_braces, unclosed_brackets, ends_inside_string) = scan_json_structure(&repaired);
    if ends_inside_string {
        return None;
    }
    for _ in 0..unclosed_braces.max(0) {
        repaired.push('}');
    }
    for _ in 0..unclosed_brackets.max(0) {
        repaired.push(']');
    }

    if repaired == original {
        return None;
    }
    serde_json::from_str::<Value>(&repaired).ok()?;
    Some(repaired)
}

fn is_ws(c: char) -> bool {
    c.is_whitespace()
}

/// Hand-written `Regex.Replace(s, @"(?<=[\[{,:\s])\'(.*?)\'(?=[\]},:}\s])", "\"$1\"")`.
///
/// Left to right, non-overlapping: a `'` preceded (in the original text, which is what .NET
/// lookbehind sees) by one of `[ { , :` or whitespace opens a match; the lazy `.*?` (no
/// newlines, no Singleline) stops at the first later `'` followed by one of `] } , :` or
/// whitespace. A `'` at the very end can't close (the lookahead needs a character). If no
/// closer is found before a newline or the end, that `'` is left alone and the scan moves on.
pub(crate) fn convert_single_quotes(s: &str) -> String {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let n = chars.len();
    let before_ok = |c: char| matches!(c, '[' | '{' | ',' | ':') || is_ws(c);
    let after_ok = |c: char| matches!(c, ']' | '}' | ',' | ':') || is_ws(c);

    let mut out = String::with_capacity(s.len());
    let mut copied = 0usize; // byte offset copied so far
    let mut pos = 0usize;
    while pos < n {
        if chars[pos].1 == '\'' && pos > 0 && before_ok(chars[pos - 1].1) {
            let mut closer = None;
            let mut j = pos + 1;
            while j < n {
                let c = chars[j].1;
                if c == '\n' {
                    break;
                }
                if c == '\'' && j + 1 < n && after_ok(chars[j + 1].1) {
                    closer = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(j) = closer {
                let open = chars[pos].0;
                let close = chars[j].0;
                out.push_str(&s[copied..open]);
                out.push('"');
                out.push_str(&s[open + 1..close]);
                out.push('"');
                copied = close + 1;
                pos = j + 1;
                continue;
            }
        }
        pos += 1;
    }
    out.push_str(&s[copied..]);
    out
}

/// Unclosed braces and brackets outside strings, and whether the text ends inside a string
/// (or right after a backslash in one).
fn scan_json_structure(text: &str) -> (i32, i32, bool) {
    let mut braces = 0i32;
    let mut brackets = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for c in text.chars() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => braces += 1,
            '}' => braces -= 1,
            '[' => brackets += 1,
            ']' => brackets -= 1,
            _ => {}
        }
    }
    (braces, brackets, in_string || escape)
}

/// `SplitJsonObjects`: one JSON object as-is, or each top-level `{...}` of concatenated
/// objects (brace depth outside strings).
pub fn split_json_objects(text: &str) -> Vec<String> {
    let mut results = Vec::new();
    if is_null_or_white_space(text) {
        return results;
    }
    if let Ok(Value::Object(_)) = serde_json::from_str::<Value>(text) {
        results.push(text.to_string());
        return results;
    }

    // Byte scan: every character that matters is ASCII, so the slice bounds are char bounds.
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    let mut start: Option<usize> = None;
    for (i, &ch) in bytes.iter().enumerate() {
        if escape {
            escape = false;
            continue;
        }
        if ch == b'\\' && in_string {
            escape = true;
            continue;
        }
        if ch == b'"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        if ch == b'{' {
            if depth == 0 {
                start = Some(i);
            }
            depth += 1;
        } else if ch == b'}' {
            depth -= 1;
            if depth == 0
                && let Some(s) = start.take()
            {
                results.push(text[s..=i].to_string());
            }
        }
    }
    results
}

/// `ShouldNudgeForToolCalls`: the reply looks like an action (tool-like JSON in a code block,
/// or a code block plus action words and a file path in the prose) without `<tool_call>` tags.
pub fn should_nudge_for_tool_calls(text: &str) -> bool {
    if contains_ordinal_ignore_case(text, TOOL_CALL_OPEN) {
        return false;
    }

    let action_text = THINK_BLOCK.replace_all(text, "").trim().to_string();
    let action_text = THINK_UNCLOSED
        .replace_all(&action_text, "")
        .trim()
        .to_string();
    if is_null_or_white_space(&action_text) {
        return false;
    }

    if CODE_BLOCK_TOOL_JSON.is_match(&action_text) {
        tracing::debug!("Nudge trigger: code block with tool-like JSON");
        return true;
    }

    // Action words and file paths must be in the PROSE, not inside code blocks; otherwise an
    // explanatory answer containing e.g. "world.update(dt)" triggers a nudge.
    let has_code_block = action_text.contains("```");
    let prose = if has_code_block {
        FENCED_CODE.replace_all(&action_text, " ").into_owned()
    } else {
        action_text.clone()
    };
    let has_action_words = ACTION_WORDS.is_match(&prose);
    let has_file_path = has_file_path(&prose);

    if has_code_block && has_action_words && has_file_path {
        // A wrap-up quoting test/command OUTPUT is reporting, not code to write with a tool.
        let mut blocks = FENCED_BLOCK.captures_iter(&action_text).peekable();
        if blocks.peek().is_some()
            && blocks.all(|b| {
                looks_like_command_output(
                    Some(b.get(1).map_or("", |m| m.as_str())),
                    b.get(2).map_or("", |m| m.as_str()),
                )
            })
        {
            tracing::debug!("No nudge: every code block is command/test output");
            return false;
        }
        tracing::debug!("Nudge trigger: code block + action words + file path");
        return true;
    }

    false
}

fn is_word(c: char) -> bool {
    let mut buf = [0u8; 4];
    WORD_CHAR.is_match(c.encode_utf8(&mut buf))
}

/// Hand-written `FilePathRegex.IsMatch`: `[\w./\\]+\.\w{1,5}\b(?!\s*\()`.
///
/// A match exists iff some `.` has a path character (`\w . / \`) right before it and is
/// followed by a run of 1-5 word characters that is the WHOLE word run (the `\b` after a word
/// character needs a non-word character or the end), and the run is not followed by optional
/// whitespace and `(`. Backtracking can always shrink `[\w./\\]+` to the single character
/// before the dot, so that is all the prefix needs. Hence `world.update(dt)` does not match
/// (6-letter run, and a call), `obj.run(x)` does not (call), but `a.b.update(x)` does: `a.b`
/// is followed by `.`, not `(`, exactly as .NET backtracks to it.
pub(crate) fn has_file_path(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let path_char = |c: char| c == '.' || c == '/' || c == '\\' || is_word(c);
    for p in 1..n {
        if chars[p] != '.' || !path_char(chars[p - 1]) {
            continue;
        }
        let mut k = 0;
        while p + 1 + k < n && k <= 5 && is_word(chars[p + 1 + k]) {
            k += 1;
        }
        if k == 0 || k > 5 {
            continue;
        }
        let mut q = p + 1 + k;
        while q < n && chars[q].is_whitespace() {
            q += 1;
        }
        if q < n && chars[q] == '(' {
            continue;
        }
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(text: &str) -> Vec<ParsedToolCall> {
        parse_tool_calls_from_text(text)
    }

    fn arg_str(call: &ParsedToolCall, key: &str) -> Option<String> {
        call.arguments.get(key).map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
    }

    // ── Pattern 1: <tool_call> tags ──

    #[test]
    fn parse_tool_call_tags_extracts_name_and_arguments() {
        let text = "I'll read the file for you.\n<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"/tmp/test.txt\"}}\n</tool_call>";
        let results = parse(text);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "read_file");
        assert_eq!(
            arg_str(&results[0], "path").as_deref(),
            Some("/tmp/test.txt")
        );
    }

    #[test]
    fn parse_multiple_tool_call_tags_extracts_all() {
        let text = "<tool_call>{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.txt\"}}</tool_call>\n<tool_call>{\"name\": \"write_file\", \"arguments\": {\"path\": \"b.txt\", \"content\": \"hello\"}}</tool_call>";
        let results = parse(text);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].name, "read_file");
        assert_eq!(results[1].name, "write_file");
    }

    // ── Pattern 2: <|tool_call|> pipe-delimited tags ──

    #[test]
    fn parse_pipe_delimited_tags_extracts_tool_call() {
        let text = "<|tool_call|>\n{\"name\": \"bash\", \"arguments\": {\"command\": \"ls -la\"}}\n<|/tool_call|>";
        let results = parse(text);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "bash");
    }

    // ── Pattern 3: [TOOL_CALL] bracket tags ──

    #[test]
    fn parse_bracket_tags_extracts_tool_call() {
        let text = "[TOOL_CALL]\n{\"name\": \"grep\", \"arguments\": {\"pattern\": \"TODO\", \"path\": \".\"}}\n[/TOOL_CALL]";
        let results = parse(text);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "grep");
    }

    // ── Pattern 4: JSON in code fences ──

    #[test]
    fn parse_json_code_fence_extracts_tool_call() {
        let text = "Let me search for that:\n```json\n{\"name\": \"grep\", \"arguments\": {\"pattern\": \"error\", \"path\": \"/var/log\"}}\n```";
        let results = parse(text);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "grep");
    }

    #[test]
    fn parse_code_fence_without_json_label_extracts_tool_call() {
        let text = "```\n{\"name\": \"list_files\", \"arguments\": {\"path\": \".\"}}\n```";
        let results = parse(text);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "list_files");
    }

    // ── Pattern 4b: Unclosed <tool_call> tag ──

    #[test]
    fn parse_unclosed_tool_call_tag_extracts_partial_tool_call() {
        let text = "<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"test.cs\"}}";
        let results = parse(text);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "read_file");
    }

    // ── Pattern 5: Bare JSON ──

    #[test]
    fn parse_bare_json_with_name_and_arguments_extracts_tool_call() {
        let results = parse(r#"{"name": "bash", "arguments": {"command": "echo hello"}}"#);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "bash");
    }

    // ── <think> tag handling ──

    #[test]
    fn parse_think_tags_stripped_before_parsing() {
        let text = "<think>Let me think about which tool to use... I'll use read_file.</think>\n<tool_call>{\"name\": \"read_file\", \"arguments\": {\"path\": \"config.json\"}}</tool_call>";
        let results = parse(text);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "read_file");
    }

    #[test]
    fn parse_unclosed_think_tag_stripped_before_parsing() {
        // The tool call is inside the unclosed think: found via the full-text fallback.
        let text = "<think>Still thinking about this...\n<tool_call>{\"name\": \"bash\", \"arguments\": {\"command\": \"pwd\"}}</tool_call>";
        let results = parse(text);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "bash");
    }

    // ── <assistant> tag stripping ──

    #[test]
    fn parse_assistant_tags_stripped_before_parsing() {
        let text = "<assistant>Some intro text</assistant>\n<tool_call>{\"name\": \"read_file\", \"arguments\": {\"path\": \"test.txt\"}}</tool_call>";
        let results = parse(text);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "read_file");
    }

    // ── "parameters" alias ──

    #[test]
    fn parse_parameters_alias_treated_as_arguments() {
        let text =
            r#"<tool_call>{"name": "read_file", "parameters": {"path": "test.txt"}}</tool_call>"#;
        let results = parse(text);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "read_file");
        assert_eq!(arg_str(&results[0], "path").as_deref(), Some("test.txt"));
    }

    // ── "function" wrapper format ──

    #[test]
    fn parse_function_wrapper_format_extracts_tool_call() {
        let text = r#"<tool_call>{"function": {"name": "bash", "arguments": {"command": "ls"}}}</tool_call>"#;
        let results = parse(text);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "bash");
    }

    // ── <function_call> tags ──

    #[test]
    fn parse_function_call_tags_extracts_tool_call() {
        let text =
            r#"<function_call>{"name": "grep", "arguments": {"pattern": "test"}}</function_call>"#;
        let results = parse(text);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "grep");
    }

    // ── Edge cases ──

    #[test]
    fn parse_empty_string_returns_empty() {
        assert!(parse("").is_empty());
    }

    // Parse_NullString_ReturnsEmpty: not portable, a Rust &str cannot be null.

    #[test]
    fn parse_whitespace_only_returns_empty() {
        assert!(parse("   \n  \t  ").is_empty());
    }

    #[test]
    fn parse_plain_text_no_tool_calls_returns_empty() {
        assert!(
            parse("Hello! I can help you with that. Let me explain how this works.").is_empty()
        );
    }

    #[test]
    fn parse_invalid_json_skips_gracefully() {
        assert!(parse("<tool_call>\n{this is not valid json at all}\n</tool_call>").is_empty());
    }

    #[test]
    fn parse_json_without_name_skips_gracefully() {
        assert!(
            parse("<tool_call>\n{\"arguments\": {\"path\": \"test.txt\"}}\n</tool_call>")
                .is_empty()
        );
    }

    #[test]
    fn parse_generates_unique_call_ids() {
        let text = "<tool_call>{\"name\": \"a\", \"arguments\": {}}</tool_call>\n<tool_call>{\"name\": \"b\", \"arguments\": {}}</tool_call>";
        let results = parse(text);
        assert_eq!(results.len(), 2);
        assert_ne!(results[0].id, results[1].id);
        assert!(results[0].id.starts_with("parsed_"));
        // Guid "N" format: 32 lowercase hex digits.
        let hex = &results[0].id["parsed_".len()..];
        assert_eq!(hex.len(), 32);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn parse_boolean_and_numeric_arguments_preserved() {
        let text = r#"<tool_call>{"name": "test", "arguments": {"flag": true, "count": 42, "label": null}}</tool_call>"#;
        let results = parse(text);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].arguments["flag"], Value::Bool(true));
        assert_eq!(arg_str(&results[0], "count").as_deref(), Some("42"));
        assert_eq!(results[0].arguments["label"], Value::Null);
    }

    // ── ShouldNudgeForToolCalls tests ──

    #[test]
    fn nudge_already_has_tool_call_tag_returns_false() {
        let text = r#"<tool_call>{"name": "bash", "arguments": {"command": "ls"}}</tool_call>"#;
        assert!(!should_nudge_for_tool_calls(text));
    }

    #[test]
    fn nudge_code_block_with_tool_like_json_returns_true() {
        let text = "I'll create a file for you. Here's what I would do:\n```json\n{\"name\": \"write_file\", \"arguments\": {\"path\": \"test.txt\"}}\n```";
        assert!(should_nudge_for_tool_calls(text));
    }

    #[test]
    fn nudge_code_block_with_action_words_and_file_path_returns_true() {
        let text = "I'll create the file src/test.cs with this content:\n```csharp\nConsole.WriteLine(\"hello\");\n```";
        assert!(should_nudge_for_tool_calls(text));
    }

    #[test]
    fn nudge_wrap_up_quoting_test_output_returns_false() {
        for block in [
            "```\n============ test session starts ============\ncollected 4 items\n\ntests/test_helpers.py ....  [100%]\n\n============ 4 passed in 0.02s ============\n```",
            "```text\nanything at all\n```",
            "```\nRan 3 tests in 0.001s\n\nOK\n```",
            "```console\n$ pytest\n4 passed\n```",
        ] {
            let text =
                format!("I updated src/helpers.py and the tests now pass:\n{block}\nAll done.");
            assert!(!should_nudge_for_tool_calls(&text), "{block}");
        }
    }

    #[test]
    fn nudge_output_block_plus_source_block_still_nudges() {
        let text = "I'll create src/helpers.py:\n```python\ndef f():\n    pass\n```\nThen:\n```\n4 passed\n```";
        assert!(should_nudge_for_tool_calls(text));
    }

    #[test]
    fn nudge_printed_shell_command_still_nudges() {
        let text =
            "Now update src/helpers.py by running:\n```bash\n$ sed -i 's/a/b/' src/helpers.py\n```";
        assert!(should_nudge_for_tool_calls(text));
    }

    #[test]
    fn nudge_plain_text_returns_false() {
        assert!(!should_nudge_for_tool_calls(
            "Sure, I can help you understand how that works."
        ));
    }

    #[test]
    fn nudge_empty_text_returns_false() {
        assert!(!should_nudge_for_tool_calls(""));
    }

    #[test]
    fn nudge_think_block_only_returns_false() {
        assert!(!should_nudge_for_tool_calls(
            "<think>I should use a tool here but I'll just think about it.</think>"
        ));
    }

    // ── ParseArguments tests ──

    fn obj(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            _ => unreachable!(),
        }
    }

    #[test]
    fn parse_arguments_string_value_returns_string() {
        let result = parse_arguments(&obj(json!({"key": "value"})));
        assert_eq!(result["key"], Value::String("value".into()));
    }

    #[test]
    fn parse_arguments_bool_value_returns_bool() {
        let result = parse_arguments(&obj(json!({"flag": true, "other": false})));
        assert_eq!(result["flag"], Value::Bool(true));
        assert_eq!(result["other"], Value::Bool(false));
    }

    #[test]
    fn parse_arguments_null_value_returns_null() {
        let result = parse_arguments(&obj(json!({"key": null})));
        assert_eq!(result["key"], Value::Null);
    }

    #[test]
    fn parse_arguments_number_value_round_trips_as_json_number() {
        let src: Map<String, Value> = serde_json::from_str(r#"{"line": 5, "ratio": 1.5}"#).unwrap();
        let result = parse_arguments(&src);
        let serialized = serde_json::to_string(&result).unwrap();
        let round: Value = serde_json::from_str(&serialized).unwrap();
        assert!(round["line"].is_number());
        assert_eq!(round["line"].as_i64(), Some(5));
        assert_eq!(round["ratio"].as_f64(), Some(1.5));
        // Key order and integer/decimal text survive.
        assert_eq!(serialized, r#"{"line":5,"ratio":1.5}"#);
    }

    #[test]
    fn parse_arguments_array_and_object_values_round_trip_as_json_types() {
        let src: Map<String, Value> =
            serde_json::from_str(r#"{"items": [1, "two"], "opts": {"n": 3}}"#).unwrap();
        let result = parse_arguments(&src);
        let serialized = serde_json::to_string(&result).unwrap();
        let round: Value = serde_json::from_str(&serialized).unwrap();
        assert!(round["items"].is_array());
        assert_eq!(round["items"][0].as_i64(), Some(1));
        assert_eq!(round["opts"]["n"].as_i64(), Some(3));
    }

    // ── Bare JSON regex false positive tests (#98) ──

    #[test]
    fn parse_bare_json_with_nested_arguments_extracts_tool_call() {
        let results = parse(
            r#"{"name": "bash", "arguments": {"command": "echo", "opts": {"verbose": true}}}"#,
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "bash");
    }

    #[test]
    fn parse_bare_json_with_extra_fields_before_arguments_skipped() {
        assert!(
            parse(r#"Just a plain JSON config: {"host": "localhost", "port": 8080}"#).is_empty()
        );
    }

    #[test]
    fn parse_random_json_with_name_field_not_false_positive() {
        assert!(parse(r#"{"name": "John", "age": 30, "city": "NYC"}"#).is_empty());
    }

    #[test]
    fn parse_bare_json_requires_name_first() {
        assert!(parse(r#"{"arguments": {"path": "test.txt"}, "name": "read_file"}"#).is_empty());
    }

    #[test]
    fn parse_bare_json_with_whitespace_extracts_tool_call() {
        let text = "{\n    \"name\": \"read_file\",\n    \"arguments\": {\n        \"path\": \"/tmp/test.txt\"\n    }\n}";
        let results = parse(text);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "read_file");
    }

    #[test]
    fn parse_wrapped_tool_call_format_extracts_tool_call() {
        let results = parse(r#"{"tool_call": {"name": "bash", "arguments": {"command": "ls"}}}"#);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "bash");
    }

    // ── Multiple JSON objects in single <tool_call> block ──

    #[test]
    fn parse_multiple_json_objects_in_single_tag_extracts_all() {
        let text = "<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.txt\"}}\n{\"name\": \"list_directory\", \"arguments\": {\"path\": \".\"}}\n{\"name\": \"bash\", \"arguments\": {\"command\": \"echo hi\"}}\n</tool_call>";
        let results = parse(text);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].name, "read_file");
        assert_eq!(results[1].name, "list_directory");
        assert_eq!(results[2].name, "bash");
    }

    #[test]
    fn parse_two_json_objects_in_single_tag_extracts_both() {
        let text = "<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"test.cs\"}}\n{\"name\": \"write_file\", \"arguments\": {\"path\": \"out.cs\", \"content\": \"hello\"}}\n</tool_call>";
        let results = parse(text);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].name, "read_file");
        assert_eq!(results[1].name, "write_file");
        assert_eq!(arg_str(&results[1], "path").as_deref(), Some("out.cs"));
    }

    #[test]
    fn parse_multiple_json_objects_with_nested_braces_extracts_all() {
        let text = "<tool_call>\n{\"name\": \"bash\", \"arguments\": {\"command\": \"echo {hello}\"}}\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"test.txt\"}}\n</tool_call>";
        let results = parse(text);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].name, "bash");
        assert_eq!(results[1].name, "read_file");
    }

    // ── SplitJsonObjects unit tests ──

    #[test]
    fn split_json_objects_single_object_returns_single() {
        assert_eq!(split_json_objects(r#"{"name": "test"}"#).len(), 1);
    }

    #[test]
    fn split_json_objects_multiple_objects_splits_correctly() {
        assert_eq!(
            split_json_objects(r#"{"name": "a"} {"name": "b"} {"name": "c"}"#).len(),
            3
        );
    }

    #[test]
    fn split_json_objects_newline_separated_splits_correctly() {
        assert_eq!(
            split_json_objects("{\"name\": \"a\"}\n{\"name\": \"b\"}").len(),
            2
        );
    }

    #[test]
    fn split_json_objects_empty_string_returns_empty() {
        assert!(split_json_objects("").is_empty());
    }

    #[test]
    fn split_json_objects_nested_braces_handled_correctly() {
        let result = split_json_objects(r#"{"a": {"b": "c"}} {"d": "e"}"#);
        assert_eq!(result.len(), 2);
        assert!(result[0].contains("\"b\""));
        assert!(result[1].contains("\"d\""));
    }

    #[test]
    fn split_json_objects_string_with_braces_not_split_incorrectly() {
        let result = split_json_objects(r#"{"cmd": "echo {hello} world"}"#);
        assert_eq!(result.len(), 1);
        assert!(result[0].contains("echo {hello} world"));
    }

    // ── JSON Repair Tests ──

    #[test]
    fn try_repair_json_fixes_trailing_comma() {
        let repaired =
            try_repair_json(r#"{"name": "read_file", "arguments": {"path": "/foo.txt",}}"#)
                .unwrap();
        assert!(!repaired.contains(",}"));
        serde_json::from_str::<Value>(&repaired).unwrap();
    }

    #[test]
    fn try_repair_json_fixes_missing_closing_brace() {
        let repaired =
            try_repair_json(r#"{"name": "bash", "arguments": {"command": "ls"}"#).unwrap();
        serde_json::from_str::<Value>(&repaired).unwrap();
    }

    #[test]
    fn try_repair_json_returns_null_for_valid_json() {
        assert_eq!(
            try_repair_json(r#"{"name": "read_file", "arguments": {"path": "/foo.txt"}}"#),
            None
        );
    }

    #[test]
    fn try_repair_json_truncated_inside_string_value_refuses_repair() {
        let json = r#"{"name": "write_file", "arguments": {"path": "a.py", "content": "def main():\n    pri"#;
        assert_eq!(try_repair_json(json), None);
    }

    #[test]
    fn try_repair_json_apostrophe_inside_double_quoted_string_not_mangled() {
        let repaired =
            try_repair_json(r#"{"name": "write_file", "arguments": {"content": "don't panic",}}"#)
                .unwrap();
        let doc: Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(doc["arguments"]["content"], "don't panic");
    }

    #[test]
    fn try_repair_json_single_quoted_json_still_converted() {
        let repaired =
            try_repair_json("{'name': 'read_file', 'arguments': {'path': 'foo.txt'}}").unwrap();
        let doc: Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(doc["name"], "read_file");
    }

    #[test]
    fn try_repair_json_brace_inside_string_value_not_counted_for_balancing() {
        let repaired =
            try_repair_json(r#"{"name": "bash", "arguments": {"command": "echo '{'",}}"#).unwrap();
        let doc: Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(doc["arguments"]["command"], "echo '{'");
    }

    #[test]
    fn try_repair_json_unrepairable_garbage_returns_null() {
        assert_eq!(
            try_repair_json("{\"name\": read_file no quotes at all,"),
            None
        );
    }

    #[test]
    fn parse_tool_calls_from_text_normalizes_tool_name_case() {
        let results = parse(
            r#"<tool_call>{"name": "READ_FILE", "arguments": {"path": "/test.txt"}}</tool_call>"#,
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "read_file");
    }

    // ── Rust-only: hand-written lookaround patterns and .NET details ──

    #[test]
    fn file_path_matches_like_dotnet_backtracking() {
        // Matches.
        for text in [
            "src/test.cs",
            "edit C:\\dir\\a.json now",
            "see a.b.update(x)", // `a.b` is followed by '.', not '('
            "file.py.",
            "x.y",
            "version 1.5",
            "файл.тхт",
            "call a.cs(x) after b.txt",
        ] {
            assert!(has_file_path(text), "{text}");
        }
        // No match.
        for text in [
            "world.update(dt)", // 6-char run, and a call
            "obj.run(x)",
            "obj.run  (x)",
            "a.cs (x)y",   // the whole run is followed by "\s*("
            ".cs only",    // nothing before the dot
            "a.toolong",   // run longer than 5
            "end with a.", // nothing after the dot
            "a .cs",       // space before the dot
            "plain words",
        ] {
            assert!(!has_file_path(text), "{text}");
        }
    }

    #[test]
    fn single_quote_conversion_matches_dotnet_regex_replace() {
        assert_eq!(convert_single_quotes("{'a':'b'}"), r#"{"a":"b"}"#);
        assert_eq!(
            convert_single_quotes("{'msg': 'it's fine'}"),
            r#"{"msg": "it's fine"}"#
        );
        // A quote at the very start has nothing for the lookbehind to see.
        assert_eq!(convert_single_quotes("'a' "), "'a' ");
        // The closing quote needs a following character.
        assert_eq!(convert_single_quotes("x 'a'"), "x 'a'");
        // `.` does not cross newlines.
        assert_eq!(convert_single_quotes("{'a\nb': 1}"), "{'a\nb': 1}");
        // An opener with no closer is skipped; a later pair still converts.
        assert_eq!(convert_single_quotes("[ 'x\n 'y' ]"), "[ 'x\n \"y\" ]");
        assert_eq!(
            convert_single_quotes("{'k': ['a', 'b']}"),
            r#"{"k": ["a", "b"]}"#
        );
    }

    #[test]
    fn function_equals_tag_is_wrapped_into_a_call() {
        let results = parse("<function=Read_File>{\"path\": \"a.txt\"}</function>");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "read_file");
        assert_eq!(arg_str(&results[0], "path").as_deref(), Some("a.txt"));
    }

    #[test]
    fn arguments_keep_source_key_order() {
        let results = parse(
            r#"<tool_call>{"name": "x", "arguments": {"z": 1, "a": 2, "m": [true]}}</tool_call>"#,
        );
        let keys: Vec<&str> = results[0].arguments.keys().map(String::as_str).collect();
        assert_eq!(keys, ["z", "a", "m"]);
    }

    #[test]
    fn non_string_name_is_skipped() {
        assert!(parse(r#"<tool_call>{"name": 5, "arguments": {}}</tool_call>"#).is_empty());
    }

    #[test]
    fn repaired_call_is_parsed() {
        let results =
            parse("<tool_call>{'name': 'bash', 'arguments': {'command': 'ls',}}</tool_call>");
        assert_eq!(results.len(), 1);
        assert_eq!(arg_str(&results[0], "command").as_deref(), Some("ls"));
    }
}
