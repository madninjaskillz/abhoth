//! `MarkdownRenderer` and `MarkdownStreamRenderer`: model replies as console markup.
//!
//! C# parses with Markdig (advanced extensions) and draws with Spectre. This is a hand-written
//! parser for the subset the renderer draws: ATX and setext headings, paragraphs, fenced and
//! indented code (in a panel), bullet / ordered / task lists (nested), block quotes, pipe
//! tables, thematic breaks and HTML blocks (skipped); inline code, emphasis (`*` `_`, and
//! `~~strike~~`, plus Markdig's `~sub~` `^sup^` `==mark==` `++ins++` drawn as plain text),
//! links and images, autolinks and bare URLs, entities, backslash escapes, inline HTML and
//! line breaks. Reference links, footnotes, definition lists and the like render as text.

use std::sync::LazyLock;

use regex::Regex;

use super::links;
use super::text::{is_blank, sanitize};
use crate::ui::console::{Console, cell_width};
use crate::ui::markup::escape;
use crate::ui::widgets::{Table, panel_lines};

// ── Blocks ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Block {
    Heading(usize, String),
    Paragraph(String),
    /// `fenced`: a fenced block (its panel gets a header); `info`: the language.
    Code {
        fenced: bool,
        info: String,
        text: String,
    },
    List {
        ordered: bool,
        items: Vec<Vec<Block>>,
    },
    Rule,
    Quote(Vec<Block>),
    /// The first row is the header.
    Table(Vec<Vec<String>>),
    Html,
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// A code fence opening: (fence char, run length, info string).
fn fence_open(line: &str) -> Option<(char, usize, String)> {
    if indent_of(line) > 3 {
        return None;
    }
    let t = line.trim_start();
    let ch = t.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let len = t.chars().take_while(|c| *c == ch).count();
    if len < 3 {
        return None;
    }
    let info = t[len..].trim();
    if ch == '`' && info.contains('`') {
        return None;
    }
    Some((
        ch,
        len,
        info.split_whitespace().next().unwrap_or("").to_string(),
    ))
}

fn is_fence_close(line: &str, ch: char, len: usize) -> bool {
    if indent_of(line) > 3 {
        return false;
    }
    let t = line.trim();
    let run = t.chars().take_while(|c| *c == ch).count();
    run >= len && run == t.chars().count()
}

/// An ATX heading: (level, text).
fn atx_heading(line: &str) -> Option<(usize, String)> {
    if indent_of(line) > 3 {
        return None;
    }
    let t = line.trim_start();
    let level = t.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &t[level..];
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    let mut text = rest.trim();
    // A closing run of #s (preceded by a space) is not part of the text.
    let stripped = text.trim_end_matches('#');
    if stripped.len() < text.len() && (stripped.is_empty() || stripped.ends_with(' ')) {
        text = stripped.trim_end();
    }
    Some((level, text.to_string()))
}

fn is_rule(line: &str) -> bool {
    if indent_of(line) > 3 {
        return false;
    }
    let t: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    let Some(first) = t.chars().next() else {
        return false;
    };
    matches!(first, '-' | '*' | '_') && t.chars().count() >= 3 && t.chars().all(|c| c == first)
}

#[derive(Debug, Clone, Copy)]
struct ListMarker {
    ordered: bool,
    /// The bullet char, or the ordered delimiter (`.` / `)`).
    kind: char,
    /// The column the item's content starts at.
    content: usize,
    /// The ordered list's first number (only 1 may interrupt a paragraph).
    number: u64,
}

fn list_marker(line: &str) -> Option<ListMarker> {
    let ind = indent_of(line);
    if ind > 3 {
        return None;
    }
    let t = &line[ind..];
    let (ordered, kind, marker_len, number) = match t.chars().next()? {
        c @ ('-' | '*' | '+') => (false, c, 1, 0),
        c if c.is_ascii_digit() => {
            let digits = t.chars().take_while(char::is_ascii_digit).count();
            if digits > 9 {
                return None;
            }
            let delim = t[digits..].chars().next()?;
            if delim != '.' && delim != ')' {
                return None;
            }
            (true, delim, digits + 1, t[..digits].parse().ok()?)
        }
        _ => return None,
    };
    let after = &t[marker_len..];
    if after.is_empty() {
        return Some(ListMarker {
            ordered,
            kind,
            content: ind + marker_len + 1,
            number,
        });
    }
    let spaces = after.len() - after.trim_start_matches(' ').len();
    if spaces == 0 {
        return None;
    }
    let spaces = if spaces > 4 || after.trim().is_empty() {
        1
    } else {
        spaces
    };
    Some(ListMarker {
        ordered,
        kind,
        content: ind + marker_len + spaces,
        number,
    })
}

const HTML_BLOCK_TAGS: [&str; 46] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "body",
    "center",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "dd",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "html",
    "iframe",
    "li",
    "main",
    "menu",
    "nav",
    "ol",
    "p",
    "pre",
    "script",
    "section",
    "style",
    "summary",
    "table",
    "tbody",
    "td",
    "th",
    "tr",
    "ul",
    "br",
];

