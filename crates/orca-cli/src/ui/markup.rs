//! Spectre.Console markup (`[green]text[/]`, `[[` / `]]` escapes) rendered as ANSI, so the
//! console texts of the C# build can be kept as they are. Only what the CLI uses is supported:
//! named colours, `#rrggbb`, `colorN`, `on <colour>` backgrounds, bold / dim / italic /
//! underline / strikethrough, and `link=` (rendered as an OSC 8 hyperlink).

/// `Markup.Escape`: brackets doubled so the text shows literally.
pub fn escape(text: &str) -> String {
    text.replace('[', "[[").replace(']', "]]")
}

/// `Markup.Remove`: the plain text of some markup.
pub fn remove(markup: &str) -> String {
    let mut out = String::new();
    for seg in parse(markup) {
        if let Segment::Text(t) = seg {
            out.push_str(&t);
        }
    }
    out
}

/// The markup as ANSI text (`ansi` false: plain text).
pub fn render(markup: &str, ansi: bool) -> String {
    if !ansi {
        return remove(markup);
    }
    let mut out = String::new();
    let mut stack: Vec<Style> = Vec::new();
    let mut styled = false;
    for seg in parse(markup) {
        match seg {
            Segment::Open(spec) => stack.push(Style::parse(&spec)),
            Segment::Close => {
                stack.pop();
            }
            Segment::Text(t) => {
                if t.is_empty() {
                    continue;
                }
                let combined = Style::combine(&stack);
                if combined.is_plain() {
                    if styled {
                        out.push_str("\x1b[0m");
                        styled = false;
                    }
                    out.push_str(&t);
                } else {
                    out.push_str("\x1b[0m");
                    out.push_str(&combined.sgr());
                    if let Some(link) = &combined.link {
                        out.push_str(&format!("\x1b]8;;{link}\x1b\\{t}\x1b]8;;\x1b\\"));
                    } else {
                        out.push_str(&t);
                    }
                    styled = true;
                }
            }
        }
    }
    if styled {
        out.push_str("\x1b[0m");
    }
    out
}

#[derive(Debug, PartialEq)]
enum Segment {
    Text(String),
    Open(String),
    Close,
}

fn parse(markup: &str) -> Vec<Segment> {
    let mut segs = Vec::new();
    let mut text = String::new();
    let mut chars = markup.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '[' if chars.peek() == Some(&'[') => {
                chars.next();
                text.push('[');
            }
            ']' if chars.peek() == Some(&']') => {
                chars.next();
                text.push(']');
            }
            '[' => {
                let mut tag = String::new();
                let mut closed = false;
                for t in chars.by_ref() {
                    if t == ']' {
                        closed = true;
                        break;
                    }
                    tag.push(t);
                }
                if !closed {
                    // Malformed: keep it as text rather than failing the line.
                    text.push('[');
                    text.push_str(&tag);
                    continue;
                }
                if !text.is_empty() {
                    segs.push(Segment::Text(std::mem::take(&mut text)));
                }
                if tag.trim() == "/" {
                    segs.push(Segment::Close);
                } else {
                    segs.push(Segment::Open(tag));
                }
            }
            _ => text.push(c),
        }
    }
    if !text.is_empty() {
        segs.push(Segment::Text(text));
    }
    segs
}

#[derive(Debug, Clone, Default)]
struct Style {
    fg: Option<String>,
    bg: Option<String>,
    attrs: Vec<u8>,
    link: Option<String>,
}

impl Style {
    fn parse(spec: &str) -> Self {
        let mut s = Style::default();
        let mut words = spec.split_whitespace().peekable();
        while let Some(w) = words.next() {
            let lw = w.to_ascii_lowercase();
            if let Some(url) = w.strip_prefix("link=") {
                s.link = Some(url.to_string());
                continue;
            }
            match lw.as_str() {
                "bold" => s.attrs.push(1),
                "dim" => s.attrs.push(2),
                "italic" => s.attrs.push(3),
                "underline" => s.attrs.push(4),
                "invert" => s.attrs.push(7),
                "strikethrough" => s.attrs.push(9),
                "default" => {}
                "on" => {
                    if let Some(bg) = words.next() {
                        s.bg = color_code(&bg.to_ascii_lowercase(), true);
                    }
                }
                _ => {
                    if let Some(code) = color_code(&lw, false) {
                        s.fg = Some(code);
                    }
                }
            }
        }
        s
    }

