//! Long-task signals in a turn's tool history (port of `OpenOrca.Cli.Repl.LongTaskMonitor`):
//! per-command test results (counts, failing tests, error signature), progress across a window
//! of iterations, and stuck probing streaks. Deterministic — it only reads what tools actually
//! returned.
//!
//! The C# monitor holds a live reference to the session's tool history. Here the monitor owns
//! only the turn start, the mutation predicate, the workspace directory and the parse cache;
//! every method that reads the history takes it as `&[ToolCallRecord]`, so one instance can
//! live for the whole turn while the history grows.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use orca_tools_common::{NEWLINE, utf16_len};
use regex::Regex;

use super::run_evidence::{self, CommandKind, RunOutcome};
use super::session::ToolCallRecord;

/// One test/build run seen in the tool history, with what its output says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestRunInfo {
    /// Position in the session tool history.
    pub index: usize,
    /// Normalised test command (pipes, redirections and verbosity flags removed).
    pub key: String,
    /// Pass or Fail (runs with no signal are not recorded).
    pub outcome: RunOutcome,
    /// Passing test count when the output states it.
    pub passed: Option<i32>,
    /// Failing (failures + errors) count when the output states it.
    pub failed: Option<i32>,
    /// Names of the failing tests the output lists, sorted (ordinal).
    pub failing_tests: Vec<String>,
    /// First error line, digits normalised — the fallback signature.
    pub first_error: Option<String>,
}

impl TestRunInfo {
    pub fn is_fail(&self) -> bool {
        self.outcome == RunOutcome::Fail
    }

    /// Short human/model-readable description, e.g. "2 failing (test_a, test_b)".
    pub fn describe(&self) -> String {
        if !self.is_fail() {
            return match self.passed {
                Some(p) => format!("passing ({p} tests)"),
                None => "passing".to_string(),
            };
        }
        let mut sb = match self.failed {
            Some(f) => format!("{f} failing"),
            None => "failing".to_string(),
        };
        if !self.failing_tests.is_empty() {
            let names: Vec<&str> = self
                .failing_tests
                .iter()
                .take(3)
                .map(|n| short_test_name(n))
                .collect();
            sb.push_str(" (");
            sb.push_str(&names.join(", "));
            if self.failing_tests.len() > 3 {
                sb.push_str(", …");
            }
            sb.push(')');
        } else if let Some(e) = &self.first_error {
            sb.push_str(": ");
            sb.push_str(e);
        }
        sb
    }
}

fn short_test_name(name: &str) -> &str {
    match name.find(' ') {
        Some(cut) if cut > 0 => &name[..cut],
        _ => name,
    }
}

/// Whether the last window of a turn shows measurable progress, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressVerdict {
    pub progress: bool,
    pub reason: String,
}

impl ProgressVerdict {
    fn new(progress: bool, reason: impl Into<String>) -> Self {
        Self {
            progress,
            reason: reason.into(),
        }
    }
}

/// A "probing in circles" streak: tool calls since the first failing test run that no file
/// change, passing run, or changed failure has interrupted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StuckStreak {
    pub calls: usize,
    pub anchor_index: usize,
    pub failure: TestRunInfo,
    pub recent_probes: Vec<String>,
}

/// Whether a tool record is a successful file modification.
pub type MutationPredicate<'a> = Box<dyn Fn(&ToolCallRecord) -> bool + Send + Sync + 'a>;

/// Reads a turn's tool history for long-task signals. One instance per turn; test-run parses
/// are cached by history index.
pub struct LongTaskMonitor<'a> {
    turn_start: usize,
    is_mutation: MutationPredicate<'a>,
    work_dir: String,
    cache: HashMap<usize, Option<TestRunInfo>>,
}

impl<'a> LongTaskMonitor<'a> {
    /// Iterations added per budget extension; also the window progress is judged over.
    pub const EXTENSION_ITERATIONS: usize = 25;

    /// `turn_start`: index of this turn's first tool call. `is_mutation`: whether a record is a
    /// successful file modification. `work_dir`: writes outside this directory (scratch files in
    /// /tmp) are not code changes; `None` means the current directory.
    pub fn new(
        turn_start: usize,
        is_mutation: impl Fn(&ToolCallRecord) -> bool + Send + Sync + 'a,
        work_dir: Option<&Path>,
    ) -> Self {
        let dir = match work_dir {
            Some(d) => d.to_path_buf(),
            None => std::env::current_dir().unwrap_or_default(),
        };
        let full = get_full_path(&dir.to_string_lossy())
            .unwrap_or_else(|| dir.to_string_lossy().into_owned());
        Self {
            turn_start,
            is_mutation: Box::new(is_mutation),
            work_dir: trim_ending_directory_separator(full),
            cache: HashMap::new(),
        }
    }

    fn start(&self, history: &[ToolCallRecord]) -> usize {
        self.turn_start.min(history.len())
    }

    /// A successful modification of a file in the workspace (scratch files elsewhere don't count).
    pub fn is_code_change(&self, history: &[ToolCallRecord], index: usize) -> bool {
        let rec = &history[index];
        if !(self.is_mutation)(rec) {
            return false;
        }
        let path = arg_string(rec.arguments.as_deref(), &["path", "file_path", "filePath"]);
        let Some(path) = path.filter(|p| !is_null_or_white_space(p)) else {
            return true;
        };
        if !is_path_rooted(&path) {
            return true;
        }
        let Some(full) = get_full_path(&path) else {
            return true;
        };
        starts_with_ignore_case(
            &full,
            &format!("{}{}", self.work_dir, std::path::MAIN_SEPARATOR),
        ) || starts_with_ignore_case(&full, &format!("{}/", self.work_dir))
    }