fn is_html_block_start(line: &str) -> bool {
    if indent_of(line) > 3 {
        return false;
    }
    let t = line.trim_start();
    if t.starts_with("<!--") || t.starts_with("<?") || t.starts_with("<!") {
        return true;
    }
    let Some(rest) = t.strip_prefix('<') else {
        return false;
    };
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let name: String = rest
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase();
    if !HTML_BLOCK_TAGS.contains(&name.as_str()) {
        return false;
    }
    let after = &rest[name.len()..];
    after.is_empty() || after.starts_with([' ', '>', '/', '\t'])
}

static TABLE_SEPARATOR_CELL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^:?-+:?$").expect("static regex"));

/// A pipe table's cells (outer pipes dropped, `\|` kept as a pipe, pipes in code spans kept).
fn split_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = if t.ends_with('|') && !t.ends_with("\\|") {
        &t[..t.len() - 1]
    } else {
        t
    };
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut in_code = false;
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                chars.next();
                cell.push('|');
            }
            '`' => {
                in_code = !in_code;
                cell.push(c);
            }
            '|' if !in_code => cells.push(std::mem::take(&mut cell).trim().to_string()),
            _ => cell.push(c),
        }
    }
    cells.push(cell.trim().to_string());
    cells
}

fn is_table_separator(line: &str) -> bool {
    if !line.contains('|') && !line.contains('-') {
        return false;
    }
    let cells = split_row(line);
    !cells.is_empty() && cells.iter().all(|c| TABLE_SEPARATOR_CELL.is_match(c))
}

/// Whether `line` starts a block that interrupts a paragraph.
fn interrupts_paragraph(line: &str) -> bool {
    fence_open(line).is_some()
        || atx_heading(line).is_some()
        || is_rule(line)
        || (indent_of(line) <= 3 && line.trim_start().starts_with('>'))
        || is_html_block_start(line)
        // A list item interrupts a paragraph when it has content (an ordered one only
        // when it starts at 1), as in CommonMark.
        || list_marker(line).is_some_and(|m| {
            (!m.ordered || m.number == 1) && !is_blank(line.get(m.content..).unwrap_or(""))
        })
}

fn parse_blocks(lines: &[String]) -> Vec<Block> {
    let mut out = Vec::new();
    let n = lines.len();
    let mut i = 0;
    while i < n {
        let line = &lines[i];
        if is_blank(line) {
            i += 1;
            continue;
        }
        // Indented code.
        if indent_of(line) >= 4 {
            let mut j = i;
            let mut code = Vec::new();
            while j < n && (is_blank(&lines[j]) || indent_of(&lines[j]) >= 4) {
                code.push(lines[j].get(4..).unwrap_or("").to_string());
                j += 1;
            }
            while code.last().is_some_and(|l| is_blank(l)) {
                code.pop();
            }
            out.push(Block::Code {
                fenced: false,
                info: String::new(),
                text: code.join("\n"),
            });
            i = j;
            continue;
        }
        if let Some((ch, len, info)) = fence_open(line) {
            let ind = indent_of(line);
            let mut j = i + 1;
            let mut code = Vec::new();
            while j < n && !is_fence_close(&lines[j], ch, len) {
                let l = &lines[j];
                code.push(l[indent_of(l).min(ind)..].to_string());
                j += 1;
            }
            while code.last().is_some_and(|l| is_blank(l)) {
                code.pop();
            }
            out.push(Block::Code {
                fenced: true,
                info,
                text: code.join("\n"),
            });
            i = j + 1;
            continue;
        }
        if let Some((level, text)) = atx_heading(line) {
            out.push(Block::Heading(level, text));
            i += 1;
            continue;
        }
        if is_rule(line) {
            out.push(Block::Rule);
            i += 1;
            continue;
        }
        if line.trim_start().starts_with('>') {
            let mut j = i;
            let mut inner = Vec::new();
            while j < n && !is_blank(&lines[j]) {
                let t = lines[j].trim_start();
                if let Some(rest) = t.strip_prefix('>') {
                    inner.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
                } else if inner.last().is_some_and(|l: &String| !is_blank(l))
                    && !interrupts_paragraph(&lines[j])
                {
                    inner.push(t.to_string()); // lazy continuation
                } else {
                    break;
                }
                j += 1;
            }
            out.push(Block::Quote(parse_blocks(&inner)));
            i = j;
            continue;
        }
        if let Some(marker) = list_marker(line) {
            let (block, next) = parse_list(lines, i, marker);
            out.push(block);
            i = next;
            continue;
        }
        if is_html_block_start(line) {
            while i < n && !is_blank(&lines[i]) {
                i += 1;
            }
            out.push(Block::Html);
            continue;
        }
        if i + 1 < n && line.contains('|') && is_table_separator(&lines[i + 1]) {
            let mut rows = vec![split_row(line)];
            let mut j = i + 2;
            while j < n && !is_blank(&lines[j]) && lines[j].contains('|') {
                rows.push(split_row(&lines[j]));
                j += 1;
            }
            out.push(Block::Table(rows));
            i = j;
            continue;
        }
        // Paragraph (or a setext heading).
        let mut para = vec![line.trim_start().to_string()];
        let mut j = i + 1;
        let mut setext = None;
        while j < n && !is_blank(&lines[j]) {
            let l = &lines[j];
            let t = l.trim();
            if indent_of(l) <= 3 && !t.is_empty() && t.chars().all(|c| c == '=') {
                setext = Some(1);
                j += 1;
                break;
            }
            if indent_of(l) <= 3 && !t.is_empty() && t.chars().all(|c| c == '-') {
                setext = Some(2);
                j += 1;
                break;
            }
            if interrupts_paragraph(l) {
                break;
            }
            para.push(l.trim_start().to_string());
            j += 1;
        }
        let text = para.join("\n");
        out.push(match setext {
            Some(level) => Block::Heading(level, text.trim().to_string()),
            None => Block::Paragraph(text.trim_end().to_string()),
        });
        i = j;
    }
    out
}

