//! Classifies shell output as evidence of a passing or failing test/build run (port of
//! `OpenOrca.Cli.Repl.RunEvidence`).
//!
//! Summary lines live at the END of test output, so callers must hand in head+tail, not just
//! the head. This is the ONE place that decides whether a command is a test run — the long-task
//! monitor (progress / iteration extensions) and the tool-call executor (verification,
//! red-suite refusal) both go through it.

use std::sync::LazyLock;

use regex::Regex;

/// Outcome signalled by a command's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunOutcome {
    /// No test/build signal either way.
    None,
    /// A test/build run that passed, or a code-running command that exited cleanly.
    Pass,
    /// A test/build run that failed.
    Fail,
}

/// What a command line does, as far as test/build evidence goes. Ordered: a stronger kind wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CommandKind {
    /// Reads, searches, edits, installs, navigates — runs nothing that exercises the code.
    None,
    /// Runs code (a script, a binary, `python -c`) but isn't a test runner.
    RunsCode,
    /// A build / compile / type-check (dotnet build, tsc, make, cargo check).
    Build,
    /// A real test-runner invocation (pytest, unittest, node --test, npm test, dotnet test, ...).
    Test,
}

/// Default head length (UTF-16 code units) kept by [`excerpt`].
pub const EXCERPT_HEAD_CHARS: usize = 400;
/// Default tail length (UTF-16 code units) kept by [`excerpt`].
pub const EXCERPT_TAIL_CHARS: usize = 1200;

/// Classify a command execution. `command` is the command line (or the tool's raw JSON
/// arguments); `is_error` is the tool's error flag (non-zero exit for bash). A known command
/// that neither tests, builds nor runs code (`cat`, `head`, `grep`, `sed`, an editor, ...) never
/// signals either way: `cat tests/test_x.py` prints test SOURCE, not a test result. With no
/// command (unknown), only the output is judged.
pub fn classify(output: Option<&str>, is_error: bool, command: Option<&str>) -> RunOutcome {
    let command_line = extract_command_line(command);
    let kind = classify_command(command_line.as_deref());
    if command_line.is_some() && kind == CommandKind::None {
        return RunOutcome::None;
    }

    // BashTool prefixes stderr lines with "[stderr] " — unittest prints its summary
    // ("Ran 3 tests ... OK") on stderr, which a line-anchored "OK" never matched.
    let output = STREAM_PREFIX.replace_all(output.unwrap_or(""), "");
    if FAIL_PATTERN.is_match(&output) {
        return RunOutcome::Fail;
    }

    if is_error {
        return if matches!(kind, CommandKind::Test | CommandKind::Build) {
            RunOutcome::Fail
        } else {
            RunOutcome::None
        };
    }

    if PASS_PATTERN.is_match(&output) {
        return RunOutcome::Pass;
    }

    // A test/build command — or a command that runs the code — exiting 0 is itself
    // evidence the code works, even when the runner prints nothing recognisable.
    if let Some(cl) = &command_line
        && !INSTALL_OR_INFO_COMMAND.is_match(cl)
    {
        return RunOutcome::Pass;
    }

    RunOutcome::None
}

/// Whether a command line (or a tool's JSON arguments) runs tests or a build (pytest,
/// unittest, dotnet test, npm test, ...).
pub fn is_test_or_build_command(command: Option<&str>) -> bool {
    matches!(
        classify_command(extract_command_line(command).as_deref()),
        CommandKind::Test | CommandKind::Build
    )
}

/// Whether output carries a recognisable pass summary ("5 passed", "Ran 3 tests ... OK",
/// "Build succeeded", ...).
pub fn has_pass_summary(output: Option<&str>) -> bool {
    PASS_PATTERN.is_match(&STREAM_PREFIX.replace_all(output.unwrap_or(""), ""))
}

/// The command line in `command_or_args`: the string itself, or its `command`/`cmd` property
/// when it is a tool's JSON arguments. `None` when there is no command (none, blank, or JSON
/// without one — e.g. `get_process_output`).
pub fn extract_command_line(command_or_args: Option<&str>) -> Option<String> {
    let s = command_or_args?;
    if is_null_or_white_space(s) {
        return None;
    }
    let trimmed = s.trim_start();
    if !trimmed.starts_with('{') {
        return Some(s.to_string());
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(serde_json::Value::Object(map)) => {
            for name in ["command", "cmd"] {
                if let Some(serde_json::Value::String(v)) = map.get(name) {
                    return if is_null_or_white_space(v) {
                        None
                    } else {
                        Some(v.clone())
                    };
                }
            }
            None
        }
        Ok(_) => None,
        // Not JSON after all (a brace group) — treat it as a command line.
        Err(_) => Some(s.to_string()),
    }
}

