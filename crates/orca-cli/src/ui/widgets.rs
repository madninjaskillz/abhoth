//! The Spectre.Console widgets the commands use (Table, Panel, Grid, Tree), drawn as text.
//! Cells hold markup; they are measured by their plain text and never wrap (a cell too wide
//! for the terminal is cut with `…`).

use super::console::{Console, cell_width, ellipsize};
use super::markup;

fn plain_width(m: &str) -> usize {
    cell_width(&markup::remove(m))
}

/// A cell's markup fitted to `width` cells (cut with `…` when longer, padded when shorter).
fn fit(m: &str, width: usize) -> String {
    let w = plain_width(m);
    if w <= width {
        return format!("{m}{}", " ".repeat(width - w));
    }
    let cut = ellipsize(&markup::remove(m), width);
    let pad = width.saturating_sub(cell_width(&cut));
    format!("{}{}", markup::escape(&cut), " ".repeat(pad))
}

/// `Table` with a rounded border.
pub struct Table {
    title: Option<String>,
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
    expand: bool,
}

impl Table {
    pub fn new() -> Self {
        Self {
            title: None,
            columns: Vec::new(),
            rows: Vec::new(),
            expand: false,
        }
    }

    pub fn title(mut self, title: &str) -> Self {
        self.title = Some(title.to_string());
        self
    }

    pub fn expand(mut self) -> Self {
        self.expand = true;
        self
    }

    pub fn column(mut self, header: &str) -> Self {
        self.columns.push(header.to_string());
        self
    }

    pub fn row<S: AsRef<str>>(&mut self, cells: &[S]) {
        self.rows
            .push(cells.iter().map(|c| c.as_ref().to_string()).collect());
    }

    /// The table as markup lines for a console `width` columns wide.
    pub fn lines(&self, width: usize) -> Vec<String> {
        let n = self.columns.len().max(1);
        let mut widths: Vec<usize> = (0..n)
            .map(|i| {
                let head = self.columns.get(i).map_or(0, |c| plain_width(c));
                self.rows
                    .iter()
                    .map(|r| r.get(i).map_or(0, |c| plain_width(c)))
                    .max()
                    .unwrap_or(0)
                    .max(head)
            })
            .collect();
        // Borders: "│ " + cells joined by " │ " + " │".
        let chrome = 4 + 3 * (n - 1);
        let avail = width.saturating_sub(1).max(chrome + n);
        let mut total: usize = widths.iter().sum::<usize>() + chrome;
        while total > avail {
            // Shrink the widest column.
            let (i, _) = widths.iter().enumerate().max_by_key(|(_, w)| **w).unwrap();
            if widths[i] <= 3 {
                break;
            }
            widths[i] -= 1;
            total -= 1;
        }
        if self.expand && total < avail {
            let last = n - 1;
            widths[last] += avail - total;
        }
        let inner: usize = widths.iter().sum::<usize>() + chrome - 2;
        let mut out = Vec::new();
        if let Some(t) = &self.title {
            let tw = plain_width(t);
            let pad = (inner + 2).saturating_sub(tw) / 2;
            out.push(format!("{}{t}", " ".repeat(pad)));
        }
        let rule = |l: &str, m: &str, r: &str| {
            let segs: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
            format!("{l}{}{r}", segs.join(m))
        };
        let row = |cells: &[String]| {
            let parts: Vec<String> = widths
                .iter()
                .enumerate()
                .map(|(i, w)| fit(cells.get(i).map_or("", |c| c.as_str()), *w))
                .collect();
            format!("│ {} │", parts.join(" │ "))
        };
        out.push(rule("╭", "┬", "╮"));
        out.push(row(&self.columns));
        out.push(rule("├", "┼", "┤"));
        for r in &self.rows {
            out.push(row(r));
        }
        out.push(rule("╰", "┴", "╯"));
        out
    }

    pub fn write(&self, console: &Console) {
        for l in self.lines(console.width()) {
            console.markup_line(&l);
        }
    }
}

impl Default for Table {
    fn default() -> Self {
        Self::new()
    }
}