    /// The test run recorded at `index`, or `None` when it isn't one.
    pub fn test_run_at(&mut self, history: &[ToolCallRecord], index: usize) -> Option<TestRunInfo> {
        if let Some(cached) = self.cache.get(&index) {
            return cached.clone();
        }
        let parsed = Self::parse_test_run(&history[index], index);
        self.cache.insert(index, parsed.clone());
        parsed
    }

    /// This turn's test runs from `from` on, oldest first.
    pub fn test_runs(&mut self, history: &[ToolCallRecord], from: usize) -> Vec<TestRunInfo> {
        (from.max(self.start(history))..history.len())
            .filter_map(|i| self.test_run_at(history, i))
            .collect()
    }

    /// The most recent test run this turn, if any.
    pub fn latest_test_run(&mut self, history: &[ToolCallRecord]) -> Option<TestRunInfo> {
        (self.start(history)..history.len())
            .rev()
            .find_map(|i| self.test_run_at(history, i))
    }

    /// Progress since history index `window_start`: a test command got better (fail→pass, fewer
    /// failures, more passes), a new test command passes, or files changed without leaving the
    /// latest failure exactly as it was before the window.
    pub fn assess_progress(
        &mut self,
        history: &[ToolCallRecord],
        window_start: usize,
    ) -> ProgressVerdict {
        let window_start = window_start.clamp(self.start(history), history.len());
        let all = self.test_runs(history, 0);
        let in_window: Vec<&TestRunInfo> = all.iter().filter(|r| r.index >= window_start).collect();

        let code_changes = (window_start..history.len())
            .filter(|&i| self.is_code_change(history, i))
            .count();

        let reference_for = |key: &str| -> &TestRunInfo {
            all.iter()
                .rev()
                .find(|r| r.key == key && r.index < window_start)
                .or_else(|| in_window.iter().copied().find(|r| r.key == key))
                .expect("key comes from a run in the window")
        };

        for key in distinct(in_window.iter().map(|r| r.key.as_str())) {
            let latest = in_window
                .iter()
                .rev()
                .find(|r| r.key == key)
                .expect("key comes from a run in the window");
            let reference = reference_for(key);
            if reference.index == latest.index {
                if !latest.is_fail() {
                    return ProgressVerdict::new(true, format!("`{key}` passes"));
                }
                continue;
            }
            if Self::is_better(latest, reference) {
                return ProgressVerdict::new(
                    true,
                    format!("`{key}`: {} → {}", reference.describe(), latest.describe()),
                );
            }
        }

        if code_changes == 0 {
            return ProgressVerdict::new(false, "no file changes and no test improvement");
        }

        let Some(last) = in_window.last().filter(|l| l.is_fail()) else {
            return ProgressVerdict::new(true, format!("{code_changes} file change(s)"));
        };
        let last_ref = reference_for(&last.key);
        if last_ref.index == last.index || !Self::same_failure(last, last_ref) {
            return ProgressVerdict::new(
                true,
                format!("{code_changes} file change(s), failure changed"),
            );
        }
        ProgressVerdict::new(
            false,
            format!(
                "{code_changes} file change(s) but `{}` still {}",
                last.key,
                last.describe()
            ),
        )
    }

    /// The current probing streak: tool calls since the first failing test run that has not
    /// been interrupted by a workspace file change, a passing run, or a different failure.
    /// `None` when the latest test run passes or there is none (exploration is never "stuck").
    pub fn current_stuck_streak(&mut self, history: &[ToolCallRecord]) -> Option<StuckStreak> {
        let latest = self.latest_test_run(history).filter(TestRunInfo::is_fail)?;

        let mut anchor = latest.index;
        for i in (self.start(history)..latest.index).rev() {
            if self.is_code_change(history, i) {
                break;
            }
            let Some(run) = self.test_run_at(history, i) else {
                continue;
            };
            if !run.is_fail() || run.key != latest.key || !Self::same_failure(&run, &latest) {
                break;
            }
            anchor = i;
        }

        // A code change after the latest failing run starts a fresh attempt.
        if (latest.index + 1..history.len()).any(|i| self.is_code_change(history, i)) {
            return None;
        }

        let calls = history.len() - 1 - anchor;
        let mut probes: Vec<String> = (anchor + 1..history.len())
            .rev()
            .take(5)
            .map(|i| describe_call(&history[i]))
            .collect();
        probes.reverse();
        Some(StuckStreak {
            calls,
            anchor_index: anchor,
            failure: latest,
            recent_probes: probes,
        })
    }

