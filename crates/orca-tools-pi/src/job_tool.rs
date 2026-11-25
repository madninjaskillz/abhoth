//! OpenOrca's companion to bash `background: true` (`PiJobTool`): `wait` blocks until the job
//! exits, a new output line matches a regex, or a timeout passes, so the model never
//! sleep-polls; `list` and `kill` manage jobs. `screenshot` (Windows window capture) is not
//! ported: its argument checks run, then it reports that it is not available in this build.

use std::sync::Arc;
use std::time::Duration;

use orca_abstractions::{
    Tool, ToolCapabilities, ToolContext, ToolErrorCode, ToolRenderHints, ToolResult, ToolRiskLevel,
};
use regex::Regex;
use serde_json::Value;

use crate::jobs::{PiJob, PiJobManager, WaitOutcome};
use crate::truncate;
use crate::window_capture::{self, PiKey, PiWindowShot};
use crate::{PiToolInfo, fail, fail_with, get_bool, get_number, get_string};

pub const TOOL_NAME: &str = "job";

/// Default and maximum wait, in seconds.
pub const DEFAULT_WAIT_SECONDS: i32 = 300;
pub const MAX_WAIT_SECONDS: i32 = 600;

/// Output lines shown after an exit, and after a match or timeout.
pub const EXIT_TAIL_LINES: u64 = 60;
pub const RUNNING_TAIL_LINES: u64 = 15;
const TAIL_BYTES: usize = 8 * 1024;

/// At most this many sizes (shots) per screenshot call.
pub const MAX_SIZES: usize = 3;
/// At most this many images per screenshot call (key steps plus extra sizes).
pub const MAX_SHOTS: usize = 8;

const SCHEMA: &str = r#"{"type":"object","required":["action"],"properties":{"action":{"type":"string","enum":["wait","list","kill","screenshot"]},"id":{"type":"string","description":"Job id from bash (wait, kill, screenshot)"},"keys":{"type":"string","description":"screenshot: keys to press in the window first, separated by spaces (letters, digits, Left Right Up Down Tab Enter Esc Space Backspace Home End PageUp PageDown F1-F12, Shift+/Ctrl+/Alt+ combos); the word shot takes a picture at that point (e.g. \"shot Right shot G shot Esc\")"},"wait":{"type":"number","description":"screenshot: seconds to wait after the keys (default 1, max 30)"},"size":{"type":"string","description":"screenshot: resize the window's inside to this size first, e.g. \"1920x1080\"; several sizes separated by spaces (\"1280x720 1920x1080\") give one shot each"},"kill":{"type":"boolean","description":"screenshot: kill the job after the shots"},"until":{"type":"string","description":"wait: return when a new output line matches this regex"},"timeout":{"type":"number","description":"wait: seconds (default 300, max 600)"}}}"#;

pub struct PiJobTool {
    pub jobs: Arc<PiJobManager>,
}

impl PiJobTool {
    pub fn new(jobs: Arc<PiJobManager>) -> Self {
        Self { jobs }
    }
}

impl Default for PiJobTool {
    fn default() -> Self {
        Self::new(PiJobManager::shared())
    }
}

impl PiToolInfo for PiJobTool {
    fn prompt_snippet(&self) -> String {
        "Wait on, list or kill background jobs (bash background: true); wait instead of sleep; screenshot a job's window (GUI apps)".into()
    }
}