/// What a shell command line does, judged by the PROGRAM each segment runs (segments split on
/// `&&`, `||`, `;`, `|`, `&` and newlines); the strongest segment wins. So `cd x && pytest -q`
/// is a test run, while `cat tests/test_x.py | head` is not: a test file's NAME appearing as an
/// argument to a reader/searcher/editor never makes a command a test run.
pub fn classify_command(command_line: Option<&str>) -> CommandKind {
    let Some(command_line) = command_line else {
        return CommandKind::None;
    };
    if is_null_or_white_space(command_line) {
        return CommandKind::None;
    }
    segment_split(command_line)
        .into_iter()
        .map(|segment| classify_segment(&tokenize(segment), 0))
        .max()
        .unwrap_or(CommandKind::None)
}

fn classify_segment(tokens: &[String], depth: u32) -> CommandKind {
    if depth > 3 {
        return CommandKind::None;
    }

    // Skip env assignments and wrappers that don't change what runs: FOO=1, env, sudo, time, timeout 60, ...
    let mut i = 0;
    while i < tokens.len() {
        let p = program(&tokens[i]);
        if ENV_ASSIGNMENT.is_match(&tokens[i])
            || matches!(
                p.as_str(),
                "env"
                    | "sudo"
                    | "time"
                    | "command"
                    | "exec"
                    | "nohup"
                    | "nice"
                    | "xvfb-run"
                    | "("
                    | "{"
            )
        {
            i += 1;
        } else if matches!(p.as_str(), "timeout" | "gtimeout") {
            i += 1;
            while i < tokens.len() && tokens[i].starts_with('-') {
                i += 1;
            }
            if i < tokens.len() {
                i += 1; // the duration
            }
        } else {
            break;
        }
    }
    if i >= tokens.len() {
        return CommandKind::None;
    }

    let program = program(&tokens[i]);
    let args = &tokens[i + 1..];
    if args.iter().any(|a| a == "--version" || a == "--help") {
        return CommandKind::None;
    }
    let arg = |n: usize| -> String { args.get(n).map(|a| a.to_lowercase()).unwrap_or_default() };
    let rest = |from: usize| -> CommandKind {
        classify_segment(args.get(from..).unwrap_or(&[]), depth + 1)
    };
    let script_or_code = |candidates: &[String]| -> CommandKind {
        match candidates.iter().find(|a| !a.starts_with('-')) {
            Some(script) if is_test_script(script) => CommandKind::Test,
            _ => CommandKind::RunsCode,
        }
    };

    if PYTHON_EXE.is_match(&program) {
        let mut j = 0;
        while j < args.len()
            && args[j].starts_with('-')
            && !matches!(args[j].as_str(), "-m" | "-c" | "-")
        {
            j += 1;
        }
        if j >= args.len() || matches!(args[j].as_str(), "-c" | "-") {
            return CommandKind::RunsCode; // REPL, stdin heredoc, inline code
        }
        if args[j] == "-m" {
            let module = arg(j + 1);
            return match module.as_str() {
                "pytest" | "unittest" | "nose" | "nose2" | "tox" | "doctest" => CommandKind::Test,
                "pip" | "venv" | "ensurepip" => CommandKind::None,
                _ => CommandKind::RunsCode,
            };
        }
        return if is_test_script(&args[j]) {
            CommandKind::Test
        } else {
            CommandKind::RunsCode
        };
    }

    match program.as_str() {
        "pytest" | "py.test" | "nose2" | "nosetests" | "tox" | "nox" | "jest" | "vitest"
        | "mocha" | "ava" | "karma" | "phpunit" | "pest" | "rspec" | "ctest" | "busted"
        | "cucumber" | "behave" | "gotestsum" | "bats" | "prove" | "trial" => {
            return CommandKind::Test;
        }

        "tsc" | "msbuild" | "ninja" | "cmake" | "javac" | "gcc" | "g++" | "clang" | "rustc" => {
            return CommandKind::Build;
        }

        "make" | "gmake" | "mvn" | "mvnw" | "gradle" | "gradlew" | "ant" | "sbt" | "rake"
        | "bazel" | "just" => {
            return if args.iter().any(|a| {
                matches!(
                    a.to_lowercase().as_str(),
                    "test" | "tests" | "check" | "verify" | "spec"
                ) || starts_with_ignore_ascii_case(a, "test")
            }) {
                CommandKind::Test
            } else {
                CommandKind::Build
            };
        }

        "node" | "nodejs" | "ts-node" | "tsx" => {
            return if args
                .iter()
                .any(|a| a == "--test" || a.starts_with("--test="))
            {
                CommandKind::Test
            } else {
                script_or_code(args)
            };
        }

        "deno" | "bun" => {
            return match arg(0).as_str() {
                "test" => CommandKind::Test,
                "install" | "add" | "remove" | "upgrade" | "init" => CommandKind::None,
                "x" => rest(1),
                "run" if program == "bun" => package_script(&arg(1)),
                _ => CommandKind::RunsCode,
            };
        }

        "npm" | "yarn" | "pnpm" => {
            return match arg(0).as_str() {
                "test" | "t" | "tst" | "cit" | "install-test" => CommandKind::Test,
                "run" | "run-script" => package_script(&arg(1)),
                "exec" | "dlx" | "x" => rest(1),
                "build" => CommandKind::Build,
                "" | "install" | "i" | "ci" | "add" | "remove" | "uninstall" | "init"
                | "create" | "list" | "ls" | "view" | "info" | "outdated" | "audit" | "config"
                | "why" => CommandKind::None,
                // yarn/pnpm run package scripts (and bin tools) without "run": `yarn test:unit`, `pnpm jest`
                s if program != "npm" && !s.starts_with('-') => match package_script(s) {
                    k @ (CommandKind::Test | CommandKind::Build) => k,
                    _ => rest(0),
                },
                _ => CommandKind::None,
            };
        }

        "npx" | "pnpx" | "bunx" | "uvx" => {
            return match rest(0) {
                CommandKind::None => CommandKind::RunsCode,
                k => k,
            };
        }

        "uv" | "poetry" | "pipenv" | "pdm" | "hatch" | "rye" | "conda" | "mamba" | "pipx" => {
            return match args.iter().position(|a| a == "run") {
                Some(j) => rest(j + 1),
                None => CommandKind::None,
            };
        }

        "bundle" => {
            return if arg(0) == "exec" {
                rest(1)
            } else {
                CommandKind::None
            };
        }

        "rails" => {
            return if matches!(arg(0).as_str(), "test" | "spec") {
                CommandKind::Test
            } else {
                CommandKind::None
            };
        }

        "dotnet" => {
            return match arg(0).as_str() {
                "test" | "vstest" => CommandKind::Test,
                "build" | "msbuild" | "publish" | "pack" => CommandKind::Build,
                "run" | "exec" => CommandKind::RunsCode,
                s if s.ends_with(".dll") => CommandKind::RunsCode,
                _ => CommandKind::None,
            };
        }

        "go" => {
            return match arg(0).as_str() {
                "test" => CommandKind::Test,
                "build" | "vet" => CommandKind::Build,
                "run" => CommandKind::RunsCode,
                _ => CommandKind::None,
            };
        }

        "cargo" => {
            return match arg(0).as_str() {
                "test" | "nextest" | "bench" => CommandKind::Test,
                "build" | "check" | "clippy" => CommandKind::Build,
                "run" => CommandKind::RunsCode,
                _ => CommandKind::None,
            };
        }

        // In C# a failed `when` guard falls through to later case labels: swift/dart reach the
        // script-runner arm below, mix/flutter reach the local-file check.
        "mix" | "dart" | "flutter" | "swift" if arg(0) == "test" => return CommandKind::Test,

        "bash" | "sh" | "zsh" | "dash" | "pwsh" | "powershell" | "cmd" => {
            let j = args.iter().position(|a| {
                matches!(a.to_lowercase().as_str(), "-c" | "-lc" | "-command" | "/c")
            });
            if let Some(j) = j {
                return if j + 1 < args.len() {
                    classify_command(Some(&args[j + 1..].join(" ")))
                } else {
                    CommandKind::None
                };
            }
            return if args.iter().any(|a| !a.starts_with('-')) {
                script_or_code(args)
            } else {
                CommandKind::None
            };
        }

        "ruby" | "php" | "perl" | "java" | "lua" | "julia" | "rscript" | "swift" | "kotlin"
        | "scala" | "elixir" | "dart" => return script_or_code(args),

        _ => {}
    }

    // ./run_tests.sh, ./bin/app, scripts/test.sh — executing a local file runs code.
    let raw = tokens[i].trim_matches(['(', '{', '"', '\'']);
    if raw.starts_with("./") || raw.starts_with(".\\") || SCRIPT_FILE.is_match(raw) {
        return if is_test_script(raw) {
            CommandKind::Test
        } else {
            CommandKind::RunsCode
        };
    }
    CommandKind::None
}