    /// Deterministic facts for the exhaustion handoff: files changed, the latest result of each
    /// test command, and the latest failure output. Lines end with the platform newline.
    pub fn build_handoff_facts<I, S>(
        &mut self,
        history: &[ToolCallRecord],
        files_modified: I,
    ) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut sb = String::new();
        let files: Vec<String> = files_modified
            .into_iter()
            .take(15)
            .map(|f| f.as_ref().to_string())
            .collect();
        sb.push_str("- Files modified: ");
        sb.push_str(&if files.is_empty() {
            "(none)".to_string()
        } else {
            files.join(", ")
        });
        sb.push_str(NEWLINE);
        let runs = self.test_runs(history, 0);
        if runs.is_empty() {
            sb.push_str("- Test runs this turn: none");
            sb.push_str(NEWLINE);
            return sb;
        }
        sb.push_str("- Latest result per test command:");
        sb.push_str(NEWLINE);
        let keys: Vec<&str> = distinct(runs.iter().map(|r| r.key.as_str()));
        for key in &keys[keys.len().saturating_sub(8)..] {
            let last = runs
                .iter()
                .rev()
                .find(|r| r.key == *key)
                .expect("key comes from a run");
            sb.push_str("  - `");
            sb.push_str(key);
            sb.push_str("`: ");
            sb.push_str(&last.describe());
            sb.push_str(NEWLINE);
        }
        if let Some(latest_fail) = runs.last().filter(|r| r.is_fail()) {
            let result = history[latest_fail.index].result.as_deref().unwrap_or("");
            let output = STREAM_PREFIX.replace_all(result, "");
            for line in [
                "- Latest failing output (excerpt):",
                "```text",
                &run_evidence::excerpt(output.trim(), 300, 900),
                "```",
            ] {
                sb.push_str(line);
                sb.push_str(NEWLINE);
            }
        }
        sb
    }

    /// Whether two failing runs show the same failure. Only a shared signal that differs counts
    /// as "different" — a tail-only and a head-only view of one run must still match.
    pub fn same_failure(a: &TestRunInfo, b: &TestRunInfo) -> bool {
        if !a.is_fail() || !b.is_fail() {
            return false;
        }
        let mut compared = false;
        if let (Some(fa), Some(fb)) = (a.failed, b.failed) {
            if fa != fb {
                return false;
            }
            compared = true;
        }
        if !a.failing_tests.is_empty() && !b.failing_tests.is_empty() {
            if a.failing_tests != b.failing_tests {
                return false;
            }
            compared = true;
        }
        if !compared && let (Some(ea), Some(eb)) = (&a.first_error, &b.first_error) {
            return ea == eb;
        }
        true
    }

    /// Whether `now` is a measurable improvement over `before`.
    pub fn is_better(now: &TestRunInfo, before: &TestRunInfo) -> bool {
        if before.is_fail() && !now.is_fail() {
            return true;
        }
        if !now.is_fail() {
            // C# lifted `int?` comparison: false when either side is null.
            return matches!((now.passed, before.passed), (Some(n), Some(b)) if n > b);
        }
        if let (Some(fn_), Some(fb)) = (now.failed, before.failed)
            && fn_ < fb
        {
            return true;
        }
        if let (Some(pn), Some(pb)) = (now.passed, before.passed)
            && pn > pb
            && !matches!((now.failed, before.failed), (Some(n), Some(b)) if n > b)
        {
            return true;
        }
        !now.failing_tests.is_empty()
            && before.failing_tests.len() > now.failing_tests.len()
            && now
                .failing_tests
                .iter()
                .all(|t| before.failing_tests.contains(t))
    }

    /// Parse a history record as a test/build run; `None` when it isn't one or gave no signal.
    pub fn parse_test_run(rec: &ToolCallRecord, index: usize) -> Option<TestRunInfo> {
        if !is_command_runner(&rec.name) {
            return None;
        }
        let command = arg_string(rec.arguments.as_deref(), &["command", "cmd"])
            .or_else(|| rec.arguments.clone())
            .unwrap_or_default();
        // Only a real test-runner/build invocation is a test run — `cat tests/test_x.py` is not.
        let kind = run_evidence::classify_command(Some(&command));
        if !matches!(kind, CommandKind::Test | CommandKind::Build) {
            return None;
        }
        let outcome = run_evidence::classify(rec.result.as_deref(), rec.is_error, Some(&command));
        if outcome == RunOutcome::None {
            return None;
        }

        let output = STREAM_PREFIX.replace_all(rec.result.as_deref().unwrap_or(""), "");
        let (passed, mut failed) = Self::parse_counts(&output);
        // A passing TEST run must show it: a pass summary or a positive pass count. A bare
        // exit 0 (or "Ran 0 tests ... OK") proves nothing, and passes buy iteration extensions.
        if outcome == RunOutcome::Pass
            && kind == CommandKind::Test
            && (passed == Some(0)
                || (passed.is_none() && !run_evidence::has_pass_summary(Some(&output))))
        {
            return None;
        }
        if outcome == RunOutcome::Pass && failed.is_none() {
            failed = Some(0);
        }
        let failing: Vec<String> = if outcome == RunOutcome::Fail {
            let mut names: Vec<String> = FAILING_TEST_LINE
                .captures_iter(&output)
                .filter_map(|c| {
                    (1..=6)
                        .find_map(|g| c.name(&format!("n{g}")))
                        .map(|m| truncate(m.as_str().trim(), 120))
                })
                .collect();
            names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            names.dedup();
            names
        } else {
            Vec::new()
        };
        let first_error = if outcome == RunOutcome::Fail {
            ERROR_LINE
                .find(&output)
                .map(|m| truncate(&DIGITS.replace_all(m.as_str().trim(), "N"), 160))
        } else {
            None
        };
        if !failing.is_empty()
            && failed.is_none()
            && outcome == RunOutcome::Fail
            && has_complete_listing(&output)
        {
            failed = Some(failing.len() as i32);
        }
        Some(TestRunInfo {
            index,
            key: Self::normalize_key(&command),
            outcome,
            passed,
            failed,
            failing_tests: failing,
            first_error,
        })
    }

    /// Passing and failing counts stated by a runner's output (unittest, dotnet, TAP, then the
    /// generic "3 passed, 1 failed" form). A count that does not fit an `i32` (where C#
    /// `int.Parse` would throw) is ignored.
    pub fn parse_counts(output: &str) -> (Option<i32>, Option<i32>) {
        // unittest: "Ran 7 tests in 0.002s" + "FAILED (failures=2, errors=1)" or "OK".
        if let Some(total) = UNITTEST_RAN
            .captures(output)
            .and_then(|c| parse_count(&c[1]))
        {
            if let Some(uf) = UNITTEST_FAILED.captures(output) {
                let bad: i32 = UNITTEST_COUNT
                    .captures_iter(&uf[1])
                    .filter(|m| matches!(&m[1], "failures" | "errors" | "unexpected successes"))
                    .filter_map(|m| parse_count(&m[2]))
                    .fold(0i32, i32::saturating_add);
                return (Some((total - bad).max(0)), Some(bad));
            }
            return (Some(total), Some(0));
        }

        // dotnet: "Failed!  - Failed:     2, Passed:    10"
        if let Some(dn) = DOTNET_SUMMARY.captures(output)
            && let (Some(p), Some(f)) = (parse_count(&dn["p"]), parse_count(&dn["f"]))
        {
            return (Some(p), Some(f));
        }

        // node --test (tap): "# pass 5" / "# fail 2"
        let mut tap_pass = None;
        let mut tap_fail = None;
        for m in TAP_COUNT.captures_iter(output) {
            let Some(n) = parse_count(&m[2]) else {
                continue;
            };
            if &m[1] == "pass" {
                tap_pass = Some(n);
            } else {
                tap_fail = Some(n);
            }
        }
        if tap_pass.is_some() || tap_fail.is_some() {
            return (tap_pass, tap_fail);
        }

        // pytest / jest / mocha / cargo: "3 passed, 1 failed", "Tests: 1 failed, 3 passed", "2 errors"
        let mut passed = None;
        let mut failed: Option<i32> = None;
        for m in GENERIC_COUNT.captures_iter(output) {
            let Some(n) = parse_count(&m[1]) else {
                continue;
            };
            if m[2]
                .get(..4)
                .is_some_and(|p| p.eq_ignore_ascii_case("pass"))
            {
                passed = Some(n);
            } else {
                failed = Some(failed.unwrap_or(0).saturating_add(n));
            }
        }
        (passed, failed)
    }

    /// Command with pipes, redirections and verbosity flags removed — runs of one target share a key.
    pub fn normalize_key(command: &str) -> String {
        let segments: Vec<&str> = SEGMENT_SPLIT.split(command).collect();
        let seg = segments
            .iter()
            .copied()
            .find(|s| run_evidence::is_test_or_build_command(Some(s)))
            .or_else(|| segments.first().copied())
            .unwrap_or(command);
        let seg = REDIRECTION.replace_all(seg, " ");
        let seg = strip_verbosity_flags(&seg);
        let seg = WHITESPACE.replace_all(&seg, " ");
        truncate(seg.trim(), 100)
    }
}