#[async_trait::async_trait]
impl Tool for PiJobTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        concat!(
            "Background jobs started by bash with background: true. wait blocks until the job exits, a new output line matches until (a regex), or timeout seconds pass ",
            "(default 300, max 600); it returns the exit code or matched line and the output tail. Use it instead of sleep. ",
            "list shows jobs with their PIDs; kill stops a job and its child processes. ",
            "screenshot (Windows) shows the window of a job that runs a GUI app or game as an image: keys (e.g. \"Right Right Tab\", \"Shift+Tab\", \"Esc\") are pressed in it first, ",
            "then it waits wait seconds (default 1) and captures it; use it to look at what the program shows. ",
            "size (e.g. \"1920x1080\") resizes the window first, to check the program fills a window of another size; several sizes (\"1280x720 1920x1080\") take one shot each in the same call, and kill: true stops the job after the shots, so one call does the whole look. ",
            "The word shot in keys takes a picture at that point, so one call can walk the program: keys \"shot Right shot G shot Tab shot Esc\" gives the start screen, then one after each step ",
            "(a shot is also taken after the last keys); the first size applies to those, each further size adds one shot at the end. ",
            "A window that is not the size asked for is reported as an error (it does not resize, or it snapped back)."
        )
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::Moderate
    }

    fn parameter_schema(&self) -> Value {
        crate::schema(SCHEMA)
    }

    // The host's per-tool timeout must not cut a wait short; the wait enforces its own.
    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::VERBATIM_OUTPUT | ToolCapabilities::LONG_RUNNING
    }

    fn permission_arg_name(&self) -> &str {
        "action"
    }

    fn affected_file_paths(&self, _args: &Value, _ctx: &ToolContext) -> Vec<String> {
        Vec::new()
    }

    fn render_hints(&self) -> Option<ToolRenderHints> {
        Some(ToolRenderHints {
            emoji: "\u{23F3}",
            short_description: "Job",
            summary_arg_keys: &["action", "id", "until"],
            aliases: &[],
        })
    }

    fn prepare_arguments(&self, arguments_json: &str) -> Option<Result<String, String>> {
        Some(crate::prepare(self.name(), SCHEMA, arguments_json))
    }

    async fn execute(&self, args: &Value, _ctx: &ToolContext) -> ToolResult {
        let action = get_string(args, "action").map(|a| a.trim().to_lowercase());
        match action.as_deref() {
            Some("list") => ToolResult::success(list_text(&self.jobs.list())),
            Some("wait" | "monitor") => self.wait(args).await,
            Some("kill" | "stop") => self.kill(args).await,
            Some("screenshot" | "screen" | "look") => self.screenshot(args),
            _ => fail_with(
                "action must be one of: wait, list, kill, screenshot.",
                ToolErrorCode::InvalidArguments,
            ),
        }
    }
}

impl PiJobTool {
    #[allow(clippy::result_large_err)]
    fn find(&self, args: &Value) -> Result<Arc<PiJob>, ToolResult> {
        let id = get_string(args, "id")
            .map(str::to_string)
            .or_else(|| get_number(args, "id").map(|n| n.to_string()));
        let Some(id) = id.filter(|i| !i.trim().is_empty()) else {
            let running: Vec<Arc<PiJob>> = self
                .jobs
                .list()
                .into_iter()
                .filter(|j| !j.has_exited())
                .collect();
            if running.len() == 1 {
                return Ok(running[0].clone());
            }
            return Err(fail_with(
                format!(
                    "Give the job id (from bash background: true).\n{}",
                    list_text(&self.jobs.list())
                ),
                ToolErrorCode::InvalidArguments,
            ));
        };
        self.jobs.get(Some(&id)).ok_or_else(|| {
            fail_with(
                format!("No job \"{id}\".\n{}", list_text(&self.jobs.list())),
                ToolErrorCode::InvalidArguments,
            )
        })
    }

    async fn wait(&self, args: &Value) -> ToolResult {
        let job = match self.find(args) {
            Ok(j) => j,
            Err(e) => return e,
        };
        let until = match get_string(args, "until").filter(|p| !p.is_empty()) {
            Some(pattern) => match Regex::new(pattern) {
                Ok(r) => Some(r),
                Err(e) => {
                    return fail_with(
                        format!("Invalid until regex: {e}"),
                        ToolErrorCode::InvalidArguments,
                    );
                }
            },
            None => None,
        };
        let seconds = match get_number(args, "timeout") {
            Some(t) if t.is_finite() && t > 0.0 => t.min(MAX_WAIT_SECONDS as f64),
            _ => DEFAULT_WAIT_SECONDS as f64,
        };
        let result = self
            .jobs
            .wait(&job, until.as_ref(), Duration::from_secs_f64(seconds))
            .await;
        let waited = duration(result.waited);
        match result.outcome {
            WaitOutcome::Matched => ToolResult::success(describe(
                &job,
                &format!(
                    "Job {} matched at line {} after {waited}: {}\nIt is {}.",
                    job.id,
                    result.matched_line_number.unwrap_or(0),
                    result.matched_line.unwrap_or_default(),
                    if job.has_exited() {
                        job.status()
                    } else {
                        "still running".into()
                    }
                ),
                RUNNING_TAIL_LINES,
            )),
            WaitOutcome::Exited => {
                let code = job.exit_code();
                let header = if job.killed() {
                    format!(
                        "Job {} was killed after {}.",
                        job.id,
                        duration(job.runtime())
                    )
                } else {
                    format!(
                        "Job {} exited with code {} after {}{}",
                        job.id,
                        code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
                        duration(job.runtime()),
                        if until.is_some() {
                            " (no line matched until)."
                        } else {
                            "."
                        }
                    )
                };
                let text = describe(&job, &header, EXIT_TAIL_LINES);
                if code == Some(0) && !job.killed() {
                    ToolResult::success(text)
                } else {
                    fail(text)
                }
            }
            WaitOutcome::TimedOut => ToolResult::success(describe(
                &job,
                &format!(
                    "Job {} is still running after waiting {waited} (pid {}, running {}){} Wait again or kill it.",
                    job.id,
                    job.pid,
                    duration(job.runtime()),
                    if until.is_some() {
                        ", no new line matched."
                    } else {
                        "."
                    }
                ),
                RUNNING_TAIL_LINES,
            )),
        }
    }

