//! Contracts between the OpenOrca host and its tools (port of `OpenOrca.Abstractions`).
//!
//! In C# tools are discovered by reflection from plugin DLLs. In Rust every tool crate exports
//! a constructor function and the host registers them at compile time (see `PORTING.md`,
//! "Plugins").

use std::ops::{BitOr, BitOrAssign};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// How dangerous a tool is; drives the permission prompt (`ToolRiskLevel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolRiskLevel {
    ReadOnly,
    Moderate,
    Dangerous,
}

impl ToolRiskLevel {
    /// The C# enum name (`ToolRiskLevel.ToString()`), used in permission checks and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            ToolRiskLevel::ReadOnly => "ReadOnly",
            ToolRiskLevel::Moderate => "Moderate",
            ToolRiskLevel::Dangerous => "Dangerous",
        }
    }
}

/// Flags describing what a tool does (`ToolCapabilities`, a `[Flags]` enum in C#).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ToolCapabilities(u32);

impl ToolCapabilities {
    pub const NONE: Self = Self(0);
    pub const MODIFIES_FILES: Self = Self(1 << 0);
    pub const DIRECTORY_RESTRICTED: Self = Self(1 << 1);
    pub const HAS_DIFF_OUTPUT: Self = Self(1 << 2);
    pub const ALWAYS_VISIBLE: Self = Self(1 << 3);
    pub const INTERACTIVE: Self = Self(1 << 4);
    /// Exempt from the global per-tool timeout; the tool manages its own time budget.
    pub const LONG_RUNNING: Self = Self(1 << 5);
    /// Reads file content the model will base edits on (read-before-edit tracking).
    pub const READS_FILE_CONTENT: Self = Self(1 << 6);
    /// Rewrites an existing file's content (read-before-edit, same-file ordering).
    pub const MODIFIES_FILE_CONTENT: Self = Self(1 << 7);
    /// The result text is final: no "ERROR:" prefix, no host truncation (the Pi profile).
    pub const VERBATIM_OUTPUT: Self = Self(1 << 8);

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for ToolCapabilities {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for ToolCapabilities {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Machine-readable failure category (`ToolErrorCode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ToolErrorCode {
    #[default]
    None,
    InvalidArguments,
    NotFound,
    PermissionDenied,
    Blocked,
    Timeout,
    ExecutionFailed,
    Cancelled,
}

/// One piece of structured tool output (text, or binary such as an image).
#[derive(Debug, Clone, PartialEq)]
pub enum ToolResultPart {
    Text(String),
    Image { data: Vec<u8>, mime_type: String },
}

/// The outcome of a tool call (`ToolResult`). `content` is always the canonical text.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
    pub was_denied: bool,
    pub error_code: ToolErrorCode,
    pub parts: Option<Vec<ToolResultPart>>,
    pub metadata: Option<serde_json::Map<String, Value>>,
}

impl ToolResult {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ..Self::default()
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::error_with(message, ToolErrorCode::ExecutionFailed)
    }

    pub fn error_with(message: impl Into<String>, code: ToolErrorCode) -> Self {
        Self {
            content: message.into(),
            is_error: true,
            error_code: code,
            ..Self::default()
        }
    }

    pub fn denied(reason: impl Into<String>) -> Self {
        Self {
            content: reason.into(),
            is_error: true,
            was_denied: true,
            error_code: ToolErrorCode::PermissionDenied,
            ..Self::default()
        }
    }

    pub fn blocked(reason: impl Into<String>) -> Self {
        Self::error_with(reason, ToolErrorCode::Blocked)
    }
}

/// Display hints for the terminal UI (`IToolRenderHints`).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolRenderHints {
    pub emoji: &'static str,
    pub short_description: &'static str,
    pub summary_arg_keys: &'static [&'static str],
    pub aliases: &'static [&'static str],
}

/// What a tool call runs against. In C# tools read the process-wide current directory;
/// here it is passed in, so tests (and later, parallel agents) can each have their own.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
}

impl ToolContext {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }

    /// `Path.GetFullPath(path)` relative to [`ToolContext::cwd`].
    pub fn full_path(&self, path: &str) -> PathBuf {
        full_path(&self.cwd, path)
    }
}

/// Lexical `Path.GetFullPath`: joins `path` onto `base` unless it is absolute, then removes
/// `.` and resolves `..` without touching the file system (symlinks are not followed, as in
/// .NET). On Windows forward slashes become backslashes, as GetFullPath does.
pub fn full_path(base: &Path, path: &str) -> PathBuf {
    #[cfg(windows)]
    let path_owned = path.replace('/', "\\");
    #[cfg(windows)]
    let path = path_owned.as_str();

    let joined = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        base.join(path)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        use std::path::Component;
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Never pop past the root or a drive prefix.
                let at_root = matches!(
                    out.components().next_back(),
                    None | Some(Component::RootDir) | Some(Component::Prefix(_))
                );
                if !at_root {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A tool the model can call (`IOrcaTool`, plus `IStreamingOrcaTool` via
/// [`Tool::execute_streaming`]).
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn risk_level(&self) -> ToolRiskLevel;

    /// JSON Schema of the parameters, as sent for native function calling.
    fn parameter_schema(&self) -> Value;

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::NONE
    }

    /// The argument used for permission pattern matching ("path"; "command" for shells).
    fn permission_arg_name(&self) -> &str {
        "path"
    }

    /// File paths this call changes (checkpoints, files_modified). Default: `args.path`.
    /// `ctx` lets a tool resolve them the way it resolves them itself (the Pi tools return
    /// absolute paths, as C# `PiToolBase.GetAffectedFilePaths` does).
    fn affected_file_paths(&self, args: &Value, ctx: &ToolContext) -> Vec<String> {
        let _ = ctx;
        args.get("path")
            .and_then(Value::as_str)
            .map(|s| vec![s.to_string()])
            .unwrap_or_default()
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        None
    }

    /// `IArgumentPreparingTool.PrepareArguments`: a tool that owns argument repair, coercion
    /// and validation (and its error text) returns `Some(Ok(arguments_json))` with the prepared
    /// arguments or `Some(Err(message))`. `None` (the default): the host parses, alias-resolves
    /// and checks required arguments itself.
    fn prepare_arguments(&self, arguments_json: &str) -> Option<Result<String, String>> {
        let _ = arguments_json;
        None
    }

    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult;

    /// Execute, streaming partial output lines to `on_output` (for display only); the
    /// returned result is what the model sees. Default: no streaming.
    async fn execute_streaming(
        &self,
        args: &Value,
        ctx: &ToolContext,
        on_output: &(dyn for<'s> Fn(&'s str) + Send + Sync),
    ) -> ToolResult {
        let _ = on_output;
        self.execute(args, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_combine_and_contain() {
        let caps = ToolCapabilities::MODIFIES_FILES | ToolCapabilities::DIRECTORY_RESTRICTED;
        assert!(caps.contains(ToolCapabilities::MODIFIES_FILES));
        assert!(caps.contains(ToolCapabilities::DIRECTORY_RESTRICTED));
        assert!(!caps.contains(ToolCapabilities::HAS_DIFF_OUTPUT));
        assert_eq!(caps.bits(), 3);
    }

    #[test]
    fn result_constructors_match_csharp() {
        let denied = ToolResult::denied("no");
        assert!(denied.is_error && denied.was_denied);
        assert_eq!(denied.error_code, ToolErrorCode::PermissionDenied);
        assert_eq!(
            ToolResult::error("x").error_code,
            ToolErrorCode::ExecutionFailed
        );
        assert_eq!(ToolResult::blocked("x").error_code, ToolErrorCode::Blocked);
        assert!(!ToolResult::success("ok").is_error);
    }

    #[test]
    fn full_path_is_lexical() {
        let base = std::env::temp_dir().join("orca-base");
        let p = full_path(&base, "a/./b/../c.txt");
        assert_eq!(p, base.join("a").join("c.txt"));
        let abs = full_path(&base, base.join("x").to_str().unwrap());
        assert_eq!(abs, base.join("x"));
    }
}