/// Items in first-seen order without repeats (LINQ `Distinct`).
fn distinct<'s>(items: impl Iterator<Item = &'s str>) -> Vec<&'s str> {
    let mut out: Vec<&str> = Vec::new();
    for item in items {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

/// unittest prints a summary after its listing; without one the listing may be cut off.
fn has_complete_listing(output: &str) -> bool {
    UNITTEST_RAN.is_match(output)
}

fn parse_count(s: &str) -> Option<i32> {
    s.parse().ok()
}

fn describe_call(rec: &ToolCallRecord) -> String {
    let detail = arg_string(
        rec.arguments.as_deref(),
        &["command", "cmd", "path", "file_path", "pattern"],
    )
    .unwrap_or_default();
    let mut first_line = detail.split('\n').next().unwrap_or("").trim().to_string();
    if detail.contains('\n') {
        first_line.push_str(" …");
    }
    format!("{}: {}", rec.name, truncate(&first_line, 90))
}

/// The first of `names` that is a string property of the JSON object `args_json`.
fn arg_string(args_json: Option<&str>, names: &[&str]) -> Option<String> {
    let args = args_json?;
    if is_null_or_white_space(args) || !args.starts_with('{') {
        return None;
    }
    let serde_json::Value::Object(map) = serde_json::from_str::<serde_json::Value>(args).ok()?
    else {
        return None;
    };
    names
        .iter()
        .find_map(|n| map.get(*n).and_then(|v| v.as_str()).map(str::to_string))
}

fn is_command_runner(name: &str) -> bool {
    matches!(
        name,
        "bash" | "shell" | "run_command" | "execute_command" | "get_process_output" | "run_tests"
    )
}

/// C# `s.Length <= max ? s : s[..(max - 1)] + "…"` with lengths in UTF-16 code units.
fn truncate(s: &str, max: usize) -> String {
    if utf16_len(s) <= max {
        return s.to_string();
    }
    let units: Vec<u16> = s.encode_utf16().take(max - 1).collect();
    format!("{}…", String::from_utf16_lossy(&units))
}

fn is_null_or_white_space(s: &str) -> bool {
    s.chars().all(char::is_whitespace)
}

/// .NET `Path.IsPathRooted`.
fn is_path_rooted(path: &str) -> bool {
    if cfg!(windows) {
        let b = path.as_bytes();
        matches!(b.first(), Some(b'/' | b'\\'))
            || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
    } else {
        path.starts_with('/')
    }
}

/// .NET `Path.GetFullPath`: absolute, with `.` and `..` resolved lexically. `None` where .NET
/// would throw.
fn get_full_path(path: &str) -> Option<String> {
    if path.is_empty() || path.contains('\0') {
        return None;
    }
    let abs = std::path::absolute(path).ok()?;
    if cfg!(windows) {
        // GetFullPathNameW already resolved `.`/`..` and normalised separators.
        return Some(abs.to_string_lossy().into_owned());
    }
    let mut out = std::path::PathBuf::new();
    for c in abs.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    Some(out.to_string_lossy().into_owned())
}

/// .NET `Path.TrimEndingDirectorySeparator`: drops one trailing separator unless the path is a root.
fn trim_ending_directory_separator(mut path: String) -> String {
    let ends_with_sep = path.ends_with('/') || (cfg!(windows) && path.ends_with('\\'));
    if ends_with_sep && Path::new(&path).parent().is_some() {
        path.pop();
    }
    path
}

/// `StartsWith(prefix, StringComparison.OrdinalIgnoreCase)` with simple (one-to-one) case folding.
fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    fn fold(c: char) -> char {
        let mut up = c.to_uppercase();
        match (up.next(), up.next()) {
            (Some(u), None) => u,
            _ => c,
        }
    }
    let mut chars = s.chars();
    prefix
        .chars()
        .all(|p| chars.next().is_some_and(|c| fold(c) == fold(p)))
}