/// `Panel` with a rounded border and a header; `content` is markup (lines split on `\n`).
pub fn panel_lines(content: &str, header: &str, border: &str, width: usize) -> Vec<String> {
    let lines: Vec<&str> = content.split('\n').collect();
    let content_w = lines.iter().map(|l| plain_width(l)).max().unwrap_or(0);
    let max_inner = width.saturating_sub(5).max(10);
    let inner = content_w.max(plain_width(header) + 2).min(max_inner);
    let b = |s: &str| format!("[{border}]{s}[/]");
    let head_w = plain_width(header);
    let top_fill = (inner + 2).saturating_sub(head_w + 3);
    let mut out = vec![format!(
        "{}{header}{}",
        b("╭─ "),
        b(&format!(" {}╮", "─".repeat(top_fill)))
    )];
    for l in lines {
        let l = l.trim_end_matches('\r');
        out.push(format!("{} {} {}", b("│"), fit(l, inner), b("│")));
    }
    out.push(b(&format!("╰{}╯", "─".repeat(inner + 2))));
    out
}

pub fn write_panel(console: &Console, content: &str, header: &str, border: &str) {
    for l in panel_lines(content, header, border, console.width()) {
        console.markup_line(&l);
    }
}

/// `Rule(title)`: a horizontal line across `width` with the title markup in it, at the left
/// (`LeftJustified`) or centred.
pub fn rule_line(title: &str, left: bool, width: usize) -> String {
    let width = width.max(10);
    let tw = plain_width(title);
    if tw + 6 > width {
        return format!("── {title}");
    }
    let rest = width - tw - 2;
    let (l, r) = if left {
        (2, rest - 2)
    } else {
        (rest / 2, rest - rest / 2)
    };
    format!("{} {title} {}", "─".repeat(l), "─".repeat(r))
}

pub fn write_rule(console: &Console, title: &str, left: bool) {
    console.markup_line(&rule_line(title, left, console.width()));
}

/// `Grid` of two columns: the first padded to its widest cell plus two spaces.
pub fn write_grid(console: &Console, rows: &[(String, String)]) {
    let w = rows.iter().map(|(a, _)| plain_width(a)).max().unwrap_or(0);
    for (a, b) in rows {
        let pad = w - plain_width(a);
        let line = format!("{a}{}  {b}", " ".repeat(pad));
        console.markup_line(line.trim_end());
    }
}

/// `Tree`: a root label and (label, depth) nodes, drawn with guide lines.
pub fn write_tree(console: &Console, root: &str, nodes: &[(String, usize)]) {
    console.markup_line(root);
    for (i, (label, depth)) in nodes.iter().enumerate() {
        // Whether another node at this depth follows before a shallower one.
        let last_at = |d: usize, from: usize| {
            !nodes[from + 1..]
                .iter()
                .take_while(|(_, nd)| *nd >= d)
                .any(|(_, nd)| *nd == d)
        };
        let mut prefix = String::new();
        for d in 0..*depth {
            // An ancestor column: a guide when that ancestor has later siblings.
            let ancestor = nodes[..i].iter().rposition(|(_, nd)| *nd == d).unwrap_or(0);
            prefix.push_str(if last_at(d, ancestor) {
                "    "
            } else {
                "│   "
            });
        }
        prefix.push_str(if last_at(*depth, i) {
            "└── "
        } else {
            "├── "
        });
        console.markup_line(&format!("{prefix}{label}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_fits_width_and_expands() {
        let mut t = Table::new().column("A").column("B").expand();
        t.row(&["1", "two"]);
        let lines = t.lines(20);
        assert!(lines.iter().all(|l| plain_width(l) <= 19));
        assert_eq!(plain_width(&lines[0]), 19);
        assert!(markup::remove(&lines[3]).contains("two"));
    }

    #[test]
    fn panel_draws_header_and_border() {
        let lines = panel_lines("a\nbb", "[bold]T[/]", "cyan", 40);
        let plain: Vec<String> = lines.iter().map(|l| markup::remove(l)).collect();
        assert!(plain[0].starts_with("╭─ T "));
        assert_eq!(plain[1], "│ a   │");
        assert_eq!(plain.len(), 4);
    }

    #[test]
    fn tree_draws_guides() {
        let (c, buf) = Console::buffer();
        write_tree(
            &c,
            "Sessions",
            &[("a".into(), 0), ("b".into(), 1), ("c".into(), 0)],
        );
        assert_eq!(
            Console::buffer_text(&buf),
            "Sessions\n├── a\n│   └── b\n└── c\n"
        );
    }
}
