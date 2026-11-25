//! `DiffRenderer`: unified diff text with coloured lines, in a bordered panel.

use crate::ui::console::Console;
use crate::ui::markup;
use crate::ui::widgets::write_panel;

/// `CliConstants.BashOutputMaxChars`: diffs longer than this are cut.
pub const BASH_OUTPUT_MAX_CHARS: usize = 30000;

/// `RenderUnifiedDiff`: the diff, coloured, inside a rounded panel with a bold header and a
/// border of `border_color` (a markup colour such as "green").
pub fn render_unified_diff(console: &Console, diff_text: &str, header: &str, border_color: &str) {
    if crate::render::text::is_blank(diff_text) {
        console.markup_line("[grey]No changes.[/]");
        return;
    }
    // Truncate oversized diffs (C# counts UTF-16 units; chars here).
    let text = if diff_text.chars().count() > BASH_OUTPUT_MAX_CHARS {
        let cut: String = diff_text.chars().take(BASH_OUTPUT_MAX_CHARS).collect();
        format!("{cut}\n... (truncated)")
    } else {
        diff_text.to_string()
    };
    write_panel(
        console,
        &colorize_diff_text(&text),
        &format!("[bold]{}[/]", markup::escape(header)),
        border_color,
    );
}

/// `ColorizeDiffText`: each diff line as markup.
pub fn colorize_diff_text(diff_text: &str) -> String {
    // Normalize CRLF: a trailing \r would render as an extra blank line.
    let normalized = diff_text.replace("\r\n", "\n").replace('\r', "\n");
    let mut sb = String::new();
    for (i, line) in normalized.split('\n').enumerate() {
        if i > 0 {
            sb.push('\n');
        }
        let escaped = markup::escape(line);
        let style = if line.starts_with("---") || line.starts_with("+++") {
            "bold"
        } else if line.starts_with("@@") {
            "cyan"
        } else if line.starts_with("diff ") {
            "bold yellow"
        } else if line.starts_with('+') {
            "green"
        } else if line.starts_with('-') {
            "red"
        } else {
            "dim"
        };
        sb.push_str(&format!("[{style}]{escaped}[/]"));
    }
    sb
}

#[cfg(test)]
mod tests {
    //! Ported from DiffRendererTests.cs.
    use super::*;

    #[test]
    fn colorize_diff_text_added_lines_contains_green() {
        let r = colorize_diff_text("+added line");
        assert!(r.contains("[green]") && r.contains("added line"));
    }

    #[test]
    fn colorize_diff_text_removed_lines_contains_red() {
        let r = colorize_diff_text("-removed line");
        assert!(r.contains("[red]") && r.contains("removed line"));
    }

    #[test]
    fn colorize_diff_text_hunk_headers_contains_cyan() {
        assert!(colorize_diff_text("@@ -1,3 +1,4 @@").contains("[cyan]"));
    }

    #[test]
    fn colorize_diff_text_diff_headers_contains_bold_yellow() {
        assert!(colorize_diff_text("diff --git a/file.cs b/file.cs").contains("[bold yellow]"));
    }

    #[test]
    fn colorize_diff_text_file_headers_contains_bold() {
        assert!(colorize_diff_text("--- a/file.cs\n+++ b/file.cs").contains("[bold]"));
    }

    #[test]
    fn colorize_diff_text_context_lines_contains_dim() {
        assert!(colorize_diff_text(" unchanged context line").contains("[dim]"));
    }

    #[test]
    fn colorize_diff_text_empty_diff_returns_empty() {
        assert_eq!(colorize_diff_text(""), "[dim][/]");
    }

    #[test]
    fn colorize_diff_text_multiple_line_types_all_colorized() {
        let diff = "diff --git a/f.cs b/f.cs\n--- a/f.cs\n+++ b/f.cs\n@@ -1,2 +1,3 @@\n context\n-old\n+new";
        let r = colorize_diff_text(diff);
        for tag in [
            "[bold yellow]",
            "[bold]",
            "[cyan]",
            "[green]",
            "[red]",
            "[dim]",
        ] {
            assert!(r.contains(tag), "{tag}");
        }
    }

    #[test]
    fn colorize_diff_text_escapes_markup_characters() {
        let r = colorize_diff_text("+var x = list[0];");
        assert!(r.contains("[green]"));
        assert!(r.contains("list[[0]]"));
    }

    #[test]
    fn render_unified_diff_empty_says_no_changes() {
        let (c, buf) = Console::buffer();
        render_unified_diff(&c, "  ", "x", "green");
        assert_eq!(Console::buffer_text(&buf), "No changes.\n");
    }
}
