//! Ported from MarkdownRendererTests.cs, plus checks of the hand-written parser and the
//! streaming renderer.

use super::*;
use crate::ui::markup;

fn render(md: &str) -> String {
    let (console, buf) = Console::buffer();
    MarkdownRenderer::new().render_to(&console, md);
    Console::buffer_text(&buf)
}

fn ansi(md: &str) -> String {
    MarkdownRenderer::new()
        .render_lines(md, 100)
        .iter()
        .map(|l| markup::render(l, true) + "\n")
        .collect()
}

#[test]
fn unmatched_bracket_keeps_the_rest_of_the_paragraph() {
    let out = render(r"Literal escape text: \x1b[31m should stay literal. END-MARKER");
    assert!(
        out.contains(r"\x1b[31m should stay literal. END-MARKER"),
        "{out}"
    );
}

#[test]
fn array_index_in_prose_is_not_dropped() {
    let out = render("Set items[0 to the first value and continue. TAIL");
    assert!(out.contains("items[0 to the first value and continue. TAIL"));
}

#[test]
fn bare_url_is_shown_once() {
    let out = render("See https://example.com/docs for details.");
    assert!(out.contains("https://example.com/docs"));
    assert!(!out.contains("(https://example.com/docs)"));
}

#[test]
fn named_link_shows_text_and_url() {
    let out = render("Read [the docs](https://example.com/docs).");
    assert!(out.contains("the docs (https://example.com/docs)"), "{out}");
}

#[test]
fn strikethrough_is_not_bold() {
    let out = ansi("This is ~~gone~~ now.");
    assert!(out.contains("\x1b[9m"), "{out:?}");
    assert!(!out.contains("\x1b[1m"));
}

#[test]
fn task_list_and_entities_render() {
    let out = render("- [x] done &amp; dusted\n- [ ] todo");
    assert!(out.contains("☑ done & dusted"), "{out}");
    assert!(out.contains("☐ todo"));
}

// ── The parser ──

#[test]
fn headings_paragraphs_and_lists() {
    let out =
        render("# Title\n\nSome *text* and **bold**.\n\n- one\n- two\n  - nested\n\n1. a\n2. b\n");
    assert_eq!(
        out,
        "# Title\n\nSome text and bold.\n\n • one\n • two\n   • nested\n\n 1. a\n 2. b\n\n"
    );
}

#[test]
fn heading_colours_and_emphasis_styles() {
    let lines = MarkdownRenderer::new().render_lines("## Sub\n\n*i* **b** ***bi*** `c`", 80);
    assert_eq!(lines[0], "[bold blue]## Sub[/]");
    assert_eq!(
        lines[2],
        "[italic]i[/] [bold]b[/] [bold italic]bi[/] [grey on color237]c[/]"
    );
}

#[test]
fn fenced_code_goes_in_a_panel_with_its_language() {
    let out = render("```rust\nfn main() {}\n```\n");
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("╭─ rust ─"), "{out}");
    assert_eq!(lines[1].trim_end(), "│ fn main() {} │");
    assert!(lines[2].starts_with('╰'));
    let plain = render("```\nx\n```");
    assert!(plain.starts_with("╭─ Code ─"));
}

#[test]
fn fenced_code_keeps_markdown_characters() {
    let out = render("```\n# not a heading\n- not a list [x]\n```");
    assert!(out.contains("│ # not a heading"));
    assert!(out.contains("- not a list [x]"));
}

#[test]
fn quote_table_and_rule() {
    let out = render("> quoted\n\n| a | b |\n|---|:-:|\n| 1 | 2 |\n\n---\n");
    assert!(out.contains("│ quoted"));
    assert!(out.contains("│ a"), "{out}");
    assert!(out.contains("│ 1"));
    assert!(out.contains("──────────"));
}

#[test]
fn html_blocks_are_skipped_and_inline_html_kept() {
    let out = render("<div>\nhidden\n</div>\n\nText with <b>tag</b>.");
    assert!(!out.contains("hidden"));
    assert!(out.contains("Text with <b>tag</b>."));
}

#[test]
fn intraword_underscores_and_lone_stars_stay_literal() {
    let out = render("snake_case_name and 2 * 3 * 4");
    assert!(out.contains("snake_case_name and 2 * 3 * 4"), "{out}");
}

#[test]
fn line_breaks_and_setext_headings() {
    let out = render("Title\n=====\n\nline one\nline two");
    assert_eq!(out, "# Title\n\nline one\nline two\n\n");
}

