//! Ported from ToolCallRendererTests.cs, ToolCallRendererPiTests (UiPolishTests.cs) and
//! InlineImageTests.ToolResults_OnlyShortResultsOfferImages. ANSI checks render the result
//! lines' markup with ANSI on (the test console records plain text).

use std::sync::Arc;

use orca_abstractions::Tool;
use orca_core::agent::ToolRegistry;

use super::*;
use crate::ui::markup;

fn registry() -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register_all(orca_tools_fs::all_tools());
    r.register_all(orca_tools_utility::utility_tools());
    r
}

struct Fixture {
    renderer: ToolCallRenderer,
    state: ReplState,
    buf: Arc<std::sync::Mutex<Vec<u8>>>,
    registry: ToolRegistry,
}

impl Fixture {
    fn new() -> Self {
        let (console, buf) = Console::buffer();
        Self {
            renderer: ToolCallRenderer::new(console),
            state: ReplState::default(),
            buf,
            registry: registry(),
        }
    }

    fn output(&self) -> String {
        Console::buffer_text(&self.buf)
    }

    fn result(&mut self, name: &str, result: &str, is_error: bool, args: Option<&str>) {
        let tool = self.registry.resolve(name);
        self.renderer.render_tool_result(
            name,
            result,
            is_error,
            None,
            args,
            tool.as_deref(),
            &self.state,
        );
    }

    fn call(&mut self, name: &str, args: &str) {
        let tool = self.registry.resolve(name);
        self.renderer
            .render_tool_call(name, args, tool.as_deref(), &self.state);
    }

    /// The result as ANSI text.
    fn ansi(&mut self, name: &str, result: &str) -> String {
        let tool = self.registry.resolve(name);
        self.renderer
            .result_lines(name, result, false, None, tool.as_deref(), &self.state)
            .iter()
            .map(|l| markup::render(l, true) + "\n")
            .collect()
    }
}