fn same_list(a: ListMarker, b: ListMarker) -> bool {
    a.ordered == b.ordered && a.kind == b.kind
}

fn parse_list(lines: &[String], start: usize, first: ListMarker) -> (Block, usize) {
    let n = lines.len();
    let mut items = Vec::new();
    let mut i = start;
    let mut marker = first;
    loop {
        let line = &lines[i];
        let content = marker.content;
        let mut item = vec![line.get(content..).unwrap_or("").to_string()];
        let mut j = i + 1;
        while j < n {
            let l = &lines[j];
            if is_blank(l) {
                let k = (j..n).find(|&k| !is_blank(&lines[k]));
                match k {
                    Some(k) if indent_of(&lines[k]) >= content => {
                        item.extend((j..k).map(|_| String::new()));
                        j = k;
                        continue;
                    }
                    _ => break,
                }
            }
            if indent_of(l) >= content {
                item.push(l[content..].to_string());
            } else if list_marker(l).is_some() {
                break;
            } else if item.last().is_some_and(|p| !is_blank(p)) && !interrupts_paragraph(l) {
                item.push(l.trim_start().to_string()); // lazy continuation
            } else {
                break;
            }
            j += 1;
        }
        items.push(parse_blocks(&item));
        // The next item: right here, or after blank lines.
        let k = (j..n).find(|&k| !is_blank(&lines[k]));
        match k.and_then(|k| list_marker(&lines[k]).map(|m| (k, m))) {
            Some((k, m)) if same_list(first, m) && indent_of(&lines[k]) < content => {
                i = k;
                marker = m;
            }
            _ => {
                return (
                    Block::List {
                        ordered: first.ordered,
                        items,
                    },
                    j,
                );
            }
        }
    }
}

// ── Inlines ──────────────────────────────────────────────────────────────────

static INLINE_HTML: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^(?:</?[A-Za-z][A-Za-z0-9-]*(?:\s+[A-Za-z_:][^<>]*?)?\s*/?>|<!--[\s\S]*?-->)"#)
        .expect("static regex")
});
static AUTOLINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^<([A-Za-z][A-Za-z0-9+.\-]{1,31}:[^\s<>]*|[A-Za-z0-9.!#$%&'*+/=?^_`{|}~\-]+@[A-Za-z0-9](?:[A-Za-z0-9\-]*[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9\-]*[A-Za-z0-9])?)*)>")
        .expect("static regex")
});
static ENTITY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^&(?:#[0-9]{1,7}|#[xX][0-9a-fA-F]{1,6}|[A-Za-z][A-Za-z0-9]{1,31});")
        .expect("static regex")
});

fn decode_entity(entity: &str) -> Option<String> {
    let body = &entity[1..entity.len() - 1];
    if let Some(num) = body.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        let c = char::from_u32(code)
            .filter(|c| *c != '\0')
            .unwrap_or('\u{FFFD}');
        return Some(c.to_string());
    }
    let s = match body {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{a0}",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "hellip" => "…",
        "mdash" => "—",
        "ndash" => "–",
        "laquo" => "«",
        "raquo" => "»",
        "times" => "×",
        "rarr" => "→",
        "larr" => "←",
        "deg" => "°",
        "euro" => "€",
        "middot" => "·",
        "bull" => "•",
        _ => return None,
    };
    Some(s.to_string())
}

/// `RenderLink`: autolinks and bare URLs show the URL once; named links "text (url)".
fn render_link(text: &str, url: &str) -> String {
    if text.is_empty() || text == url || format!("mailto:{text}") == url {
        let shown = if url.is_empty() { text } else { url };
        return format!("[blue underline]{}[/]", escape(shown));
    }
    format!(
        "[blue underline]{}[/] [grey]({})[/]",
        escape(text),
        escape(url)
    )
}

