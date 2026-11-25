//! Pi's edit tool (edit.ts, `PiEditTool`): `edits[].oldText` matched against the ORIGINAL file,
//! exact then fuzzy, unique and non-overlapping; CRLF and BOM preserved; Pi's error texts.

use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolErrorCode, ToolResult, ToolRiskLevel,
};
use serde_json::Value;

use crate::edit_diff::{self, PiEdit};
use crate::{
    PiToolInfo, affected_path, fail, fail_with, fs_errors, get_string, paths, with_file_lock,
};

pub struct PiEditTool;

const SCHEMA: &str = r#"{"type":"object","required":["path","edits"],"properties":{"path":{"type":"string","description":"Path to the file to edit (relative or absolute)"},"edits":{"type":"array","items":{"type":"object","required":["oldText","newText"],"properties":{"oldText":{"type":"string","description":"Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."},"newText":{"type":"string","description":"Replacement text for this targeted edit."}}},"description":"One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead."}}}"#;

impl PiToolInfo for PiEditTool {
    fn prompt_snippet(&self) -> String {
        "Make precise file edits with exact text replacement, including multiple disjoint edits in one call".into()
    }
    fn prompt_guidelines(&self) -> Vec<String> {
        vec![
            "Use edit for precise changes (edits[].oldText must match exactly)".into(),
            "When changing multiple separate locations in one file, use one edit call with multiple entries in edits[] instead of multiple edit calls".into(),
            "Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.".into(),
            "Keep edits[].oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.".into(),
        ]
    }
}

#[async_trait::async_trait]
impl Tool for PiEditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }

    fn parameter_schema(&self) -> Value {
        crate::schema(SCHEMA)
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::VERBATIM_OUTPUT
            | ToolCapabilities::MODIFIES_FILES
            | ToolCapabilities::MODIFIES_FILE_CONTENT
            | ToolCapabilities::DIRECTORY_RESTRICTED
    }

    fn prepare_arguments(&self, arguments_json: &str) -> Option<Result<String, String>> {
        let repaired = crate::arguments::prepare_edit_arguments(arguments_json);
        Some(crate::prepare(self.name(), SCHEMA, &repaired))
    }

    fn affected_file_paths(&self, args: &Value, ctx: &ToolContext) -> Vec<String> {
        affected_path(args, &ctx.cwd)
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let path = get_string(args, "path").unwrap_or("").to_string();
        let edits: Vec<PiEdit> = args
            .get("edits")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .map(|e| {
                        PiEdit::new(
                            get_string(e, "oldText").unwrap_or(""),
                            get_string(e, "newText").unwrap_or(""),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        if edits.is_empty() {
            return fail_with(
                "Edit tool input is invalid. edits must contain at least one replacement.",
                ToolErrorCode::InvalidArguments,
            );
        }
        let abs = paths::resolve_to_cwd(&path, &ctx.cwd);
        with_file_lock(&abs, edit_locked(&abs, &path, &edits)).await
    }
}

async fn edit_locked(abs: &std::path::Path, path: &str, edits: &[PiEdit]) -> ToolResult {
    if !abs.is_file() {
        let code = if abs.is_dir() { "EISDIR" } else { "ENOENT" };
        return fail_with(
            format!("Could not edit file: {path}. Error code: {code}."),
            ToolErrorCode::NotFound,
        );
    }
    if std::fs::metadata(abs).is_ok_and(|m| m.permissions().readonly()) {
        let code = if cfg!(windows) { "EPERM" } else { "EACCES" };
        return fail_with(
            format!("Could not edit file: {path}. Error code: {code}."),
            ToolErrorCode::PermissionDenied,
        );
    }
    let data = match tokio::fs::read(abs).await {
        Ok(d) => d,
        Err(e) => return fail(fs_errors::format_io(&e, "read", None)),
    };
    let decoded = String::from_utf8_lossy(&data);
    let (bom, content) = edit_diff::split_bom(&decoded);
    let original_ending = edit_diff::detect_line_ending(content);
    let normalized = edit_diff::normalize_to_lf(content);

    let new_content = match edit_diff::apply_edits(&normalized, edits, path) {
        Ok(c) => c,
        Err(message) => return fail(message),
    };

    let final_text = format!(
        "{bom}{}",
        edit_diff::restore_line_endings(&new_content, original_ending)
    );
    if let Err(e) = tokio::fs::write(abs, final_text.as_bytes()).await {
        return fail(fs_errors::format_io(
            &e,
            "open",
            Some(&abs.to_string_lossy()),
        ));
    }
    ToolResult::success(format!(
        "Successfully replaced {} block(s) in {path}.",
        edits.len()
    ))
}
