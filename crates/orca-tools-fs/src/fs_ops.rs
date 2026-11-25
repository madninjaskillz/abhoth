//! The file-management tools: `cd`, `create_directory`, `mkdir`, `delete_file`, `copy_file`,
//! `move_file`, `rename_file`.

use std::path::{Path, PathBuf};

use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolRenderHints, ToolResult, ToolRiskLevel,
};
use orca_tools_common::{get_bool_lenient, io_message, is_dangerous_path, required_str, retry_io};
use serde_json::Value;

use crate::util::{display, schema};

// ── cd ──

pub struct CdTool;

#[async_trait::async_trait]
impl Tool for CdTool {
    fn name(&self) -> &str {
        "cd"
    }

    fn description(&self) -> &str {
        "Change the current working directory for all subsequent tool calls. Returns the new absolute path. Affects bash, read_file, glob, grep, and all path-relative operations."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f4c2}",
            short_description: "List directory contents",
            summary_arg_keys: &["directory"],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "The path to change to (absolute or relative to the current working directory)"
            }
        },
        "required": ["path"]
    }"#,
        )
    }

    /// C# changes the process-wide current directory; here the result carries the new
    /// directory ([`ToolResult::with_cwd_change`]) and the session applies it.
    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let destination = match required_str(args, "path") {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        let target = ctx.full_path(destination);
        if !target.is_dir() {
            return ToolResult::error(format!("Directory does not exist: {}", display(&target)));
        }
        ToolResult::success(format!("Changed directory to: {}", display(&target)))
            .with_cwd_change(&target)
    }
}

// ── create_directory / mkdir ──

async fn create_directory(args: &Value, ctx: &ToolContext) -> ToolResult {
    let path = match required_str(args, "path") {
        Ok(p) => ctx.full_path(p),
        Err(e) => return ToolResult::error(e),
    };
    let shown = display(&path);
    if path.is_dir() {
        return ToolResult::success(format!("Directory already exists: {shown}"));
    }
    match tokio::fs::create_dir_all(&path).await {
        Ok(()) => ToolResult::success(format!("Directory created: {shown}")),
        Err(e) => ToolResult::error(format!("Error creating directory: {}", io_message(&e))),
    }
}

const DIRECTORY_SCHEMA: &str = r#"{
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "The path of the directory to create"
            }
        },
        "required": ["path"]
    }"#;

pub struct CreateDirectoryTool;

#[async_trait::async_trait]
impl Tool for CreateDirectoryTool {
    fn name(&self) -> &str {
        "create_directory"
    }

    fn description(&self) -> &str {
        "Create a directory and any missing parent directories. Succeeds silently if it already exists."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f4c1}",
            short_description: "Create directory",
            summary_arg_keys: &["path"],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(DIRECTORY_SCHEMA)
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        create_directory(args, ctx).await
    }
}

pub struct MkdirTool;

#[async_trait::async_trait]
impl Tool for MkdirTool {
    fn name(&self) -> &str {
        "mkdir"
    }

    fn description(&self) -> &str {
        "Create a directory and any missing parent directories. Succeeds silently if it already exists. Use this before write_file if unsure whether the parent directory exists."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f4c1}",
            short_description: "Create directory",
            summary_arg_keys: &["path"],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(DIRECTORY_SCHEMA)
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        create_directory(args, ctx).await
    }
}

// ── delete_file ──

pub struct DeleteFileTool;

#[async_trait::async_trait]
impl Tool for DeleteFileTool {
    fn name(&self) -> &str {
        "delete_file"
    }

    fn description(&self) -> &str {
        "Delete a file or directory. Requires recursive=true to delete non-empty directories. Rejects dangerous system paths for safety."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::MODIFIES_FILES | ToolCapabilities::DIRECTORY_RESTRICTED
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f5d1}\u{fe0f}",
            short_description: "Delete file",
            summary_arg_keys: &["path"],
            aliases: &["rm"],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "The path of the file or directory to delete"
            },
            "recursive": {
                "type": "boolean",
                "description": "Required to delete non-empty directories. Defaults to false."
            }
        },
        "required": ["path"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let path = match required_str(args, "path") {
            Ok(p) => ctx.full_path(p),
            Err(e) => return ToolResult::error(e),
        };
        let recursive = get_bool_lenient(args.get("recursive"), false);
        let shown = display(&path);
        if is_dangerous_path(&path) {
            return ToolResult::error(format!("Refusing to delete dangerous path: {shown}"));
        }
        if path.is_file() {
            return match retry_io(|| tokio::fs::remove_file(&path)).await {
                Ok(()) => ToolResult::success(format!("Deleted file: {shown}")),
                Err(e) => ToolResult::error(format!("Error deleting: {}", io_message(&e))),
            };
        }
        if path.is_dir() {
            let non_empty = std::fs::read_dir(&path)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false);
            if !recursive && non_empty {
                return ToolResult::error(format!(
                    "Directory is not empty: {shown}. Set recursive=true to delete non-empty directories."
                ));
            }
            let r = if recursive {
                retry_io(|| tokio::fs::remove_dir_all(&path)).await
            } else {
                retry_io(|| tokio::fs::remove_dir(&path)).await
            };
            return match r {
                Ok(()) => ToolResult::success(format!("Deleted directory: {shown}")),
                Err(e) => ToolResult::error(format!("Error deleting: {}", io_message(&e))),
            };
        }
        ToolResult::error(format!("Path not found: {shown}"))
    }
}