/// A package.json script name: test-ish scripts test, build-ish ones build, others run code.
fn package_script(script: &str) -> CommandKind {
    if starts_with_ignore_ascii_case(script, "test")
        || matches!(script, "check" | "verify" | "ci" | "spec" | "e2e")
    {
        CommandKind::Test
    } else if starts_with_ignore_ascii_case(script, "build")
        || matches!(script, "compile" | "typecheck" | "tsc")
    {
        CommandKind::Build
    } else if !script.is_empty() {
        CommandKind::RunsCode
    } else {
        CommandKind::None
    }
}

/// A script whose NAME marks it as a test runner: test_x.py, tests.py, x_test.go, x.test.js, run_tests.sh.
fn is_test_script(path: &str) -> bool {
    TEST_SCRIPT_NAME.is_match(path)
}

/// Lower-cased program name: grouping/quote chars, directory and .exe/.cmd/.bat stripped.
fn program(token: &str) -> String {
    let t = token.trim_matches(['(', ')', '{', '}', '"', '\'', '`']);
    if t.is_empty() {
        return token.to_string();
    }
    let t = match t.rfind(['/', '\\']) {
        Some(slash) => &t[slash + 1..],
        None => t,
    };
    let mut t = t.to_lowercase();
    for ext in [".exe", ".cmd", ".bat"] {
        if t.ends_with(ext) {
            t.truncate(t.len() - ext.len());
        }
    }
    t
}