    /// `screenshot`: the C# argument checks, then a clear "not available" error (window capture
    /// needs Win32 FFI; see the module docs).
    fn screenshot(&self, args: &Value) -> ToolResult {
        if !cfg!(windows) {
            return fail("screenshot works on Windows only.");
        }
        let job = match self.find(args) {
            Ok(j) => j,
            Err(e) => return e,
        };
        let segments = match shot_segments(get_string(args, "keys")) {
            Ok(s) => s,
            Err(bad) => {
                return fail_with(
                    format!(
                        "Unknown key \"{bad}\". Use letters, digits, Left Right Up Down Tab Enter Esc Space Backspace Home End PageUp PageDown F1-F12, or Shift+/Ctrl+/Alt+ combos, separated by spaces."
                    ),
                    ToolErrorCode::InvalidArguments,
                );
            }
        };
        let sizes = match parse_sizes(get_string(args, "size")) {
            Ok(s) => s,
            Err(bad) => {
                return fail_with(
                    format!(
                        "size \"{bad}\" is not a window size: use WIDTHxHEIGHT, e.g. \"1920x1080\" (200-7680 x 150-4320); separate several sizes with spaces."
                    ),
                    ToolErrorCode::InvalidArguments,
                );
            }
        };
        let total_shots = segments.len() + sizes.len().saturating_sub(1);
        if total_shots > MAX_SHOTS {
            return fail_with(
                format!(
                    "That is {total_shots} screenshots in one call; at most {MAX_SHOTS}. Drop some shot steps or sizes."
                ),
                ToolErrorCode::InvalidArguments,
            );
        }
        let _ = get_bool(args, "kill");
        fail(format!(
            "Could not capture job {}'s window: window capture is not available in this build of Abhoth.",
            job.id
        ))
    }

    async fn kill(&self, args: &Value) -> ToolResult {
        let job = match self.find(args) {
            Ok(j) => j,
            Err(e) => return e,
        };
        if !self.jobs.kill(&job).await {
            return ToolResult::success(format!(
                "Job {} had already {} (pid {}).",
                job.id,
                job.status(),
                job.pid
            ));
        }
        ToolResult::success(describe(
            &job,
            &format!(
                "Killed job {} (pid {}) and its child processes.",
                job.id, job.pid
            ),
            RUNNING_TAIL_LINES,
        ))
    }
}

/// keys split at the word "shot": the keys before each picture. No marker = one step; keys
/// after the last marker get a picture too; a leading "shot" is a picture of the start screen.
/// `Err(bad token)` for an unknown key.
pub fn shot_segments(text: Option<&str>) -> Result<Vec<Vec<PiKey>>, String> {
    let tokens: Vec<&str> = text
        .unwrap_or("")
        .split([' ', ',', ';'])
        .filter(|t| !t.is_empty())
        .collect();
    let mut segments = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut marked = false;
    for token in tokens {
        if token.eq_ignore_ascii_case("shot") {
            marked = true;
            segments.push(window_capture::parse_keys(Some(&current.join(" ")))?);
            current.clear();
        } else {
            current.push(token);
        }
    }
    if !marked || !current.is_empty() {
        segments.push(window_capture::parse_keys(Some(&current.join(" ")))?);
    }
    Ok(segments)
}

/// Why a shot is not at the size asked for (`None` when it is, or none was asked).
pub fn size_mismatch(
    asked: Option<(i32, i32)>,
    got: Option<(i32, i32)>,
    shot: &PiWindowShot,
) -> Option<String> {
    let a = asked?;
    let near = |x: (i32, i32), y: (i32, i32)| (x.0 - y.0).abs() <= 2 && (x.1 - y.1).abs() <= 2;
    if let Some(g) = got
        && !near(g, a)
    {
        return Some(format!(
            "asked for {}x{}, the window would only go to {}x{}",
            a.0, a.1, g.0, g.1
        ));
    }
    let actual = if shot.window_width > 0 && shot.window_height > 0 {
        (shot.window_width, shot.window_height)
    } else {
        (shot.width, shot.height)
    };
    if near(actual, a) {
        None
    } else {
        Some(format!(
            "asked for {}x{}, the window was {}x{} when the picture was taken",
            a.0, a.1, actual.0, actual.1
        ))
    }
}