fn is_ascii_punct(c: char) -> bool {
    c.is_ascii_punctuation()
}

/// The length of the run of `c` starting at `i`.
fn run_len(chars: &[char], i: usize, c: char) -> usize {
    chars[i..].iter().take_while(|&&x| x == c).count()
}

/// A link's `](destination "title")` tail starting at `i` (just after `]`): (url, end).
fn link_tail(chars: &[char], i: usize) -> Option<(String, usize)> {
    if chars.get(i) != Some(&'(') {
        return None;
    }
    let mut k = i + 1;
    while chars.get(k).is_some_and(|c| *c == ' ') {
        k += 1;
    }
    let mut url = String::new();
    if chars.get(k) == Some(&'<') {
        k += 1;
        while let Some(&c) = chars.get(k) {
            if c == '>' {
                break;
            }
            if c == '\n' || c == '<' {
                return None;
            }
            url.push(c);
            k += 1;
        }
        chars.get(k)?;
        k += 1;
    } else {
        let mut depth = 0;
        while let Some(&c) = chars.get(k) {
            if c.is_whitespace() || (c == ')' && depth == 0) {
                break;
            }
            if c == '\\' && chars.get(k + 1).is_some_and(|n| is_ascii_punct(*n)) {
                url.push(chars[k + 1]);
                k += 2;
                continue;
            }
            if c == '(' {
                depth += 1;
            } else if c == ')' {
                depth -= 1;
            }
            url.push(c);
            k += 1;
        }
    }
    while chars.get(k).is_some_and(|c| c.is_whitespace()) {
        k += 1;
    }
    // An optional title.
    if let Some(&open) = chars.get(k).filter(|c| matches!(c, '"' | '\'' | '(')) {
        let close = if open == '(' { ')' } else { open };
        k += 1;
        while chars.get(k).is_some_and(|c| *c != close) {
            k += 1;
        }
        chars.get(k)?;
        k += 1;
        while chars.get(k).is_some_and(|c| c.is_whitespace()) {
            k += 1;
        }
    }
    (chars.get(k) == Some(&')')).then_some((url, k + 1))
}

