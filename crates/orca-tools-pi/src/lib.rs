//! The Pi profile's tools (port of `OpenOrca.Tools.Pi`): Pi's exact names, descriptions,
//! JSON schemas, argument validation and result strings, so a model tuned on the Pi coding
//! agent sees the tools it knows (`toolProfile: "pi"`).
//!
//! Every tool here has [`ToolCapabilities::VERBATIM_OUTPUT`]: the host must not prefix,
//! truncate or append to its results. Each tool also validates its own arguments
//! ([`Tool::prepare_arguments`], C# `IArgumentPreparingTool`) with Pi's error texts.
//!
//! Not ported (see the crate's gaps in `STATUS.md`): `job screenshot` (Win32 window capture
//! needs `unsafe` FFI, which the workspace forbids); its pure helpers are here and tested.
//! The `subagent`, `progress` and `lead_mode` tools take host hooks; without them they answer
//! with C#'s "not available in this session" errors.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use orca_abstractions::{Tool, ToolErrorCode, ToolResult};
use serde_json::Value;

pub mod arguments;
pub mod bash;
pub mod edit;
pub mod edit_diff;
pub mod job_tool;
pub mod jobs;
pub mod json;
pub mod lead_mode;
pub mod output_accumulator;
pub mod paths;
pub mod progress;
pub mod read;
pub mod search;
pub mod shell;
pub mod subagent;
pub mod system_prompt;
pub mod test_summary;
pub mod truncate;
pub mod window_capture;
pub mod write;

pub use bash::PiBashTool;
pub use edit::PiEditTool;
pub use job_tool::PiJobTool;
pub use jobs::{PiJob, PiJobManager};
pub use lead_mode::PiLeadModeTool;
pub use progress::PiProgressTool;
pub use read::PiReadTool;
pub use search::{PiFindTool, PiGrepTool, PiLsTool};
pub use shell::PiShellConfig;
pub use subagent::PiSubagentTool;
pub use system_prompt::PiContextFile;
pub use write::PiWriteTool;

/// The Pi-side metadata of a tool: its line under `<tools>` and its `<rules>` entries in Pi's
/// system prompt (C# `PiToolBase.PromptSnippet` / `PromptGuidelines`).
pub trait PiToolInfo {
    fn prompt_snippet(&self) -> String;
    fn prompt_guidelines(&self) -> Vec<String> {
        Vec::new()
    }
}

/// A tool of this crate: a [`Tool`] with Pi metadata.
pub trait PiTool: Tool + PiToolInfo {}
impl<T: Tool + PiToolInfo> PiTool for T {}

/// `toolProfile` value that selects the Pi tools (`PiToolset.ProfileName`).
pub const PROFILE_NAME: &str = "pi";

/// Pi's four core tools, in Pi's registry order.
pub const CORE_TOOL_NAMES: [&str; 4] = ["read", "bash", "edit", "write"];

/// The optional extras (`pi.extraTools`).
pub const EXTRA_TOOL_NAMES: [&str; 3] = ["grep", "find", "ls"];

/// `PiToolset.IsPiProfile`.
pub fn is_pi_profile(tool_profile: &str) -> bool {
    tool_profile.eq_ignore_ascii_case(PROFILE_NAME)
}

/// Options for [`create_toolset`] (`PiToolsetOptions`).
#[derive(Clone)]
pub struct PiToolsetOptions {
    /// Also expose grep, find and ls (Pi's default is the four core tools).
    pub extra_tools: bool,
    /// bash `background: true` plus the job tool. On by default.
    pub jobs: bool,
    /// The resolved bash. `None` makes bash resolve it per call (and report Pi's error).
    pub shell: Option<PiShellConfig>,
    /// Timeout for bash calls without a timeout argument (capped at 600 s); 0 = none.
    pub bash_default_timeout_seconds: i32,
    /// PI_* variables for bash (PI_MODEL, PI_PROVIDER, ...).
    pub session_environment: Vec<(String, String)>,
    /// Background jobs of bash and job. Default: the process-wide manager.
    pub job_manager: Option<Arc<PiJobManager>>,
}

impl Default for PiToolsetOptions {
    fn default() -> Self {
        Self {
            extra_tools: false,
            jobs: true,
            shell: None,
            bash_default_timeout_seconds: 0,
            session_environment: Vec::new(),
            job_manager: None,
        }
    }
}

/// `PiToolset.Create`: the Pi tools in Pi's registry order (read, bash, edit, write, grep,
/// find, ls), then OpenOrca's job tool.
pub fn create_toolset(options: &PiToolsetOptions) -> Vec<Arc<dyn PiTool>> {
    let jobs = options
        .job_manager
        .clone()
        .unwrap_or_else(PiJobManager::shared);
    let mut tools: Vec<Arc<dyn PiTool>> = vec![
        Arc::new(PiReadTool),
        Arc::new(PiBashTool::new(
            options.shell.clone(),
            None,
            options.bash_default_timeout_seconds,
            options.session_environment.clone(),
            jobs.clone(),
        )),
        Arc::new(PiEditTool),
        Arc::new(PiWriteTool),
    ];
    if options.extra_tools {
        tools.push(Arc::new(PiGrepTool));
        tools.push(Arc::new(PiFindTool));
        tools.push(Arc::new(PiLsTool));
    }
    if options.jobs {
        tools.push(Arc::new(PiJobTool::new(jobs)));
    }
    tools
}