// ── copy_file ──

pub struct CopyFileTool;

/// .NET's message when `File.Copy` without overwrite finds the destination.
fn file_exists_message(dest: &Path) -> String {
    format!("The file '{}' already exists.", display(dest))
}

fn copy_one(src: &Path, dest: &Path, overwrite: bool) -> std::io::Result<()> {
    if !overwrite && dest.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            file_exists_message(dest),
        ));
    }
    std::fs::copy(src, dest).map(|_| ())
}

/// `CopyDirectory`: the tree under `source` into `destination` (created as needed).
fn copy_directory(source: &Path, destination: &Path, overwrite: bool) -> std::io::Result<()> {
    std::fs::create_dir_all(destination)?;
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        if entry.path().is_dir() {
            dirs.push(entry);
        } else {
            files.push(entry);
        }
    }
    for f in files {
        copy_one(&f.path(), &destination.join(f.file_name()), overwrite)?;
    }
    for d in dirs {
        copy_directory(&d.path(), &destination.join(d.file_name()), overwrite)?;
    }
    Ok(())
}

#[async_trait::async_trait]
impl Tool for CopyFileTool {
    fn name(&self) -> &str {
        "copy_file"
    }

    fn description(&self) -> &str {
        "Copy a file or directory. Set recursive=true to copy directories with contents. Creates parent directories for the destination automatically."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::MODIFIES_FILES | ToolCapabilities::DIRECTORY_RESTRICTED
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f4cb}",
            short_description: "Copy file",
            summary_arg_keys: &["path"],
            aliases: &["cp"],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "source": {
                "type": "string",
                "description": "The source file or directory path"
            },
            "destination": {
                "type": "string",
                "description": "The destination path"
            },
            "recursive": {
                "type": "boolean",
                "description": "Copy directories recursively. Required for directory copies. Defaults to false."
            },
            "overwrite": {
                "type": "boolean",
                "description": "Overwrite existing files at destination. Defaults to false."
            }
        },
        "required": ["source", "destination"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        let (source, destination) = match source_destination(args, ctx) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        let recursive = get_bool_lenient(args.get("recursive"), false);
        let overwrite = get_bool_lenient(args.get("overwrite"), false);
        let (src_shown, dst_shown) = (display(&source), display(&destination));
        let fail =
            |e: std::io::Error| ToolResult::error(format!("Error copying: {}", io_message(&e)));

        if let Some(dir) = destination.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            return fail(e);
        }
        if source.is_file() {
            let (s, d) = (source.clone(), destination.clone());
            return match retry_io(|| {
                let (s, d) = (s.clone(), d.clone());
                async move { copy_one(&s, &d, overwrite) }
            })
            .await
            {
                Ok(()) => {
                    ToolResult::success(format!("Copied file: {src_shown} \u{2192} {dst_shown}"))
                }
                Err(e) => fail(e),
            };
        }
        if source.is_dir() {
            if !recursive {
                return ToolResult::error(
                    "Source is a directory. Set recursive=true to copy directories.",
                );
            }
            return match copy_directory(&source, &destination, overwrite) {
                Ok(()) => ToolResult::success(format!(
                    "Copied directory: {src_shown} \u{2192} {dst_shown}"
                )),
                Err(e) => fail(e),
            };
        }
        ToolResult::error(format!("Source not found: {src_shown}"))
    }
}

fn source_destination(args: &Value, ctx: &ToolContext) -> Result<(PathBuf, PathBuf), String> {
    let source = required_str(args, "source")?;
    let destination = required_str(args, "destination")?;
    Ok((ctx.full_path(source), ctx.full_path(destination)))
}

// ── move_file / rename_file ──

/// The words that differ between `move_file` and `rename_file`.
struct MoveWords {
    verb: &'static str,
    file_done: &'static str,
    dir_done: &'static str,
    error: &'static str,
}

const MOVE: MoveWords = MoveWords {
    verb: "move",
    file_done: "Moved file",
    dir_done: "Moved directory",
    error: "Error moving",
};

const RENAME: MoveWords = MoveWords {
    verb: "rename",
    file_done: "Renamed",
    dir_done: "Renamed directory",
    error: "Error renaming",
};

/// .NET's `File.Move` error when the destination exists and overwrite is off.
fn move_exists_message(dest: &Path) -> String {
    if cfg!(windows) {
        "Cannot create a file when that file already exists.".into()
    } else {
        file_exists_message(dest)
    }
}

fn move_file(source: &Path, destination: &Path, overwrite: bool) -> std::io::Result<()> {
    if !overwrite && destination.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            move_exists_message(destination),
        ));
    }
    std::fs::rename(source, destination)
}