/// size: one or more WIDTHxHEIGHT separated by spaces or commas (empty when unset);
/// `Err(bad)` when one is not a size or there are too many.
pub fn parse_sizes(text: Option<&str>) -> Result<Vec<(i32, i32)>, String> {
    let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
        return Ok(Vec::new());
    };
    if let Some(single) = window_capture::parse_size(Some(text)) {
        return Ok(vec![single]);
    }
    let mut list = Vec::new();
    for piece in text.split([' ', ',', ';']).filter(|p| !p.is_empty()) {
        if list.len() >= MAX_SIZES {
            return Err(text.to_string());
        }
        match window_capture::parse_size(Some(piece)) {
            Some(size) => list.push(size),
            None => return Err(piece.to_string()),
        }
    }
    Ok(list)
}

/// A header line, then the last lines of the job's output and where the rest is.
pub fn describe(job: &PiJob, header: &str, tail_lines: u64) -> String {
    let mut sb = header.to_string();
    let output = job.output();
    if output.is_empty() {
        sb.push_str("\n(no output)");
        return sb;
    }
    let tail = truncate::tail(&output, TAIL_BYTES, tail_lines);
    sb.push_str("\n\n");
    sb.push_str(&tail.content);
    if tail.truncated {
        sb.push_str(&format!(
            "\n\n[Last {} of {} lines. Full output: {}]",
            tail.output_lines,
            job.total_lines(),
            job.log_path.display()
        ));
        if job.has_exited()
            && let Some(summary) = crate::test_summary::from(&output)
        {
            sb.push_str("\n\n");
            sb.push_str(&summary);
        }
    }
    sb
}

pub fn list_text(jobs: &[Arc<PiJob>]) -> String {
    if jobs.is_empty() {
        return "No background jobs.".into();
    }
    let mut sb = String::from("Jobs:");
    for job in jobs {
        let mut command = crate::write::replace_line_endings_lf(&job.command)
            .replace('\n', " ")
            .trim()
            .to_string();
        let units: Vec<u16> = command.encode_utf16().collect();
        if units.len() > 100 {
            command = format!("{}...", String::from_utf16_lossy(&units[..100]));
        }
        sb.push_str(&format!(
            "\n{}  pid {}  {}  {}  {command}",
            job.id,
            job.pid,
            job.status(),
            duration(job.runtime())
        ));
    }
    sb
}

/// "12s" or "3m 4s" (rounded half to even, as `Math.Round`).
pub fn duration(span: Duration) -> String {
    let s = round_half_even(span.as_secs_f64()) as i64;
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m {}s", s / 60, s % 60)
    }
}

pub(crate) fn round_half_even(v: f64) -> f64 {
    let r = v.round();
    if (v - v.trunc()).abs() == 0.5 && r % 2.0 != 0.0 {
        r - v.signum()
    } else {
        r
    }
}

/// The checks after a screenshot line: the size it got, a blank window, an undrawn strip.
pub fn shot_notes(
    shot: &PiWindowShot,
    asked: Option<(i32, i32)>,
    got: Option<(i32, i32)>,
) -> String {
    let mut text = String::new();
    if let (Some(a), Some(g)) = (asked, got) {
        if (g.0 - a.0).abs() <= 2 && (g.1 - a.1).abs() <= 2 {
            text.push_str(&format!(" Resized the window to {}x{} first.", g.0, g.1));
        } else {
            text.push_str(&format!(
                " Asked for {}x{}; the window would only go to {}x{}.",
                a.0, a.1, g.0, g.1
            ));
        }
    }
    if shot.dominant_share > 0.97 {
        text.push_str(&format!(
            " Warning: {}% of it is one colour, so the window looks blank.",
            round_half_even(shot.dominant_share * 100.0) as i64
        ));
    } else if shot.looks_undrawn() {
        text.push_str(&format!(
            " Warning: the right {} px and bottom {} px of the {}x{} window are one flat colour while the top-left {}x{} has content: the program seems to draw at a fixed size instead of the window's. Check how it handles resizing.",
            shot.undrawn_right,
            shot.undrawn_bottom,
            shot.window_width,
            shot.window_height,
            shot.window_width - shot.undrawn_right,
            shot.window_height - shot.undrawn_bottom
        ));
    }
    text
}