/// The `]` matching the `[` at `i` (nested brackets counted; escapes and code skipped).
fn matching_bracket(chars: &[char], i: usize) -> Option<usize> {
    let mut depth = 0;
    let mut k = i;
    while k < chars.len() {
        match chars[k] {
            '\\' => k += 1,
            '`' => {
                let n = run_len(chars, k, '`');
                if let Some(end) = find_code_close(chars, k + n, n) {
                    k = end + n - 1;
                } else {
                    k += n - 1;
                }
            }
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
        k += 1;
    }
    None
}

/// The start of a backtick run of exactly `n` from `from`.
fn find_code_close(chars: &[char], from: usize, n: usize) -> Option<usize> {
    let mut k = from;
    while k < chars.len() {
        if chars[k] == '`' {
            let len = run_len(chars, k, '`');
            if len == n {
                return Some(k);
            }
            k += len;
        } else {
            k += 1;
        }
    }
    None
}

/// The closing delimiter run for an opening run of `n` × `c` that ended at `from`.
fn find_emphasis_close(chars: &[char], from: usize, c: char, n: usize) -> Option<usize> {
    let mut k = from;
    while k < chars.len() {
        match chars[k] {
            '\\' => k += 2,
            '`' => {
                let len = run_len(chars, k, '`');
                k = find_code_close(chars, k + len, len).map_or(k + len, |e| e + len);
            }
            x if x == c => {
                let len = run_len(chars, k, c);
                let prev_ws = k == 0 || chars[k - 1].is_whitespace();
                let next = chars.get(k + len);
                let intraword_ok = c != '_' || !next.is_some_and(|n| n.is_alphanumeric());
                if len == n && !prev_ws && k > from && intraword_ok {
                    return Some(k);
                }
                k += len;
            }
            _ => k += 1,
        }
    }
    None
}

const BARE_URL_PREFIXES: [&str; 5] = ["https://", "http://", "ftp://", "mailto:", "www."];

fn starts_with_at(chars: &[char], i: usize, prefix: &str) -> bool {
    let p: Vec<char> = prefix.chars().collect();
    chars.len() >= i + p.len()
        && chars[i..i + p.len()]
            .iter()
            .zip(&p)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// A bare URL at `i`: (shown text, url, end).
fn bare_url(chars: &[char], i: usize) -> Option<(String, String, usize)> {
    if i > 0 && (chars[i - 1].is_alphanumeric() || matches!(chars[i - 1], '/' | '@' | '.')) {
        return None;
    }
    let prefix = BARE_URL_PREFIXES
        .iter()
        .find(|p| starts_with_at(chars, i, p))?;
    let mut end = i;
    while end < chars.len() && !chars[end].is_whitespace() && chars[end] != '<' {
        end += 1;
    }
    // Trailing punctuation is not part of the URL; nor is an unbalanced ')'.
    while let Some(&last) = chars[i..end].last() {
        if matches!(
            last,
            '.' | ',' | ':' | ';' | '!' | '?' | '"' | '\'' | '*' | '_' | '~'
        ) {
            end -= 1;
        } else if last == ')' {
            let open = chars[i..end].iter().filter(|c| **c == '(').count();
            let close = chars[i..end].iter().filter(|c| **c == ')').count();
            if close > open {
                end -= 1;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    let plen = prefix.chars().count();
    if end <= i + plen || !chars[i + plen..end].iter().any(|c| c.is_alphanumeric()) {
        return None;
    }
    let text: String = chars[i..end].iter().collect();
    let url = if prefix == &"www." {
        format!("http://{text}")
    } else {
        text.clone()
    };
    Some((text, url, end))
}

/// The inlines of `src` as markup (`plain`: as plain text, `GetInlineText`).
fn render_inlines(src: &str, plain: bool) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::new();
    let mut text = String::new();
    let flush = |text: &mut String, out: &mut String| {
        if text.is_empty() {
            return;
        }
        if plain {
            out.push_str(text);
        } else {
            // File paths in prose become links; "Agent A" gets the agent's badge.
            out.push_str(&super::agent_badge::decorate_markup(
                text,
                None,
                Some(&links::text_markup),
            ));
        }
        text.clear();
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => match chars.get(i + 1) {
                Some(&n) if is_ascii_punct(n) => {
                    text.push(n);
                    i += 2;
                }
                Some('\n') => {
                    flush(&mut text, &mut out);
                    out.push_str(if plain { " " } else { "\n" });
                    i += 2;
                }
                _ => {
                    text.push('\\');
                    i += 1;
                }
            },
            '\n' => {
                let trimmed = text.trim_end_matches(' ').len();
                text.truncate(trimmed);
                flush(&mut text, &mut out);
                out.push_str(if plain { " " } else { "\n" });
                i += 1;
                while chars.get(i) == Some(&' ') {
                    i += 1;
                }
            }
            '`' => {
                let n = run_len(&chars, i, '`');
                match find_code_close(&chars, i + n, n) {
                    Some(end) => {
                        let raw: String = chars[i + n..end].iter().collect();
                        let mut content = raw.replace('\n', " ");
                        if content.len() >= 2
                            && content.starts_with(' ')
                            && content.ends_with(' ')
                            && !content.trim().is_empty()
                        {
                            content = content[1..content.len() - 1].to_string();
                        }
                        flush(&mut text, &mut out);
                        if plain {
                            out.push_str(&content);
                        } else {
                            // Spectre's grey23 is xterm colour 237.
                            out.push_str(&format!(
                                "[grey on color237]{}[/]",
                                links::path_markup(&content, None)
                            ));
                        }
                        i = end + n;
                    }
                    None => {
                        text.extend(std::iter::repeat_n('`', n));
                        i += n;
                    }
                }
            }
            '&' => {
                let rest: String = chars[i..chars.len().min(i + 40)].iter().collect();
                match ENTITY.find(&rest).and_then(|m| {
                    decode_entity(m.as_str()).map(|d| (d, m.as_str().chars().count()))
                }) {
                    Some((decoded, len)) => {
                        text.push_str(&decoded);
                        i += len;
                    }
                    None => {
                        text.push('&');
                        i += 1;
                    }
                }
            }
            '<' => {
                let rest: String = chars[i..].iter().collect();
                if let Some(m) = AUTOLINK.captures(&rest) {
                    let url = m[1].to_string();
                    flush(&mut text, &mut out);
                    out.push_str(&if plain {
                        url.clone()
                    } else {
                        render_link(&url, &url)
                    });
                    i += m[0].chars().count();
                } else if let Some(m) = INLINE_HTML.find(&rest) {
                    flush(&mut text, &mut out);
                    if !plain {
                        out.push_str(&escape(m.as_str()));
                    }
                    i += m.as_str().chars().count();
                } else {
                    text.push('<');
                    i += 1;
                }
            }
            '!' if chars.get(i + 1) == Some(&'[') => match inline_link(&chars, i + 1) {
                Some((label, url, end)) => {
                    flush(&mut text, &mut out);
                    out.push_str(&if plain {
                        label
                    } else {
                        render_link(&label, &url)
                    });
                    i = end;
                }
                None => {
                    text.push('!');
                    i += 1;
                }
            },
            '[' => match inline_link(&chars, i) {
                Some((label, url, end)) => {
                    flush(&mut text, &mut out);
                    out.push_str(&if plain {
                        label
                    } else {
                        render_link(&label, &url)
                    });
                    i = end;
                }
                None => {
                    // An unmatched "[" stays literal and the rest of the text still renders.
                    text.push('[');
                    i += 1;
                }
            },
            '*' | '_' | '~' | '^' | '=' | '+' => {
                let n = run_len(&chars, i, c);
                let allowed = match c {
                    '*' | '_' => (1..=3).contains(&n),
                    '~' => n <= 2,
                    '^' => n == 1,
                    _ => n == 2,
                };
                let next = chars.get(i + n);
                let left_flanking = next.is_some_and(|x| !x.is_whitespace());
                let intraword_ok = c != '_' || i == 0 || !chars[i - 1].is_alphanumeric();
                let close = (allowed && left_flanking && intraword_ok)
                    .then(|| find_emphasis_close(&chars, i + n, c, n))
                    .flatten();
                match close {
                    Some(end) => {
                        let inner: String = chars[i + n..end].iter().collect();
                        let body = render_inlines(&inner, plain);
                        flush(&mut text, &mut out);
                        let style = match (c, n) {
                            _ if plain => None,
                            ('~', 2) => Some("strikethrough"),
                            ('*' | '_', 1) => Some("italic"),
                            ('*' | '_', 2) => Some("bold"),
                            ('*' | '_', _) => Some("bold italic"),
                            _ => None,
                        };
                        match style {
                            Some(s) => out.push_str(&format!("[{s}]{body}[/]")),
                            None => out.push_str(&body),
                        }
                        i = end + n;
                    }
                    None => {
                        text.extend(std::iter::repeat_n(c, n));
                        i += n;
                    }
                }
            }
            _ => {
                if (c == 'h'
                    || c == 'H'
                    || c == 'f'
                    || c == 'F'
                    || c == 'm'
                    || c == 'M'
                    || c == 'w'
                    || c == 'W')
                    && let Some((shown, url, end)) = bare_url(&chars, i)
                {
                    flush(&mut text, &mut out);
                    out.push_str(&if plain {
                        shown
                    } else {
                        render_link(&shown, &url)
                    });
                    i = end;
                    continue;
                }
                text.push(c);
                i += 1;
            }
        }
    }
    flush(&mut text, &mut out);
    out
}

/// `[label](url)` at `i`: (plain label, url, end).
fn inline_link(chars: &[char], i: usize) -> Option<(String, String, usize)> {
    let close = matching_bracket(chars, i)?;
    let (url, end) = link_tail(chars, close + 1)?;
    let inner: String = chars[i + 1..close].iter().collect();
    Some((render_inlines(&inner, true), url, end))
}

/// A task list item's checkbox: (glyph, rest of the text).
fn task_marker(text: &str) -> Option<(&'static str, &str)> {
    for (prefix, glyph) in [("[x]", "☑"), ("[X]", "☑"), ("[ ]", "☐")] {
        if let Some(rest) = text.strip_prefix(prefix)
            && (rest.is_empty() || rest.starts_with([' ', '\n']))
        {
            return Some((glyph, rest));
        }
    }
    None
}

// ── Rendering ────────────────────────────────────────────────────────────────

/// `MarkdownRenderer`.
#[derive(Debug, Default, Clone, Copy)]
pub struct MarkdownRenderer;

impl MarkdownRenderer {
    pub fn new() -> Self {
        Self
    }

    /// `RenderToConsole` / `RenderTo`: the markdown, block by block. (The inline image
    /// thumbnails C# adds for the final message are not ported.)
    pub fn render_to(&self, console: &Console, markdown: &str) {
        for line in self.render_lines(markdown, console.width()) {
            console.markup_line(&line);
        }
    }

    /// The markup lines `render_to` writes for a console `width` columns wide (a line may
    /// hold `\n`: a paragraph's line breaks).
    pub fn render_lines(&self, markdown: &str, width: usize) -> Vec<String> {
        let text = markdown
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', "    ");
        let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        let mut out = Vec::new();
        for block in parse_blocks(&lines) {
            render_block(&block, width, &mut out);
        }
        out
    }
}

fn render_block(block: &Block, width: usize, out: &mut Vec<String>) {
    match block {
        Block::Heading(level, text) => {
            let color = match level {
                1 => "bold cyan",
                2 => "bold blue",
                3 => "bold yellow",
                _ => "bold",
            };
            out.push(format!(
                "[{color}]{} {}[/]",
                "#".repeat(*level),
                escape(&render_inlines(text, true))
            ));
            out.push(String::new());
        }
        Block::Paragraph(text) => {
            out.push(render_inlines(text, false));
            out.push(String::new());
        }
        Block::Code { fenced, info, text } => {
            let escaped = escape(text);
            if *fenced {
                let header = if info.is_empty() {
                    "Code"
                } else {
                    info.as_str()
                };
                out.extend(panel_lines(&escaped, &escape(header), "grey", width));
            } else {
                let mut lines = panel_lines(&escaped, "", "grey", width);
                // No header: a plain top border.
                if let Some(bottom) = lines.last().cloned() {
                    lines[0] = bottom.replace('╰', "╭").replace('╯', "╮");
                }
                out.extend(lines);
            }
            out.push(String::new());
        }
        Block::List { ordered, items } => {
            render_list(*ordered, items, 0, out);
            out.push(String::new());
        }
        Block::Rule => {
            out.push(format!(
                "[grey]{}[/]",
                "─".repeat(width.saturating_sub(1).max(1))
            ));
            out.push(String::new());
        }
        Block::Quote(children) => {
            for child in children {
                if let Block::Paragraph(text) = child {
                    out.push(format!("[grey]│[/] {}", render_inlines(text, false)));
                }
            }
            out.push(String::new());
        }
        Block::Table(rows) => {
            let mut table = Table::new().expand();
            let columns = rows.first().map_or(0, Vec::len);
            for cell in rows.first().into_iter().flatten() {
                table = table.column(&format!("[bold]{}[/]", escape(&render_inlines(cell, true))));
            }
            for row in rows.iter().skip(1) {
                let mut cells: Vec<String> = row
                    .iter()
                    .take(columns)
                    .map(|c| escape(&render_inlines(c, true)))
                    .collect();
                while cells.len() < columns {
                    cells.push(String::new());
                }
                table.row(&cells);
            }
            out.extend(table.lines(width));
            out.push(String::new());
        }
        Block::Html => {} // HTML blocks are skipped in the terminal.
    }
}

fn render_list(ordered: bool, items: &[Vec<Block>], indent: usize, out: &mut Vec<String>) {
    for (index, item) in items.iter().enumerate() {
        let pad = " ".repeat(indent * 2);
        let prefix = if ordered {
            format!("{pad}{}.", index + 1)
        } else {
            format!("{pad}•")
        };
        for (c, child) in item.iter().enumerate() {
            match child {
                Block::Paragraph(text) => {
                    let body = match task_marker(text).filter(|_| c == 0) {
                        Some((glyph, rest)) => format!("{glyph}{}", render_inlines(rest, false)),
                        None => render_inlines(text, false),
                    };
                    out.push(format!(" {prefix} {body}"));
                }
                Block::List {
                    ordered: sub_ordered,
                    items: sub,
                } => render_list(*sub_ordered, sub, indent + 1, out),
                _ => {}
            }
        }
    }
}

// ── Streaming ────────────────────────────────────────────────────────────────

/// Rows kept free below the live tail (the bottom panel and some slack): a tail taller than
/// the window minus this has partly scrolled off and can no longer be erased.
const TAIL_RESERVED_ROWS: usize = 6;

/// `MarkdownStreamRenderer`. C# paints a live "tail" of the re-rendered reply at absolute
/// rows and renders the whole reply once at the end. Here each completed top-level block
/// (a blank line, outside a code fence, followed by a complete line that cannot continue the
/// previous block) is rendered once, and [`MarkdownStreamRenderer::finish`] renders the rest.
///
/// On a terminal the unfinished block is shown live: each token is written as plain,
/// sanitized text the moment it arrives; when its block completes (or on `finish`) that raw
/// text is erased (cursor up over the rows it took, `ESC[J`) and the rendered block printed
/// in its place. A tail taller than the window allows has partly scrolled off: it stays as
/// raw text and that block is not rendered again. On a non-terminal console (tests, piped
/// output) only rendered blocks are printed.
pub struct MarkdownStreamRenderer {
    console: Console,
    renderer: MarkdownRenderer,
    /// The whole reply so far.
    text: String,
    /// Byte offset in `text` up to which blocks have been rendered (or left raw).
    rendered: usize,
    active: bool,
    /// Show the unfinished block live (default: the console is a terminal).
    live: bool,
    /// The raw (sanitized) text currently on screen below the rendered blocks.
    tail: String,
    /// Window height override (tests).
    height: Option<usize>,
}

impl MarkdownStreamRenderer {
    pub fn new(console: Console) -> Self {
        let live = console.is_terminal();
        Self {
            console,
            renderer: MarkdownRenderer,
            text: String::new(),
            rendered: 0,
            active: false,
            live,
            tail: String::new(),
            height: None,
        }
    }

    /// Turn the live tail on or off (on by default on a terminal).
    pub fn set_live(&mut self, live: bool) {
        self.live = live;
    }

    /// Whether streaming is rendering (between `start` and `finish` / `clear`).
    pub fn active(&self) -> bool {
        self.active
    }

    /// `GetBuffer`: the reply so far.
    pub fn buffer(&self) -> &str {
        &self.text
    }

    /// Begin a reply: call when the first visible response token arrives.
    pub fn start(&mut self) {
        self.text.clear();
        self.tail.clear();
        self.rendered = 0;
        self.active = true;
    }

    /// Append a (filtered) token: shown live, and the blocks it completes rendered.
    pub fn append_token(&mut self, token: &str) {
        if token.is_empty() {
            return;
        }
        self.text.push_str(token);
        if !self.active {
            return;
        }
        if self.live {
            self.write_tail(&sanitize(token));
        }
        let Some(cut) = completed_prefix(&self.text[self.rendered..]) else {
            return;
        };
        let chunk = self.text[self.rendered..self.rendered + cut]
            .trim()
            .to_string();
        self.rendered += cut;
        let rest = sanitize(&self.text[self.rendered..]);
        if self.live && !self.erase_tail() {
            // The tail scrolled off: it stays as raw text, the start of the next block
            // included (the cut is at a line start, so that part has rows of its own).
            self.tail = rest;
            return;
        }
        if !chunk.is_empty() {
            self.renderer.render_to(&self.console, &chunk);
        }
        if self.live {
            self.write_tail(&rest);
        }
    }

    /// [`Self::append_token`] that starts the reply on its first text (for an
    /// `AgentObserver::on_text` that has no separate "reply started" event).
    pub fn append(&mut self, token: &str) {
        if !self.active && !token.is_empty() {
            self.start();
        }
        self.append_token(token);
    }

    /// Render what is left and stop.
    pub fn finish(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        crate::repl::agent_footer::LIVE_TAIL.store(false, std::sync::atomic::Ordering::SeqCst);
        let rest = self.text[self.rendered..].trim().to_string();
        self.rendered = self.text.len();
        if self.live && !self.tail.is_empty() && !self.erase_tail() {
            self.end_tail_line();
            self.tail.clear();
            return;
        }
        self.tail.clear();
        if !rest.is_empty() {
            self.renderer.render_to(&self.console, &rest);
        }
    }

    /// Stop as it is (cancel, or switching display modes): the raw tail stays on screen,
    /// ended with a line break so what follows starts on a new line.
    pub fn clear(&mut self) {
        if self.active && self.live {
            self.end_tail_line();
        }
        crate::repl::agent_footer::LIVE_TAIL.store(false, std::sync::atomic::Ordering::SeqCst);
        self.tail.clear();
        self.active = false;
    }

    fn write_tail(&mut self, raw: &str) {
        if raw.is_empty() {
            return;
        }
        if self.live {
            crate::repl::agent_footer::LIVE_TAIL.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.console.raw(raw);
        self.tail.push_str(raw);
    }

    fn end_tail_line(&self) {
        if !self.tail.is_empty() && !self.tail.ends_with('\n') {
            self.console.raw("\n");
        }
    }

    /// Rows the tail takes on screen (wrapped at the console width).
    fn tail_rows(&self) -> usize {
        let width = self.console.width().max(1);
        self.tail
            .split('\n')
            .map(|line| cell_width(line).div_ceil(width).max(1))
            .sum()
    }

    /// Erase the tail. `false` (nothing erased) when it is taller than the window allows.
    fn erase_tail(&mut self) -> bool {
        if self.tail.is_empty() {
            return true;
        }
        let rows = self.tail_rows();
        let height = self.height.unwrap_or_else(|| self.console.height());
        if rows > height.saturating_sub(TAIL_RESERVED_ROWS) {
            return false;
        }
        // Clear the tail's own rows (ESC[J would also wipe the footer's bars under them).
        let up = rows - 1;
        let mut s = if up > 0 {
            format!("\x1b[{up}A\r")
        } else {
            "\r".to_string()
        };
        s.push_str(&"\x1b[2K\x1b[1B".repeat(up));
        s.push_str("\x1b[2K");
        if up > 0 {
            s.push_str(&format!("\x1b[{up}A"));
        }
        self.console.raw(&s);
        self.tail.clear();
        crate::repl::agent_footer::LIVE_TAIL.store(false, std::sync::atomic::Ordering::SeqCst);
        true
    }
}

/// The length of the leading part of `text` that holds only complete top-level blocks (see
/// [`MarkdownStreamRenderer`]), or `None`.
fn completed_prefix(text: &str) -> Option<usize> {
    let mut fence: Option<(char, usize)> = None;
    let mut best = None;
    let mut offset = 0;
    let mut prev_blank = false;
    let mut seen_content = false;
    for line in text.split_inclusive('\n') {
        let complete = line.ends_with('\n');
        let l = line.trim_end_matches('\n');
        if !complete {
            break;
        }
        match fence {
            Some((ch, len)) => {
                if is_fence_close(l, ch, len) {
                    fence = None;
                }
                prev_blank = false;
            }
            None => {
                if is_blank(l) {
                    prev_blank = true;
                } else {
                    let fresh = !l.starts_with([' ', '\t'])
                        && list_marker(l).is_none()
                        && !l.starts_with('|');
                    if prev_blank && seen_content && fresh {
                        best = Some(offset);
                    }
                    if let Some((ch, len, _)) = fence_open(l) {
                        fence = Some((ch, len));
                    }
                    prev_blank = false;
                    seen_content = true;
                }
            }
        }
        offset += line.len();
    }
    best
}

#[cfg(test)]
mod tests;