fn numbered(n: usize) -> String {
    (1..=n)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn render_tool_result_success_few_lines_shows_all_no_truncation() {
    let mut f = Fixture::new();
    f.result("glob", "line1\nline2\nline3", false, None);
    let out = f.output();
    assert!(out.contains("line1") && out.contains("line2") && out.contains("line3"));
    assert!(!out.contains("more line"));
}

#[test]
fn render_tool_result_success_many_lines_truncates_with_hint() {
    let mut f = Fixture::new();
    f.result("glob", &numbered(12), false, None);
    let out = f.output();
    for i in 1..=7 {
        assert!(out.contains(&format!("line{i}")));
    }
    assert!(out.contains("5 more lines"));
    assert!(out.contains("ctrl+o to expand"));
}

#[test]
fn render_tool_result_success_many_lines_show_thinking_shows_all() {
    let mut f = Fixture::new();
    f.state.verbosity = 2;
    f.result("glob", &numbered(12), false, None);
    let out = f.output();
    for i in 1..=12 {
        assert!(out.contains(&format!("line{i}")));
    }
    assert!(!out.contains("more line"));
}

#[test]
fn render_tool_result_error_shows_red_header_and_body_preview() {
    let mut f = Fixture::new();
    f.result(
        "write_file",
        "File not found\ndetail line 1\ndetail line 2",
        true,
        None,
    );
    let out = f.output();
    for s in [
        "write_file",
        "File not found",
        "detail line 1",
        "detail line 2",
    ] {
        assert!(out.contains(s), "{s}");
    }
}

#[test]
fn render_tool_result_empty_result_no_output() {
    let mut f = Fixture::new();
    f.result("read_file", "", false, None);
    assert!(f.output().is_empty());
}

#[test]
fn render_tool_result_whitespace_result_no_output() {
    let mut f = Fixture::new();
    f.result("read_file", "   \n  ", false, None);
    assert!(f.output().is_empty());
}

#[test]
fn render_tool_result_suppressed_no_output() {
    let mut f = Fixture::new();
    f.renderer.suppressed = true;
    f.result("read_file", "some content", false, None);
    assert!(f.output().is_empty());
}

#[test]
fn render_tool_result_exactly_max_lines_no_truncation() {
    let mut f = Fixture::new();
    f.result("glob", &numbered(TOOL_OUTPUT_PREVIEW_LINES), false, None);
    let out = f.output();
    for i in 1..=TOOL_OUTPUT_PREVIEW_LINES {
        assert!(out.contains(&format!("line{i}")));
    }
    assert!(!out.contains("more line"));
}

#[test]
fn render_tool_result_single_more_line_singular_hint() {
    let mut f = Fixture::new();
    f.result(
        "glob",
        &numbered(TOOL_OUTPUT_PREVIEW_LINES + 1),
        false,
        None,
    );
    assert!(f.output().contains("1 more line "));
}

const EDIT_RESULT: &str = "Replaced 1 occurrence(s) in file.cs\n\n--- diff ---\n     1   context\n     2 - old line\n     2 + new line\n     3   context";

#[test]
fn render_tool_result_edit_file_removed_lines_have_red_background() {
    let mut f = Fixture::new();
    f.state.verbosity = 2;
    assert!(f.ansi("edit_file", EDIT_RESULT).contains("48;2;61;0;0"));
}

#[test]
fn render_tool_result_edit_file_added_lines_have_green_background() {
    let mut f = Fixture::new();
    f.state.verbosity = 2;
    assert!(f.ansi("edit_file", EDIT_RESULT).contains("48;2;0;61;0"));
}

#[test]
fn render_tool_result_edit_file_context_lines_are_dim() {
    let mut f = Fixture::new();
    f.state.verbosity = 2;
    let out = f.ansi(
        "edit_file",
        "--- diff ---\n     1   context line\n     2 - old\n     2 + new",
    );
    // Dim is SGR 2 (combined with other attributes here, e.g. ESC[2m).
    assert!(out.contains("\x1b[2m"));
}

#[test]
fn render_tool_result_edit_file_preview_mode_shows_added_lines() {
    let mut f = Fixture::new();
    f.state.verbosity = 1;
    let result = "Replaced 1 occurrence(s) in file.cs\n\n--- diff ---\n     1   ctx1\n     2   ctx2\n     3   ctx3\n     4 - old line\n     4 + new line\n     5   ctx4";
    assert!(f.ansi("edit_file", result).contains("48;2;0;61;0"));
}

#[test]
fn render_tool_result_edit_file_preview_mode_pins_two_context_lines_around_change() {
    let mut f = Fixture::new();
    f.state.verbosity = 1;
    let result = [
        "--- diff ---",
        "     1   ctx1",
        "     2   ctx2",
        "     3   ctx3",
        "     4   ctx4",
        "     5   ctx5",
        "     6   ctx6",
        "     7   ctx7",
        "     8   ctx8",
        "     9   ctx9",
        "    10   ctx10",
        "    11 - old",
        "    11 + new",
        "    12   after1",
        "    13   after2",
    ]
    .join("\n");
    let out = f.ansi("edit_file", &result);
    for s in ["ctx9", "ctx10", "after1", "after2"] {
        assert!(out.contains(s), "{s}");
    }
    assert!(!out.contains("ctx7"));
    assert!(!out.contains("ctx8"));
}

#[test]
fn render_tool_result_edit_file_preview_mode_blank_separator_between_non_adjacent_blocks() {
    let mut f = Fixture::new();
    f.state.verbosity = 1;
    let mut lines: Vec<String> = [
        "     1   before1",
        "     2   before2",
        "     3 - oldA",
        "     3 + newA",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    lines.extend((4..16).map(|n| format!("    {n:>2}   mid{}", n - 3)));
    lines.extend(
        [
            "    16 - oldB",
            "    16 + newB",
            "    17   after1",
            "    18   after2",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    let out = f.ansi("edit_file", &lines.join("\n"));
    for s in ["oldA", "newA", "oldB", "newB"] {
        assert!(out.contains(s), "{s}");
    }
    assert!(out.contains("\n\n"));
}

#[test]
fn render_tool_result_edit_file_preview_mode_adjacent_blocks_share_context() {
    let mut f = Fixture::new();
    f.state.verbosity = 1;
    let result = [
        "     1   before",
        "     2 - oldA",
        "     2 + newA",
        "     3   shared1",
        "     4   shared2",
        "     5 - oldB",
        "     5 + newB",
        "     6   after",
    ]
    .join("\n");
    let out = f.ansi("edit_file", &result);
    for s in [
        "before", "oldA", "newA", "shared1", "shared2", "oldB", "newB", "after",
    ] {
        assert!(out.contains(s), "{s}");
    }
    assert!(!out.trim_end_matches('\n').contains("\n\n"));
}

// ── Pi tools (ToolCallRendererPiTests) ────────────────────────────────────────

struct PiFixture {
    dir: tempfile::TempDir,
    renderer: ToolCallRenderer,
    state: ReplState,
    buf: Arc<std::sync::Mutex<Vec<u8>>>,
    tools: Vec<Arc<dyn Tool>>,
}

impl PiFixture {
    fn new(width: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (console, buf) = Console::buffer();
        let mut renderer = ToolCallRenderer::new(console);
        renderer.cwd = dir.path().to_path_buf();
        renderer.width = Some(width);
        Self {
            dir,
            renderer,
            state: ReplState::default(),
            buf,
            tools: vec![
                Arc::new(orca_tools_pi::PiReadTool),
                Arc::new(orca_tools_pi::PiBashTool::default()),
                Arc::new(orca_tools_pi::PiEditTool),
                Arc::new(orca_tools_pi::PiWriteTool),
            ],
        }
    }

    fn tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    fn call(&mut self, name: &str, args: &str) {
        let tool = self.tool(name);
        self.renderer
            .render_tool_call(name, args, tool.as_deref(), &self.state);
    }

    fn result(&mut self, name: &str, result: &str, is_error: bool, args: &str) {
        let tool = self.tool(name);
        self.renderer.render_tool_result(
            name,
            result,
            is_error,
            None,
            Some(args),
            tool.as_deref(),
            &self.state,
        );
    }

    fn output(&self) -> String {
        Console::buffer_text(&self.buf)
    }
}

#[test]
fn tool_call_header_is_one_line_with_path() {
    let mut f = PiFixture::new(100);
    f.call("read", r#"{"path":"src/app.py","offset":10,"limit":5}"#);
    assert_eq!(f.output().trim_end(), "  ● read  src/app.py:10-14");
}

#[test]
fn bash_header_shows_command_not_filler_description() {
    let mut f = PiFixture::new(100);
    f.call("bash", r#"{"command":"pytest -q"}"#);
    assert!(f.output().contains("● bash  pytest -q"));
    assert!(!f.output().contains("Execute tool"));
}

#[test]
fn read_result_is_summarised_at_normal_verbosity() {
    let mut f = PiFixture::new(100);
    f.result("read", "line1\nline2\nline3", false, r#"{"path":"a.py"}"#);
    assert!(f.output().contains("Read 3 lines"));
    assert!(!f.output().contains("line2"));
}

#[test]
fn read_result_paged_notice_shows_range() {
    assert_eq!(
        summarize_read("a\nb\n\n[Showing lines 1-2000 of 5120. Use offset=2001 to continue.]"),
        "Read lines 1-2000 of 5120"
    );
}

#[test]
fn bash_failure_shows_output_then_exit_status_in_red_not_first_line_as_error() {
    let mut f = PiFixture::new(100);
    f.call("bash", r#"{"command":"pytest"}"#);
    let tool = f.tool("bash");
    let lines = f.renderer.result_lines(
        "bash",
        "\x1b[32mPASS\x1b[0m a\r\nFAIL b\r\n\n\nCommand exited with code 1",
        true,
        Some(r#"{"command":"pytest"}"#),
        tool.as_deref(),
        &f.state,
    );
    let ansi: String = lines
        .iter()
        .map(|l| markup::render(l, true) + "\n")
        .collect();
    let plain = crate::ui::console::strip_ansi(&ansi);
    assert!(plain.contains("⎿  PASS a"));
    assert!(plain.contains("✗ Command exited with code 1"));
    assert!(!plain.contains("✗ PASS"));
    // The tool's own colour codes never reach the terminal.
    assert!(!ansi.contains("\x1b[32mPASS"));
}

#[test]
fn edit_result_shows_reconstructed_diff_with_counts() {
    let mut f = PiFixture::new(100);
    std::fs::write(
        f.dir.path().join("calc.py"),
        "def add(a, b):\n    return a + b\n\ndef sub(a, b):\n    # subtract\n    return a - b\n",
    )
    .unwrap();
    let args = r#"{"path":"calc.py","edits":[{"oldText":"    return a - b","newText":"    # subtract\n    return a - b"}]}"#;
    f.call("edit", args);
    f.result(
        "edit",
        "Successfully replaced 1 block(s) in calc.py.",
        false,
        args,
    );
    let out = f.output();
    assert!(out.contains("Edited calc.py  +1 −0"), "{out}");
    assert!(Regex::new(r"5 \+     # subtract").unwrap().is_match(&out));
    assert!(Regex::new(r"4\s+def sub").unwrap().is_match(&out));
    assert!(!out.contains("Successfully replaced"));
}

#[test]
fn build_edit_diff_replaced_line_shows_minus_and_plus() {
    let diff = build_edit_diff("a\nb\nNEW\nd\n", &[("c".into(), "NEW".into())]).unwrap();
    assert_eq!(diff.added, 1);
    assert_eq!(diff.removed, 1);
    let minus = Regex::new(r"^\s*3 - c$").unwrap();
    let plus = Regex::new(r"^\s*3 \+ NEW$").unwrap();
    assert!(diff.lines.iter().any(|l| minus.is_match(l)));
    assert!(diff.lines.iter().any(|l| plus.is_match(l)));
}

#[test]
fn build_edit_diff_unlocatable_edit_returns_null() {
    assert!(build_edit_diff("x\n", &[("a".into(), "b".into())]).is_none());
}

#[test]
fn write_result_shows_line_count_and_preview() {
    let mut f = PiFixture::new(100);
    let args = r##"{"path":"NOTES.md","content":"# Notes\n\n- one\n"}"##;
    f.result("write", "Successfully wrote to NOTES.md", false, args);
    assert!(f.output().contains("Wrote 3 lines to NOTES.md"));
    assert!(f.output().contains("- one"));
}

#[test]
fn parallel_batch_results_are_labelled_with_their_call() {
    let mut f = PiFixture::new(100);
    f.renderer.begin_batch();
    f.call("bash", r#"{"command":"echo one"}"#);
    f.call("bash", r#"{"command":"echo two"}"#);
    f.result("bash", "one", false, r#"{"command":"echo one"}"#);
    f.result("bash", "two", false, r#"{"command":"echo two"}"#);
    assert!(f.output().contains("⎿  bash echo one"));
    assert!(f.output().contains("⎿  bash echo two"));
}

#[test]
fn long_preview_lines_are_cut_to_the_terminal_width() {
    let mut f = PiFixture::new(60);
    f.result("bash", &"x".repeat(200), false, r#"{"command":"x"}"#);
    for line in f.output().split('\n') {
        assert!(cell_width(line.trim_end()) <= 60, "line too long: {line}");
    }
}

#[test]
fn tool_results_only_short_results_offer_images() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("out")).unwrap();
    // A minimal PNG signature is enough: only the extension decides.
    std::fs::write(dir.path().join("out").join("gen.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    let cwd = dir.path().to_string_lossy().to_string();
    assert_eq!(
        image_paths_in("Image saved: out/gen.png\nPrompt: a cat", &cwd).len(),
        1
    );
    let listing = (0..10)
        .map(|i| format!("out/gen.png  ({i})"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(image_paths_in(&listing, &cwd).is_empty());
    assert!(image_paths_in("Wrote out/notes.txt", &cwd).is_empty());
}

// ── Extra checks of the port ─────────────────────────────────────────────────

#[test]
fn call_header_reason_and_multi_line_summary() {
    let mut f = Fixture::new();
    f.call(
        "run_command",
        r#"{"command":"echo a\necho b","_reason":"check things"}"#,
    );
    let out = f.output();
    assert!(out.contains("echo a … (+1 line)"), "{out}");
    assert!(out.contains("check things"));
}

#[test]
fn level_zero_hides_calls_and_non_print_results() {
    let mut f = Fixture::new();
    f.state.verbosity = 0;
    f.call("glob", r#"{"pattern":"*.rs"}"#);
    f.result("glob", "a.rs", false, None);
    assert!(f.output().is_empty());
}

#[test]
fn permission_and_plan_lines() {
    let f = Fixture::new();
    f.renderer.render_permission_prompt("bash", "Dangerous");
    f.renderer.render_plan_mode_toggle(true);
    f.renderer
        .render_plan_tool_blocked("write_file", "Moderate");
    let out = f.output();
    assert!(out.contains("  ⎿  ⚠ bash needs approval (risk: dangerous)"));
    assert!(out.contains("⏸ Plan mode enabled — the model will plan without making changes."));
    assert!(out.contains("  ⏸ write_file blocked in plan mode (risk: Moderate)"));
}