    fn combine(stack: &[Style]) -> Style {
        let mut out = Style::default();
        for s in stack {
            if s.fg.is_some() {
                out.fg = s.fg.clone();
            }
            if s.bg.is_some() {
                out.bg = s.bg.clone();
            }
            out.attrs.extend(&s.attrs);
            if s.link.is_some() {
                out.link = s.link.clone();
            }
        }
        out
    }

    fn is_plain(&self) -> bool {
        self.fg.is_none() && self.bg.is_none() && self.attrs.is_empty() && self.link.is_none()
    }

    fn sgr(&self) -> String {
        let mut parts: Vec<String> = self.attrs.iter().map(|a| a.to_string()).collect();
        if let Some(fg) = &self.fg {
            parts.push(fg.clone());
        }
        if let Some(bg) = &self.bg {
            parts.push(bg.clone());
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("\x1b[{}m", parts.join(";"))
        }
    }
}

/// The SGR parameters of a Spectre colour name (foreground, or background with `bg`).
fn color_code(name: &str, bg: bool) -> Option<String> {
    let base = if bg { 40 } else { 30 };
    let basic = |n: u8| Some((base + n).to_string());
    let bright = |n: u8| Some((base + 60 + n).to_string());
    let indexed = |n: u8| Some(format!("{};5;{n}", if bg { 48 } else { 38 }));
    if let Some(hex) = name.strip_prefix('#')
        && hex.len() == 6
        && let Ok(v) = u32::from_str_radix(hex, 16)
    {
        return Some(format!(
            "{};2;{};{};{}",
            if bg { 48 } else { 38 },
            v >> 16,
            (v >> 8) & 0xff,
            v & 0xff
        ));
    }
    if let Some(n) = name.strip_prefix("color")
        && let Ok(n) = n.parse::<u8>()
    {
        return indexed(n);
    }
    match name {
        "black" => basic(0),
        "maroon" => basic(1),
        "green" => basic(2),
        "olive" => basic(3),
        "navy" => basic(4),
        "purple" => basic(5),
        "teal" => basic(6),
        "silver" => basic(7),
        "grey" | "gray" => bright(0),
        "red" => bright(1),
        "lime" => bright(2),
        "yellow" => bright(3),
        "blue" => bright(4),
        "fuchsia" | "magenta" => bright(5),
        "aqua" | "cyan" => bright(6),
        "white" => bright(7),
        "cyan1" => indexed(51),
        "magenta1" => indexed(201),
        "green1" => indexed(46),
        "yellow1" => indexed(226),
        "orange1" => indexed(214),
        "red1" => indexed(196),
        "blue1" => indexed(21),
        "grey50" | "gray50" => indexed(244),
        "grey37" | "gray37" => indexed(59),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_doubles_brackets() {
        assert_eq!(escape("[a] b"), "[[a]] b");
        assert_eq!(remove(&escape("[a] b")), "[a] b");
    }

    #[test]
    fn remove_strips_tags() {
        assert_eq!(
            remove("[green]Session saved: [bold]x[/][/] done"),
            "Session saved: x done"
        );
    }

    #[test]
    fn render_plain_when_not_ansi() {
        assert_eq!(render("[red]oops[/]", false), "oops");
    }

    #[test]
    fn render_nested_styles() {
        let s = render("[yellow]a [bold]b[/] c[/]", true);
        assert_eq!(
            s,
            "\x1b[0m\x1b[93ma \x1b[0m\x1b[1;93mb\x1b[0m\x1b[93m c\x1b[0m"
        );
    }

    #[test]
    fn render_unknown_tag_keeps_text() {
        assert_eq!(render("[nosuch]x[/]", true), "x");
        assert_eq!(remove("a [b"), "a [b");
    }
}