#[test]
fn autolinks_and_escapes() {
    let out = render("<https://x.dev> and \\*not em\\*");
    assert!(out.contains("https://x.dev and *not em*"), "{out}");
}

// ── Streaming ──

fn stream(tokens: &[&str]) -> (Vec<String>, String) {
    let (console, buf) = Console::buffer();
    let mut s = MarkdownStreamRenderer::new(console);
    let mut after_each = Vec::new();
    for t in tokens {
        s.append(t);
        after_each.push(Console::buffer_text(&buf));
    }
    s.finish();
    (after_each, Console::buffer_text(&buf))
}

#[test]
fn stream_renders_completed_blocks_early_and_the_rest_at_finish() {
    let (steps, done) = stream(&["# Hi\n\nfirst para", "graph\n\nsecond", "\n", " end"]);
    assert_eq!(steps[0], "");
    // The heading is complete once the paragraph after it has begun on a full line.
    assert_eq!(steps[2], "# Hi\n\nfirst paragraph\n\n");
    assert_eq!(done, "# Hi\n\nfirst paragraph\n\nsecond\nend\n\n");
}

#[test]
fn stream_never_splits_code_fences_or_lists() {
    let (steps, done) = stream(&["```\na\n\nb\n```\n\n", "1. x\n\n2. y\n\n", "after\n"]);
    assert!(
        steps
            .iter()
            .all(|s| !s.contains("│ a") || s.contains("│ b"))
    );
    assert!(done.contains(" 1. x") && done.contains(" 2. y"), "{done}");
    assert_eq!(done, render("```\na\n\nb\n```\n\n1. x\n\n2. y\n\nafter"));
}

#[test]
fn stream_output_matches_whole_render() {
    let md = "Intro text.\n\n## Steps\n\n- a\n- b\n\n```py\nprint(1)\n```\n\nDone.";
    let tokens: Vec<String> = md.chars().map(|c| c.to_string()).collect();
    let refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
    let (_, done) = stream(&refs);
    assert_eq!(done, render(md));
}

#[test]
fn stream_finish_without_start_prints_nothing_and_buffer_keeps_text() {
    let (console, buf) = Console::buffer();
    let mut s = MarkdownStreamRenderer::new(console);
    s.append_token("text");
    s.finish();
    assert_eq!(Console::buffer_text(&buf), "");
    assert_eq!(s.buffer(), "text");
}

// ── Live tail (terminal consoles) ──

fn live() -> (
    MarkdownStreamRenderer,
    std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
) {
    let (console, buf) = Console::buffer();
    let mut s = MarkdownStreamRenderer::new(console);
    s.set_live(true);
    s.height = Some(40);
    (s, buf)
}

#[test]
fn live_tail_shows_raw_text_then_the_rendered_block() {
    // The test console drops the erase sequences, so it shows raw text and renders in turn.
    let (mut s, buf) = live();
    s.append("**bold** te");
    assert_eq!(Console::buffer_text(&buf), "**bold** te");
    s.append("xt\n\nnext\n");
    assert_eq!(
        Console::buffer_text(&buf),
        "**bold** text\n\nnext\nbold text\n\nnext\n"
    );
    s.finish();
    assert!(Console::buffer_text(&buf).ends_with("bold text\n\nnext\nnext\n\n"));
}

#[test]
fn live_tail_rows_count_wrapping() {
    let (mut s, _) = live();
    s.tail = format!("{}\nb", "a".repeat(150));
    assert_eq!(s.tail_rows(), 3); // 150 cells at width 100 take two rows
    s.tail = "x\n".into();
    assert_eq!(s.tail_rows(), 2); // the cursor sits on the row after the line break
}

#[test]
fn live_tail_taller_than_the_window_stays_raw_and_is_not_rendered_again() {
    let (mut s, buf) = live();
    s.height = Some(8); // two rows may be erased
    s.append("l1\nl2\nl3\n\nnext\n");
    s.finish();
    let out = Console::buffer_text(&buf);
    assert_eq!(out.matches("l1").count(), 1, "{out}");
    assert_eq!(out, "l1\nl2\nl3\n\nnext\nnext\n\n");
}

#[test]
fn clear_keeps_the_partial_text_and_ends_its_line() {
    let (mut s, buf) = live();
    s.append("streams slowly");
    s.clear();
    s.finish();
    assert_eq!(Console::buffer_text(&buf), "streams slowly\n");
    assert!(!s.active());
}
