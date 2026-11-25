//! `PiTestSummary`: a one-line pass/fail digest of a Python test run (unittest or pytest) whose
//! output bash truncated to its tail: the names of failing tests sit above their tracebacks, so
//! the tail often loses them.

use std::sync::LazyLock;

use regex::Regex;

/// At most this many failing tests are named.
pub const MAX_NAMES: usize = 12;

static UNITTEST_FAILURE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(FAIL|ERROR): (\S+) \(([^)]+)\)").unwrap());
static PYTEST_SHORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(FAILED|ERROR) (\S+::\S+)").unwrap());
static PYTEST_VERBOSE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\S+::\S+) (FAILED|ERROR)\b").unwrap());
static UNITTEST_RAN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Ran \d+ tests? in ").unwrap());
static UNITTEST_RESULT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(OK|FAILED)( \(.*\))?\s*$").unwrap());
static PYTEST_RESULT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^=+ (.*\b(passed|failed|error|errors)\b.*) =+\s*$").unwrap());

/// "[Test summary (whole output): FAILED (failures=2) after Ran 40 tests in 0.3s. Failing: a, b]",
/// or `None` when the output is not a unittest/pytest run with a result line.
pub fn from(output: &str) -> Option<String> {
    let mut names: Vec<String> = Vec::new();
    let mut ran: Option<String> = None;
    let mut result: Option<String> = None;
    let add = |names: &mut Vec<String>, n: String| {
        if !names.contains(&n) {
            names.push(n);
        }
    };
    for raw in output.split('\n') {
        let line = raw.trim_end_matches('\r');
        if let Some(m) = UNITTEST_FAILURE.captures(line) {
            add(&mut names, format!("{} ({})", &m[2], &m[3]));
        } else if let Some(m) = PYTEST_SHORT.captures(line) {
            add(&mut names, m[2].to_string());
        } else if let Some(m) = PYTEST_VERBOSE.captures(line) {
            add(&mut names, m[1].to_string());
        } else if UNITTEST_RAN.is_match(line) {
            ran = Some(line.trim().to_string());
        } else if UNITTEST_RESULT.is_match(line) && ran.is_some() {
            result = Some(line.trim().to_string());
        } else if let Some(m) = PYTEST_RESULT.captures(line) {
            result = Some(m[1].trim().to_string());
        }
    }
    let result = result?;
    let mut sb = format!("[Test summary (whole output): {result}");
    if let Some(ran) = ran {
        sb.push_str(" after ");
        sb.push_str(&ran);
    }
    if !names.is_empty() {
        sb.push_str(". Failing: ");
        sb.push_str(
            &names
                .iter()
                .take(MAX_NAMES)
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
        );
        if names.len() > MAX_NAMES {
            sb.push_str(&format!(" (+{} more)", names.len() - MAX_NAMES));
        }
    }
    sb.push(']');
    Some(sb)
}