/// Replaces `\s(?:-v|-vv|-q|--verbose|--quiet|-x|--tb=\S+)(?=\s|$)` with a space, left to right.
/// Hand-written: the `regex` crate has no lookahead, and the lookahead does not consume the
/// following whitespace (so `-v -q` strips both).
fn strip_verbosity_flags(s: &str) -> String {
    const FLAGS: [&str; 6] = ["-v", "-vv", "-q", "--verbose", "--quiet", "-x"];
    let at_boundary = |rest: &str| rest.chars().next().is_none_or(char::is_whitespace);
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let c = s[i..].chars().next().expect("i is on a char boundary");
        let after = i + c.len_utf8();
        if c.is_whitespace() {
            let rest = &s[after..];
            let mut end = FLAGS
                .iter()
                .filter(|f| rest.starts_with(**f) && at_boundary(&rest[f.len()..]))
                .map(|f| after + f.len())
                .next();
            if end.is_none()
                && let Some(value) = rest.strip_prefix("--tb=")
            {
                let run = value.find(char::is_whitespace).unwrap_or(value.len());
                if run > 0 {
                    end = Some(after + "--tb=".len() + run);
                }
            }
            if let Some(end) = end {
                out.push(' ');
                i = end;
                continue;
            }
        }
        out.push(c);
        i = after;
    }
    out
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid built-in regex")
}

static STREAM_PREFIX: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^\[std(err|out)\] ?"));
static UNITTEST_RAN: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^\s*Ran\s+(\d+)\s+tests?\b"));
static UNITTEST_FAILED: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^\s*FAILED\s+\(([^)]*)\)"));
static UNITTEST_COUNT: LazyLock<Regex> =
    LazyLock::new(|| re(r"(failures|errors|skipped|expected failures|unexpected successes)=(\d+)"));
static DOTNET_SUMMARY: LazyLock<Regex> =
    LazyLock::new(|| re(r"Failed:\s+(?<f>\d+),\s+Passed:\s+(?<p>\d+)"));
static TAP_COUNT: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^#\s+(pass|fail)\s+(\d+)\s*$"));
static GENERIC_COUNT: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b(\d+)\s+(passed|passing|failed|failing|errors?)\b"));
// unittest "FAIL: test_x (mod.Class.test_x)", pytest "FAILED tests/t.py::test_x - ...",
// dotnet "  Failed Ns.Class.Test [12 ms]", go "--- FAIL: TestX", tap "not ok 3 - name".
// The C# pattern reuses one group name (`name`) per alternative; Rust needs distinct names.
static FAILING_TEST_LINE: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r"(?m)^(?:(?:FAIL|ERROR):\s+(?<n1>\S+(?:\s+\([^)\r\n]*\))?)",
        r"|FAILED\s+(?<n2>\S+::\S+)",
        r"|ERROR\s+(?<n3>\S+::\S+)",
        r"|\s*Failed\s+(?<n4>[\w.`<>]+)\s+\[",
        r"|---\s+FAIL:\s+(?<n5>\S+)",
        r"|not\s+ok\s+\d+\s+-\s+(?<n6>.+?)\s*$)",
    ))
});
static ERROR_LINE: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?m)^\s*(?:[\w.]*(?:Error|Exception)\b(?::.*)?|.*\berror\s+(?:CS|TS)\d{4}\b.*)$")
});
static DIGITS: LazyLock<Regex> = LazyLock::new(|| re(r"\d+"));
static SEGMENT_SPLIT: LazyLock<Regex> = LazyLock::new(|| re(r"\s*(?:&&|\|\||;|\||\r?\n)\s*"));
static REDIRECTION: LazyLock<Regex> = LazyLock::new(|| re(r"\s\d?>&\d|\s\d?>>?\s*\S+"));
static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| re(r"\s+"));

#[cfg(test)]
mod tests {
    //! Port of `tests/OpenOrca.Cli.Tests/LongTaskMonitorTests.cs`.
    use super::*;
    use serde_json::json;

    const STAGE1: &str = "python -m unittest tests.test_stage1";

    fn bash(command: &str, result: &str) -> ToolCallRecord {
        bash_err(command, result, false)
    }

    fn bash_err(command: &str, result: &str, is_error: bool) -> ToolCallRecord {
        ToolCallRecord {
            name: "bash".into(),
            arguments: Some(json!({ "command": command }).to_string()),
            result: Some(result.into()),
            is_error,
            duration_ms: 0,
        }
    }

    fn write(path: &str) -> ToolCallRecord {
        ToolCallRecord {
            name: "write".into(),
            arguments: Some(json!({ "path": path, "content": "x" }).to_string()),
            result: Some("ok".into()),
            is_error: false,
            duration_ms: 0,
        }
    }

    fn read(path: &str) -> ToolCallRecord {
        ToolCallRecord {
            name: "read".into(),
            arguments: Some(json!({ "path": path }).to_string()),
            result: Some("contents".into()),
            is_error: false,
            duration_ms: 0,
        }
    }