/// Every Pi tool's metadata (the subagent and progress tools included): like every entry here
/// they only reach the prompt when the tool is actually advertised.
static METADATA: LazyLock<Vec<(String, String, Vec<String>)>> = LazyLock::new(|| {
    let mut tools = create_toolset(&PiToolsetOptions {
        extra_tools: true,
        job_manager: Some(Arc::new(PiJobManager::new())),
        ..Default::default()
    });
    tools.push(Arc::new(PiSubagentTool::default()));
    tools.push(Arc::new(PiProgressTool::default()));
    tools
        .iter()
        .map(|t| {
            (
                t.name().to_string(),
                t.prompt_snippet(),
                t.prompt_guidelines(),
            )
        })
        .collect()
});

/// `PiToolset.PromptSnippets`: every Pi tool's `<tools>` snippet, by name.
pub fn prompt_snippets() -> HashMap<String, String> {
    METADATA
        .iter()
        .map(|(n, s, _)| (n.clone(), s.clone()))
        .collect()
}

/// `PiToolset.PromptGuidelines`: every Pi tool's `<rules>` guidelines, by name.
pub fn prompt_guidelines() -> HashMap<String, Vec<String>> {
    METADATA
        .iter()
        .map(|(n, _, g)| (n.clone(), g.clone()))
        .collect()
}

/// `PiToolset.AdvertisedNames`: the names the Pi profile advertises, in order.
pub fn advertised_names(extra_tools: bool, jobs: bool) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = CORE_TOOL_NAMES.to_vec();
    if extra_tools {
        names.extend(EXTRA_TOOL_NAMES);
    }
    if jobs {
        names.push(job_tool::TOOL_NAME);
    }
    names
}

// ---- shared helpers (PiToolBase) ------------------------------------------------------------

/// Parse a schema literal (key order kept: `serde_json` has `preserve_order`).
pub(crate) fn schema(text: &str) -> Value {
    serde_json::from_str(text).expect("Pi tool schema is valid JSON")
}

/// `PiToolBase.PrepareArguments`: Pi's validation against the schema.
pub(crate) fn prepare(
    name: &str,
    schema_json: &str,
    arguments_json: &str,
) -> Result<String, String> {
    arguments::validate(name, &schema(schema_json), arguments_json)
}

/// A failed call: Pi's text as is, flagged as an error.
pub(crate) fn fail(message: impl Into<String>) -> ToolResult {
    ToolResult::error_with(message, ToolErrorCode::ExecutionFailed)
}

pub(crate) fn fail_with(message: impl Into<String>, code: ToolErrorCode) -> ToolResult {
    ToolResult::error_with(message, code)
}

pub(crate) fn get_string<'a>(args: &'a Value, name: &str) -> Option<&'a str> {
    args.get(name).and_then(Value::as_str)
}

pub(crate) fn get_number(args: &Value, name: &str) -> Option<f64> {
    args.get(name).and_then(Value::as_f64)
}

pub(crate) fn get_bool(args: &Value, name: &str) -> bool {
    args.get(name).and_then(Value::as_bool) == Some(true)
}

/// The file a call touches, resolved the way the tool resolves it (~, @, /c/...).
pub(crate) fn affected_path(args: &Value, cwd: &Path) -> Vec<String> {
    match get_string(args, "path") {
        Some(p) if !p.is_empty() => {
            vec![paths::resolve_to_cwd(p, cwd).to_string_lossy().into_owned()]
        }
        _ => Vec::new(),
    }
}

/// `PiFileMutationQueue`: serialises write/edit calls to the same file, so a parallel tool
/// batch that touches one file twice applies the calls one at a time.
pub(crate) async fn with_file_lock<T, F>(absolute_path: &Path, action: F) -> T
where
    F: std::future::Future<Output = T>,
{
    static LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        LazyLock::new(Default::default);
    let key = {
        let s = absolute_path.to_string_lossy().into_owned();
        if cfg!(windows) { s.to_lowercase() } else { s }
    };
    let gate = LOCKS
        .lock()
        .unwrap()
        .entry(key)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    let _guard = gate.lock().await;
    action.await
}

/// `PiFsErrors`: Node-style fs error text ("ENOENT: no such file or directory, open 'x'").
pub(crate) mod fs_errors {
    use std::io;

    pub fn code(e: &io::Error) -> Option<&'static str> {
        match e.kind() {
            io::ErrorKind::NotFound => Some("ENOENT"),
            io::ErrorKind::PermissionDenied => Some(if cfg!(windows) { "EPERM" } else { "EACCES" }),
            io::ErrorKind::InvalidFilename => Some("ENAMETOOLONG"),
            _ => None,
        }
    }

    fn description(code: &str) -> Option<&'static str> {
        Some(match code {
            "ENOENT" => "no such file or directory",
            "EPERM" => "operation not permitted",
            "EACCES" => "permission denied",
            "EISDIR" => "illegal operation on a directory",
            "ENOTDIR" => "not a directory",
            "ENAMETOOLONG" => "name too long",
            _ => return None,
        })
    }

    pub fn format(code: &str, syscall: &str, path: Option<&str>) -> String {
        let d = description(code).unwrap_or(code);
        match path {
            None => format!("{code}: {d}, {syscall}"),
            Some(p) => format!("{code}: {d}, {syscall} '{p}'"),
        }
    }

    /// An I/O error as Node words it, else the error's own message.
    pub fn format_io(e: &io::Error, syscall: &str, path: Option<&str>) -> String {
        match code(e) {
            Some(c) => format(c, syscall, path),
            None => e.to_string(),
        }
    }
}

/// A pseudo-random 64-bit value (temp file names); no extra crate needed for that.
pub(crate) fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        h.write_u128(d.as_nanos());
    }
    h.write_u32(std::process::id());
    h.finish()
}

/// `Path.GetTempPath()` without the trailing separator .NET adds (joined with a file name the
/// result is the same).
pub(crate) fn temp_dir() -> PathBuf {
    std::env::temp_dir()
}