/// Whitespace-split tokens with simple single/double quoting removed.
fn tokenize(segment: &str) -> Vec<String> {
    TOKEN
        .find_iter(segment)
        .map(|m| {
            let v = m.as_str();
            let b = v.as_bytes();
            if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
                v[1..v.len() - 1].to_string()
            } else {
                v.to_string()
            }
        })
        .collect()
}

/// Whether a fenced code block (info string `language`, contents `body`) is quoted
/// command/test OUTPUT rather than source the model should have written with a tool: an
/// output-ish fence (text/console/output/log), a test/build pass-or-fail summary, or typical
/// runner/prompt lines.
pub fn looks_like_command_output(language: Option<&str>, body: &str) -> bool {
    if is_null_or_white_space(body) {
        return false;
    }
    if OUTPUT_FENCE.is_match(language.unwrap_or("")) {
        return true;
    }
    let text = STREAM_PREFIX.replace_all(body, "");
    PASS_PATTERN.is_match(&text)
        || FAIL_PATTERN.is_match(&text)
        || RUNNER_OUTPUT_LINE.is_match(&text)
}

/// Head + tail excerpt of a long output, so failure summaries at the end survive. Lengths are
/// UTF-16 code units, as in the C# original (defaults: [`EXCERPT_HEAD_CHARS`],
/// [`EXCERPT_TAIL_CHARS`]); the omitted count is formatted with `,` thousands separators.
pub fn excerpt(text: &str, head_chars: usize, tail_chars: usize) -> String {
    let len = orca_tools_common::utf16_len(text);
    if len <= head_chars + tail_chars + 64 {
        return text.to_string();
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    let omitted = len - head_chars - tail_chars;
    format!(
        "{}\n[... {} chars omitted ...]\n{}",
        String::from_utf16_lossy(&units[..head_chars]),
        group_thousands(omitted),
        String::from_utf16_lossy(&units[len - tail_chars..])
    )
}

/// `{n:N0}` in the invariant/en-US culture: `1234567` -> `1,234,567`.
fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (idx, ch) in digits.chars().enumerate() {
        if idx > 0 && (digits.len() - idx).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// C# `string.IsNullOrWhiteSpace` for a non-null string.
fn is_null_or_white_space(s: &str) -> bool {
    s.chars().all(char::is_whitespace)
}

/// C# `s.StartsWith(prefix, StringComparison.OrdinalIgnoreCase)` for an ASCII `prefix`.
fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

/// Splits a command line into segments on `&&`, `||`, `;`, `|`, `|&`, `&` and newlines (with
/// surrounding whitespace) — but not on the `&` of a redirection (`2>&1`, `&>log`).
///
/// Hand-written equivalent of the C# `Regex.Split` with
/// `\s*(?:&&|\|\||;|\|&?|(?<![<>])&(?!>)|\r?\n)\s*`: the Rust `regex` crate has no
/// lookaround, so this emulates the .NET backtracking matcher (leftmost start, greedy leading
/// whitespace that backs off — so `\r`/`\n` can still match the newline alternative —,
/// alternatives tried in order, greedy trailing whitespace), producing identical splits.
fn segment_split(s: &str) -> Vec<&str> {
    let cs: Vec<(usize, char)> = s.char_indices().collect();
    let n = cs.len();
    let byte_at = |i: usize| if i < n { cs[i].0 } else { s.len() };
    let ch = |i: usize| if i < n { Some(cs[i].1) } else { None };

    // One of the separator alternatives starting at char index k; returns its end.
    let alt_at = |k: usize| -> Option<usize> {
        match ch(k)? {
            '&' => {
                if ch(k + 1) == Some('&') {
                    Some(k + 2)
                } else {
                    let prev = if k > 0 { ch(k - 1) } else { None };
                    let after_redirect = matches!(prev, Some('<' | '>'));
                    (!after_redirect && ch(k + 1) != Some('>')).then_some(k + 1)
                }
            }
            '|' => Some(if matches!(ch(k + 1), Some('|' | '&')) {
                k + 2
            } else {
                k + 1
            }),
            ';' | '\n' => Some(k + 1),
            '\r' => (ch(k + 1) == Some('\n')).then_some(k + 2),
            _ => None,
        }
    };
    let is_ws = |i: usize| ch(i).is_some_and(char::is_whitespace);
    let match_at = |p: usize| -> Option<usize> {
        let mut ws_end = p;
        while is_ws(ws_end) {
            ws_end += 1;
        }
        (p..=ws_end).rev().find_map(alt_at).map(|mut e| {
            while is_ws(e) {
                e += 1;
            }
            e
        })
    };

    let mut pieces = Vec::new();
    let (mut last, mut p) = (0, 0);
    while p < n {
        if let Some(e) = match_at(p) {
            pieces.push(&s[byte_at(last)..byte_at(p)]);
            last = e;
            p = e;
        } else {
            p += 1;
        }
    }
    pieces.push(&s[byte_at(last)..]);
    pieces
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid run-evidence regex")
}

// pytest "5 passed", unittest "OK" after "Ran N tests", dotnet "Passed!  - Failed: 0" /
// "Test Run Successful." / "Build succeeded.", jest "Tests: 5 passed", mocha "5 passing",
// go "ok  pkg" / bare "PASS", cargo "test result: ok.", node --test "# pass 5" / "ℹ pass 5",
// generic "all tests passed".
static PASS_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?imx)
          \b\d+\s+passed\b
        | ^\s*Ran\s+\d+\s+tests?\s+in\s+[\d.]+s\s*\r?\n\s*\r?\n?\s*OK\b
        | ^\s*OK(\s*\(skipped=\d+\))?\s*$
        | \bPassed!\s+-\s+Failed:\s+0\b
        | \bTest\s+Run\s+Successful\b
        | \bBuild\s+succeeded\b
        | \b\d+\s+passing\b
        | ^ok\s+\S+
        | ^PASS\s*$
        | \btest\s+result:\s+ok\b
        | ^\s*(\#|ℹ)\s+pass\s+[1-9]\d*\s*$
        | \ball\s+(\d+\s+)?tests?\s+pass(ed)?\b
        | \btests?\s+passed\b
        | \b0\s+failed\b
        ")
});