    fn unittest_fail(total: i32, failures: i32, errors: i32, failing: &[&str]) -> String {
        let lines: Vec<String> = failing
            .iter()
            .map(|f| format!("FAIL: {f} (tests.test_stage1.T.{f})\n----\nAssertionError: boom\n"))
            .collect();
        format!(
            "{}\n----------------------------------------------------------------------\nRan {total} tests in 0.002s\n\nFAILED (failures={failures}, errors={errors})\n",
            lines.join("\n")
        )
    }

    fn unittest_ok(total: i32) -> String {
        format!(
            "----------------------------------------------------------------------\nRan {total} tests in 0.002s\n\nOK\n"
        )
    }

    fn monitor() -> LongTaskMonitor<'static> {
        let cwd = std::env::current_dir().unwrap();
        LongTaskMonitor::new(
            0,
            |r: &ToolCallRecord| !r.is_error && matches!(r.name.as_str(), "write" | "edit"),
            Some(&cwd),
        )
    }

    // ── parsing ──

    #[test]
    fn parse_counts_unittest_failures_and_errors_sum() {
        let (passed, failed) = LongTaskMonitor::parse_counts(&unittest_fail(7, 2, 2, &[]));
        assert_eq!(passed, Some(3));
        assert_eq!(failed, Some(4));
    }

    #[test]
    fn parse_counts_unittest_ok_zero_failed() {
        assert_eq!(
            LongTaskMonitor::parse_counts(&unittest_ok(7)),
            (Some(7), Some(0))
        );
    }

    #[test]
    fn parse_counts_other_runners() {
        for (output, passed, failed) in [
            ("===== 3 passed, 1 failed in 0.1s =====", 3, 1),
            ("Failed!  - Failed:     2, Passed:    10, Skipped: 0", 10, 2),
            ("# tests 7\n# pass 5\n# fail 2\n", 5, 2),
        ] {
            assert_eq!(
                LongTaskMonitor::parse_counts(output),
                (Some(passed), Some(failed)),
                "{output}"
            );
        }
    }

    #[test]
    fn normalize_key_strips_pipes_redirects_and_verbosity() {
        for (command, key) in [
            (
                "python -m unittest tests.test_stage1 -v 2>&1 | tail -5",
                STAGE1,
            ),
            (
                "touch tests/__init__.py && python -m unittest tests.test_stage1 2>&1 | head -40",
                STAGE1,
            ),
            (
                "python -m unittest discover -s tests",
                "python -m unittest discover -s tests",
            ),
        ] {
            assert_eq!(LongTaskMonitor::normalize_key(command), key, "{command}");
        }
    }

    #[test]
    fn parse_test_run_diagnostic_probe_is_not_a_test_run() {
        let probe = bash_err(
            "python - <<'EOF'\nfrom mini import tokenize\nprint(tokenize('\"hi\"'))\nEOF",
            "Traceback (most recent call last):\nMiniError: unterminated string",
            true,
        );
        assert!(LongTaskMonitor::parse_test_run(&probe, 0).is_none());
    }

    #[test]
    fn parse_test_run_unittest_failure_records_key_counts_and_names() {
        let run = LongTaskMonitor::parse_test_run(
            &bash(
                &format!("{STAGE1} 2>&1 | tail -20"),
                &unittest_fail(7, 2, 0, &["test_a", "test_b"]),
            ),
            3,
        )
        .expect("a test run");
        assert!(run.is_fail());
        assert_eq!(run.key, STAGE1);
        assert_eq!(run.failed, Some(2));
        assert_eq!(run.passed, Some(5));
        assert_eq!(run.failing_tests.len(), 2);
        assert!(run.describe().contains("2 failing"));
    }

    #[test]
    fn same_failure_tail_only_and_head_only_views_of_one_run_match() {
        let tail = LongTaskMonitor::parse_test_run(
            &bash(
                &format!("{STAGE1} | tail -3"),
                "Ran 7 tests in 0.002s\n\nFAILED (failures=2, errors=2)",
            ),
            0,
        )
        .unwrap();
        let head = LongTaskMonitor::parse_test_run(
            &bash(
                &format!("{STAGE1} | head -40"),
                "ERROR: test_strings (tests.test_stage1.T.test_strings)\nTraceback (most recent call last):\nMiniError: bad",
            ),
            1,
        )
        .unwrap();
        assert!(LongTaskMonitor::same_failure(&tail, &head));
    }

    #[test]
    fn same_failure_different_counts_differ() {
        let a = LongTaskMonitor::parse_test_run(&bash(STAGE1, &unittest_fail(7, 2, 0, &[])), 0)
            .unwrap();
        let b = LongTaskMonitor::parse_test_run(&bash(STAGE1, &unittest_fail(7, 1, 0, &[])), 1)
            .unwrap();
        assert!(!LongTaskMonitor::same_failure(&a, &b));
        assert!(LongTaskMonitor::is_better(&b, &a));
        assert!(!LongTaskMonitor::is_better(&a, &b));
    }

    // ── progress ──

    #[test]
    fn assess_progress_failures_decreasing_is_progress() {
        let h = vec![
            bash(
                "python -m unittest tests.test_stage4",
                &unittest_fail(8, 0, 14, &[]),
            ),
            write("mini/__init__.py"),
            bash(
                "python -m unittest tests.test_stage4",
                &unittest_fail(8, 0, 2, &[]),
            ),
        ];
        let v = monitor().assess_progress(&h, 0);
        assert!(v.progress, "{}", v.reason);
    }

    #[test]
    fn assess_progress_improvement_measured_against_run_before_window() {
        let h = vec![
            bash(STAGE1, &unittest_fail(7, 3, 0, &[])),
            write("mini/__init__.py"),
            bash(STAGE1, &unittest_fail(7, 1, 0, &[])), // window starts here
        ];
        assert!(monitor().assess_progress(&h, 2).progress);
    }

    #[test]
    fn assess_progress_probing_with_same_failure_is_not_progress() {
        let mut h = vec![bash(STAGE1, &unittest_fail(7, 2, 2, &[]))];
        for i in 0..10 {
            h.push(bash(&format!("python -c \"print({i})\""), &i.to_string()));
        }
        h.push(bash(
            &format!("{STAGE1} 2>&1 | tail -3"),
            &unittest_fail(7, 2, 2, &[]),
        ));
        let v = monitor().assess_progress(&h, 1);
        assert!(!v.progress, "{}", v.reason);
    }

    #[test]
    fn assess_progress_edits_that_leave_the_same_failure_are_not_progress() {
        let h = vec![
            bash(STAGE1, &unittest_fail(7, 1, 0, &["test_errors"])),
            write("mini/__init__.py"),
            bash(STAGE1, &unittest_fail(7, 1, 0, &["test_errors"])),
            write("mini/__init__.py"),
            bash(STAGE1, &unittest_fail(7, 1, 0, &["test_errors"])),
        ];
        let v = monitor().assess_progress(&h, 1);
        assert!(!v.progress, "{}", v.reason);
        // Exact reason text (C# wording).
        assert_eq!(
            v.reason,
            format!("2 file change(s) but `{STAGE1}` still 1 failing (test_errors)")
        );
    }

    #[test]
    fn assess_progress_new_stage_passing_is_progress() {
        let h = vec![
            read("tests/test_stage3.py"),
            write("mini/__init__.py"),
            bash("python -m unittest tests.test_stage3", &unittest_ok(4)),
        ];
        assert!(monitor().assess_progress(&h, 0).progress);
    }

    // Regression: a benchmark run got an iteration extension for "`cat tests/test_stage4.py` passes".
    #[test]
    fn assess_progress_reading_a_test_file_via_bash_is_not_a_test_run_or_progress() {
        let source = "import unittest\n\nclass T(unittest.TestCase):\n    def test_x(self):\n        self.assertEqual(f(), 'OK')\n";
        for command in [
            "cat tests/test_stage4.py",
            "head -60 tests/test_stage4.py",
            "grep -n 'def test' tests/test_stage4.py",
            "sed -n '1,120p' tests/test_stage4.py",
            "cat tests/test_stage4.py | head -40",
        ] {
            let h = vec![read("SPEC.md"), bash(command, source)];
            assert!(
                LongTaskMonitor::parse_test_run(&h[1], 1).is_none(),
                "{command}"
            );
            let v = monitor().assess_progress(&h, 0);
            assert!(!v.progress, "{command}: {}", v.reason);
            assert!(monitor().latest_test_run(&h).is_none(), "{command}");
        }
    }

    #[test]
    fn assess_progress_real_runner_passing_is_progress() {
        for (command, output) in [
            ("cd proj && pytest -q", "===== 5 passed in 0.10s ====="),
            ("python -m pytest -q 2>&1 | tail -5", "5 passed in 0.10s"),
            ("npm test", "Tests:       5 passed, 5 total"),
            ("node --test", "# tests 5\n# pass 5\n# fail 0\n"),
            ("go test ./...", "ok  \texample.com/pkg\t0.012s"),
        ] {
            let h = vec![bash(command, output)];
            assert!(
                LongTaskMonitor::parse_test_run(&h[0], 0).is_some(),
                "{command}"
            );
            assert!(monitor().assess_progress(&h, 0).progress, "{command}");
        }
    }

    #[test]
    fn parse_test_run_pass_without_test_result_output_is_no_signal() {
        for (command, output) in [
            ("python -m unittest tests.test_stage4", ""), // exit 0, nothing recognisable
            (
                "python -m unittest tests.test_stage4",
                "----------------------------------------------------------------------\nRan 0 tests in 0.000s\n\nOK\n",
            ),
        ] {
            assert!(
                LongTaskMonitor::parse_test_run(&bash(command, output), 0).is_none(),
                "{output}"
            );
        }
    }

    #[test]
    fn assess_progress_file_changes_without_tests_is_progress() {
        let h = vec![read("SPEC.md"), write("mini/__init__.py")];
        assert!(monitor().assess_progress(&h, 0).progress);
    }

    #[test]
    fn assess_progress_only_reads_is_not_progress() {
        let h = vec![read("SPEC.md"), read("a.py"), bash("ls", "a b")];
        assert!(!monitor().assess_progress(&h, 0).progress);
    }

    // ── stuck streak ──

    #[test]
    fn stuck_streak_exploration_before_any_failing_run_is_null() {
        let h: Vec<ToolCallRecord> = (0..20).map(|i| read(&format!("file{i}.py"))).collect();
        assert!(monitor().current_stuck_streak(&h).is_none());
    }

    #[test]
    fn stuck_streak_probes_after_failure_counts_calls_since_first_matching_failure() {
        let mut h = vec![
            write("mini/__init__.py"),
            bash(&format!("{STAGE1} | tail -5"), &unittest_fail(7, 2, 2, &[])),
            bash(
                &format!("{STAGE1} | head -40"),
                "FAIL: test_errors (tests.test_stage1.T.test_errors)\nAssertionError: x",
            ),
        ];
        for i in 0..6 {
            h.push(bash(
                &format!("python - <<'EOF'\nprint({i})\nEOF"),
                &i.to_string(),
            ));
        }
        let streak = monitor().current_stuck_streak(&h).expect("a streak");
        assert_eq!(streak.calls, 7); // the head re-run + 6 probes after the first failing run
        assert_eq!(streak.anchor_index, 1);
        assert_eq!(streak.recent_probes.len(), 5);
        assert_eq!(streak.recent_probes[4], "bash: python - <<'EOF' …");
    }

    #[test]
    fn stuck_streak_code_change_after_failure_resets_streak() {
        let mut h = vec![bash(STAGE1, &unittest_fail(7, 2, 0, &[]))];
        for i in 0..5 {
            h.push(bash(&format!("python -c \"print({i})\""), "x"));
        }
        h.push(write("mini/__init__.py"));
        assert!(monitor().current_stuck_streak(&h).is_none());
    }

    #[test]
    fn stuck_streak_scratch_file_outside_workspace_does_not_reset() {
        let scratch = std::env::temp_dir().join("probe_t.py");
        let scratch = scratch.to_string_lossy();
        let h = vec![
            bash(STAGE1, &unittest_fail(7, 2, 0, &[])),
            write(&scratch),
            bash(&format!("python {scratch}"), "x"),
        ];
        let streak = monitor().current_stuck_streak(&h).expect("a streak");
        assert_eq!(streak.calls, 2);
    }

    #[test]
    fn stuck_streak_latest_run_passes_is_null() {
        let h = vec![
            bash(STAGE1, &unittest_fail(7, 2, 0, &[])),
            bash(STAGE1, &unittest_ok(7)),
            read("x"),
        ];
        assert!(monitor().current_stuck_streak(&h).is_none());
    }

    #[test]
    fn stuck_streak_failure_changed_anchors_at_new_failure() {
        let h = vec![
            bash(STAGE1, &unittest_fail(7, 2, 0, &[])),
            bash("python -c 1", "x"),
            bash(STAGE1, &unittest_fail(7, 1, 0, &[])), // changed (e.g. edited via bash)
            bash("python -c 2", "x"),
        ];
        let streak = monitor().current_stuck_streak(&h).expect("a streak");
        assert_eq!(streak.anchor_index, 2);
        assert_eq!(streak.calls, 1);
    }

    // ── handoff facts ──

    #[test]
    fn build_handoff_facts_lists_files_latest_result_per_command_and_failure_excerpt() {
        let h = vec![
            bash("python -m unittest tests.test_stage1", &unittest_ok(7)),
            bash(
                "python -m unittest tests.test_stage2 2>&1 | tail -20",
                &unittest_fail(7, 1, 0, &["test_syntax_errors"]),
            ),
        ];
        let facts = monitor().build_handoff_facts(&h, ["mini/__init__.py"]);
        assert!(facts.contains("mini/__init__.py"));
        assert!(facts.contains("`python -m unittest tests.test_stage1`: passing"));
        assert!(
            facts.contains("`python -m unittest tests.test_stage2`: 1 failing (test_syntax_errors")
        );
        assert!(facts.contains("FAILED (failures=1"));
        // Exact header lines (C# wording, platform newlines).
        let nl = NEWLINE;
        assert!(facts.starts_with(&format!(
            "- Files modified: mini/__init__.py{nl}- Latest result per test command:{nl}  - `python -m unittest tests.test_stage1`: passing (7 tests){nl}"
        )));
        assert!(facts.contains(&format!(
            "- Latest failing output (excerpt):{nl}```text{nl}FAIL:"
        )));
        assert!(facts.ends_with(&format!("FAILED (failures=1, errors=0){nl}```{nl}")));
    }

    // ── stuck steer wording ──
    //
    // StuckProbeMessage_PiProfile_NamesOnlyPiTools, StuckProbeMessage_Neutral_NamesNoTools and
    // StuckProbeMessage_SecondStrike_ForbidsDiagnostics test `PromptConstants.GetStuckProbeMessage`,
    // not the monitor; they move with that prompt constant when the prompt system ports it
    // (orca-core::agent::prompts has no stuck-probe message yet).

    // ── extra checks on the hand-written parts ──

    #[test]
    fn strip_verbosity_flags_matches_the_csharp_lookahead() {
        assert_eq!(strip_verbosity_flags("pytest -v -q x"), "pytest   x");
        assert_eq!(strip_verbosity_flags("pytest -vv"), "pytest ");
        assert_eq!(
            strip_verbosity_flags("pytest -vx --tb=short -x"),
            "pytest -vx  "
        );
        assert_eq!(strip_verbosity_flags("pytest --tb= x"), "pytest --tb= x");
        assert_eq!(strip_verbosity_flags("-v pytest"), "-v pytest");
    }

    #[test]
    fn describe_lists_at_most_three_short_names_or_the_error() {
        let mut run = TestRunInfo {
            index: 0,
            key: "k".into(),
            outcome: RunOutcome::Fail,
            passed: None,
            failed: Some(4),
            failing_tests: vec!["a (x)".into(), "b".into(), "c".into(), "d".into()],
            first_error: Some("E".into()),
        };
        assert_eq!(run.describe(), "4 failing (a, b, c, …)");
        run.failing_tests.clear();
        run.failed = None;
        assert_eq!(run.describe(), "failing: E");
        run.outcome = RunOutcome::Pass;
        assert_eq!(run.describe(), "passing");
    }

    #[test]
    fn first_error_normalises_digits_and_failing_names_sort_ordinally() {
        let run = LongTaskMonitor::parse_test_run(
            &bash_err(
                "pytest -q",
                "FAILED tests/t.py::test_b - x\nFAILED tests/t.py::test_a - y\nValueError: bad 42 at 7\n",
                true,
            ),
            0,
        )
        .unwrap();
        assert_eq!(
            run.failing_tests,
            vec!["tests/t.py::test_a", "tests/t.py::test_b"]
        );
        assert_eq!(run.first_error.as_deref(), Some("ValueError: bad N at N"));
    }

    #[test]
    fn truncate_counts_utf16_units() {
        assert_eq!(truncate("abcdef", 6), "abcdef");
        assert_eq!(truncate("abcdefg", 6), "abcde…");
    }
}