/// 8 hex digits for the backup name (C# takes them from a GUID).
fn backup_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{:08x}",
        (nanos as u64 ^ u64::from(std::process::id())) & 0xFFFF_FFFF
    )
}

fn move_directory_over(source: &Path, destination: &Path) -> std::io::Result<()> {
    // Move the destination aside first so a failed move can be undone.
    let backup = PathBuf::from(format!(
        "{}.orca-backup-{}",
        destination.display(),
        backup_suffix()
    ));
    std::fs::rename(destination, &backup)?;
    let moved =
        std::fs::rename(source, destination).and_then(|()| std::fs::remove_dir_all(&backup));
    if let Err(e) = moved {
        if backup.is_dir() && !destination.is_dir() {
            let _ = std::fs::rename(&backup, destination);
        }
        return Err(e);
    }
    Ok(())
}

fn move_or_rename(args: &Value, ctx: &ToolContext, words: &MoveWords) -> ToolResult {
    let (source, destination) = match source_destination(args, ctx) {
        Ok(p) => p,
        Err(e) => return ToolResult::error(e),
    };
    let overwrite = get_bool_lenient(args.get("overwrite"), false);
    let (src_shown, dst_shown) = (display(&source), display(&destination));
    if is_dangerous_path(&source) {
        return ToolResult::error(format!(
            "Refusing to {} dangerous path: {src_shown}",
            words.verb
        ));
    }
    let fail =
        |e: std::io::Error| ToolResult::error(format!("{}: {}", words.error, io_message(&e)));

    if let Some(dir) = destination.parent()
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        return fail(e);
    }
    if source.is_file() {
        return match move_file(&source, &destination, overwrite) {
            Ok(()) => ToolResult::success(format!(
                "{}: {src_shown} \u{2192} {dst_shown}",
                words.file_done
            )),
            Err(e) => fail(e),
        };
    }
    if source.is_dir() {
        let r = if destination.is_dir() {
            if !overwrite {
                return ToolResult::error(format!(
                    "Destination directory already exists: {dst_shown}. Set overwrite=true or choose a different name."
                ));
            }
            move_directory_over(&source, &destination)
        } else {
            std::fs::rename(&source, &destination)
        };
        return match r {
            Ok(()) => ToolResult::success(format!(
                "{}: {src_shown} \u{2192} {dst_shown}",
                words.dir_done
            )),
            Err(e) => fail(e),
        };
    }
    ToolResult::error(format!("Source not found: {src_shown}"))
}

const MOVE_SCHEMA: &str = r#"{
        "type": "object",
        "properties": {
            "source": {
                "type": "string",
                "description": "The source file or directory path"
            },
            "destination": {
                "type": "string",
                "description": "The destination path"
            },
            "overwrite": {
                "type": "boolean",
                "description": "Overwrite the destination if it exists. Defaults to false."
            }
        },
        "required": ["source", "destination"]
    }"#;

pub struct MoveFileTool;

#[async_trait::async_trait]
impl Tool for MoveFileTool {
    fn name(&self) -> &str {
        "move_file"
    }

    fn description(&self) -> &str {
        "Move or rename a file or directory. Creates parent directories for the destination automatically. Set overwrite=true to replace an existing destination."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::MODIFIES_FILES | ToolCapabilities::DIRECTORY_RESTRICTED
    }

    /// The source, and `path` (which the schema does not have; C# reads it anyway).
    fn affected_file_paths(&self, args: &Value) -> Vec<String> {
        ["source", "path"]
            .iter()
            .filter_map(|k| args.get(*k).and_then(Value::as_str))
            .map(str::to_string)
            .collect()
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f4e6}",
            short_description: "Move or rename file",
            summary_arg_keys: &["source"],
            aliases: &["mv"],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(MOVE_SCHEMA)
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        move_or_rename(args, ctx, &MOVE)
    }
}

pub struct RenameFileTool;

#[async_trait::async_trait]
impl Tool for RenameFileTool {
    fn name(&self) -> &str {
        "rename_file"
    }

    fn description(&self) -> &str {
        "Rename a file or directory. Creates parent directories for the destination automatically. Set overwrite=true to replace an existing destination."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::MODIFIES_FILES | ToolCapabilities::DIRECTORY_RESTRICTED
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{1f4e6}",
            short_description: "Move or rename file",
            summary_arg_keys: &["path"],
            aliases: &[],
        })
    }

    fn parameter_schema(&self) -> Value {
        schema(
            r#"{
        "type": "object",
        "properties": {
            "source": {
                "type": "string",
                "description": "The current file or directory path"
            },
            "destination": {
                "type": "string",
                "description": "The new file or directory path"
            },
            "overwrite": {
                "type": "boolean",
                "description": "Overwrite the destination if it exists. Defaults to false."
            }
        },
        "required": ["source", "destination"]
    }"#,
        )
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult {
        move_or_rename(args, ctx, &RENAME)
    }
}