static FAIL_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?imx)
          \b[1-9]\d*\s+(failed|failing|errors?)\b
        | (?-i:\bFAILED\b)
        | (?-i:^FAIL\b)
        | Traceback\s+\(most\s+recent\s+call\s+last\)
        | \bAssertion\s?Error\b
        | \bTest\s+Run\s+Failed\b
        | \bBuild\s+FAILED\b
        | \berror\s+(CS|TS)\d{4}\b
        | \btest\s+result:\s+FAILED\b
        | \bFailed!\s+-\s+Failed:\s+[1-9]
        | ^\s*(\#|ℹ)\s+fail\s+[1-9]\d*\s*$
        | \btests?\s+failed\b
        ")
});

static TOKEN: LazyLock<Regex> = LazyLock::new(|| re(r#""(?:[^"\\]|\\.)*"|'[^']*'|\S+"#));

static ENV_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Za-z_][A-Za-z0-9_]*="));

static PYTHON_EXE: LazyLock<Regex> = LazyLock::new(|| re(r"^(python|pypy|py)\d*(\.\d+)?w?$"));

static SCRIPT_FILE: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\.(sh|bash|ps1|py|js|mjs|cjs|ts|rb|pl|php)$"));

// test_x.py, tests.py, test.sh, x_test.go, x_tests.py, x.test.js, x.spec.ts, run_tests.sh, runtests.py
static TEST_SCRIPT_NAME: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)(^|[/\\])(test[\w-]*|[\w-]+_tests?|run_?tests?|[\w-]+\.(test|spec))\.\w+$")
});

static INSTALL_OR_INFO_COMMAND: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)\b(pip3?|npm|yarn|pnpm|cargo|dotnet|go|gem|bundle|composer|poetry|uv)\s+(install|uninstall|add|init|new|restore)\b|\bpip3?\b|\s--version\b|\s--help\b",
    )
});

static STREAM_PREFIX: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^\[std(err|out)\] ?"));

static OUTPUT_FENCE: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)^(text|txt|plain|plaintext|console|output|out|log|shell-session|terminal)$")
});

// pytest "===== test session starts =====" / "collected 5 items" / "test_x.py::test_y PASSED",
// unittest "test_x (m.T) ... ok", "Ran 3 tests in", go "--- PASS:". A bare "$ cmd" line is
// deliberately NOT output — a command the model only printed is exactly what nudges catch.
static RUNNER_OUTPUT_LINE: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?imx)
          ^={3,}.*={3,}\s*$
        | ^collected\s+\d+\s+items?
        | ::\S+\s+(PASSED|FAILED|ERROR|SKIPPED)\b
        | \.\.\.\s+(ok|FAIL|ERROR|skipped)\s*$
        | ^Ran\s+\d+\s+tests?\s+in\b
        | ^---\s+(PASS|FAIL):
        ")
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_passing_runner_output_pass() {
        let cases: &[(&str, &str)] = &[
            // pytest
            ("===== 5 passed in 0.12s =====", "pytest -q"),
            ("== 12 passed, 1 warning in 0.4s ==", "python -m pytest"),
            // unittest (summary on stderr, as BashTool reports it)
            (
                "[stderr] ...\n[stderr] ------\n[stderr] Ran 3 tests in 0.000s\n[stderr] \n[stderr] OK",
                "python -m unittest",
            ),
            (
                "Ran 4 tests in 0.001s\n\nOK (skipped=1)",
                "python test_ledger.py",
            ),
            // dotnet
            (
                "Passed!  - Failed:     0, Passed:    28, Skipped:     0, Total:    28",
                "dotnet test",
            ),
            (
                "Build succeeded.\n    0 Warning(s)\n    0 Error(s)",
                "dotnet build",
            ),
            // jest / mocha
            ("Tests:       5 passed, 5 total", "npm test"),
            ("  7 passing (20ms)", "npx mocha"),
            // go
            ("ok  \texample.com/pkg\t0.012s", "go test ./..."),
            ("PASS", "go test"),
            // cargo
            (
                "test result: ok. 9 passed; 0 failed; 0 ignored",
                "cargo test",
            ),
        ];
        for (output, command) in cases {
            assert_eq!(
                RunOutcome::Pass,
                classify(Some(output), false, Some(command)),
                "{command:?} / {output:?}"
            );
        }
    }

    #[test]
    fn classify_failing_runner_output_fail() {
        let cases: &[(&str, &str, bool)] = &[
            ("== 1 failed, 4 passed in 0.2s ==", "pytest", false),
            (
                "[stderr] Ran 10 tests in 0.006s\n[stderr] \n[stderr] FAILED (failures=1, errors=1)",
                "python -m unittest",
                true,
            ),
            (
                "Traceback (most recent call last):\n  File \"x.py\"\nAssertionError",
                "python x.py",
                true,
            ),
            (
                "Failed!  - Failed:     2, Passed:    26",
                "dotnet test",
                true,
            ),
            (
                "Program.cs(3,1): error CS1002: ; expected\nBuild FAILED.",
                "dotnet build",
                true,
            ),
            (
                "--- FAIL: TestAdd (0.00s)\nFAIL\texample.com/pkg",
                "go test",
                true,
            ),
            (
                "test result: FAILED. 8 passed; 1 failed",
                "cargo test",
                true,
            ),
            ("whatever", "pytest", true), // test runner exiting non-zero
        ];
        for (output, command, is_error) in cases {
            assert_eq!(
                RunOutcome::Fail,
                classify(Some(output), *is_error, Some(command)),
                "{command:?} / {output:?}"
            );
        }
    }

    #[test]
    fn classify_non_test_activity_not_fail() {
        let cases: &[(&str, Option<&str>, bool)] = &[
            ("file1.py\nfile2.py", Some("ls"), false),
            ("No such file", Some("cat missing.txt"), true), // non-test command failing isn't a test failure
            (
                "Successfully installed requests",
                Some("pip install requests"),
                false,
            ),
            ("Python 3.12.1", Some("python --version"), false),
            ("0 failed", None, false), // "0 failed" alone is a pass signal, never a failure
        ];
        for (output, command, is_error) in cases {
            assert_ne!(
                RunOutcome::Fail,
                classify(Some(output), *is_error, *command),
                "{command:?}"
            );
            if command.is_some() {
                assert_eq!(
                    RunOutcome::None,
                    classify(Some(output), *is_error, *command),
                    "{command:?}"
                );
            }
        }
    }

    #[test]
    fn classify_script_run_exiting_cleanly_pass() {
        assert_eq!(
            RunOutcome::Pass,
            classify(
                Some("total: 42"),
                false,
                Some("{\"command\":\"python report.py data.csv\"}")
            )
        );
    }

    // ── which commands are test runs (regression: `cat tests/test_stage4.py` was a "passing test run") ──

    const TEST_FILE_SOURCE: &str = "import unittest\n\nclass T(unittest.TestCase):\n    def test_ok(self):\n        self.assertEqual(run(), 'OK')\n        print('all tests passed')\n\nOK\n";

    #[test]
    fn reading_or_searching_test_files_is_not_a_test_run() {
        let commands = [
            "cat tests/test_stage4.py",
            "head -50 tests/test_stage4.py",
            "tail -n 20 tests/test_stage4.py",
            "less tests/test_stage4.py",
            "grep -n 'def test_' tests/test_stage4.py",
            "sed -n '1,80p' tests/test_stage4.py",
            "vim tests/test_stage4.py",
            "nano test_app.js",
            "wc -l tests/test_*.py",
            "ls tests/",
            "cat tests/x.py | head",
            "cat tests/test_stage4.py | head -40",
            "cd proj && cat tests/test_stage4.py",
            "echo pytest",
            "grep -rn pytest .",
            "{\"command\":\"cat tests/test_stage4.py\"}",
        ];
        for command in commands {
            assert!(!is_test_or_build_command(Some(command)), "{command:?}");
            assert_eq!(
                CommandKind::None,
                classify_command(extract_command_line(Some(command)).as_deref()),
                "{command:?}"
            );
            // Test source can contain pass/fail-looking text — it still isn't evidence either way.
            assert_eq!(
                RunOutcome::None,
                classify(Some(TEST_FILE_SOURCE), false, Some(command)),
                "{command:?}"
            );
            assert_eq!(
                RunOutcome::None,
                classify(
                    Some("Traceback (most recent call last):\nAssertionError"),
                    false,
                    Some(command)
                ),
                "{command:?}"
            );
        }
    }

    #[test]
    fn real_test_runner_invocations_are_test_runs() {
        let commands = [
            "python -m unittest tests.test_stage4",
            "python3 -m pytest -q",
            "pytest",
            "pytest -q tests/test_stage4.py",
            "py.test -x",
            "python tests/test_stage4.py",
            "node --test",
            "node --test tests/",
            "node test_app.js",
            "npm test",
            "npm run test:unit",
            "yarn test",
            "pnpm test",
            "dotnet test",
            "go test ./...",
            "cargo test",
            "npx jest",
            "npx vitest run",
            "jest --ci",
            "mocha",
            "uv run pytest",
            "poetry run pytest -q",
            "./run_tests.sh",
            "bash -c \"cd x && pytest -q\"",
            "cd x && pytest -q",
            "cd x && python -m pytest -q 2>&1 | tail -20",
            "PYTHONPATH=. timeout 60 python -m unittest discover -s tests",
            "cat tests/test_stage4.py && pytest -q",
            ".venv/bin/pytest -q",
            "{\"command\":\"cd /work && python -m unittest -q\"}",
        ];
        for command in commands {
            assert!(is_test_or_build_command(Some(command)), "{command:?}");
            assert_eq!(
                CommandKind::Test,
                classify_command(extract_command_line(Some(command)).as_deref()),
                "{command:?}"
            );
        }
    }

    #[test]
    fn build_commands_are_builds() {
        for command in [
            "dotnet build",
            "npm run build",
            "tsc --noEmit",
            "cargo check",
            "go vet ./...",
            "make",
        ] {
            assert_eq!(
                CommandKind::Build,
                classify_command(Some(command)),
                "{command:?}"
            );
            assert!(is_test_or_build_command(Some(command)), "{command:?}");
        }
    }

    #[test]
    fn code_running_commands_run_code_but_are_not_test_runs() {
        let commands = [
            "python report.py data.csv",
            "python -c \"print(1)\"",
            "python - <<'EOF'\nprint(1)\nEOF",
            "node app.js",
            "./bin/app",
            "dotnet run",
        ];
        for command in commands {
            assert_eq!(
                CommandKind::RunsCode,
                classify_command(Some(command)),
                "{command:?}"
            );
            assert!(!is_test_or_build_command(Some(command)), "{command:?}");
        }
    }

    #[test]
    fn install_and_info_commands_are_not_test_runs() {
        for command in [
            "pip install pytest",
            "npm install",
            "python -m pip install -r requirements.txt",
            "pytest --version",
        ] {
            assert!(!is_test_or_build_command(Some(command)), "{command:?}");
        }
    }

    #[test]
    fn classify_cat_of_test_file_exit_zero_is_not_a_pass() {
        assert_eq!(
            RunOutcome::None,
            classify(
                Some("import unittest\n"),
                false,
                Some("cat tests/test_stage4.py")
            )
        );
        assert_eq!(
            RunOutcome::Pass,
            classify(
                Some("Ran 3 tests in 0.001s\n\nOK"),
                false,
                Some("cd x && python -m unittest -q")
            )
        );
    }

    // ── extra checks on the hand-written splitter (no lookaround in Rust regex) and excerpt ──

    #[test]
    fn segment_split_keeps_redirections_together() {
        assert_eq!(segment_split("a 2>&1 | b"), vec!["a 2>&1", "b"]);
        assert_eq!(segment_split("a &>log && b"), vec!["a &>log", "b"]);
        assert_eq!(segment_split("a & b"), vec!["a", "b"]);
        assert_eq!(
            segment_split("a |& b; c || d\r\ne"),
            vec!["a", "b", "c", "d", "e"]
        );
        assert_eq!(segment_split("a \n &>x"), vec!["a", "&>x"]);
        assert_eq!(segment_split("; a ;"), vec!["", "a", ""]);
    }

    #[test]
    fn excerpt_keeps_head_and_tail() {
        let short = "x".repeat(84); // head + tail + 64: still kept whole
        assert_eq!(excerpt(&short, 10, 10), short);
        let long = format!("{}{}{}", "h".repeat(10), "m".repeat(2000), "t".repeat(10));
        assert_eq!(
            excerpt(&long, 10, 10),
            format!(
                "{}\n[... 2,000 chars omitted ...]\n{}",
                "h".repeat(10),
                "t".repeat(10)
            )
        );
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
    }
}
