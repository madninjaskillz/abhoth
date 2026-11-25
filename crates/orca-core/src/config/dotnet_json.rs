//! JSON output that matches System.Text.Json with `WriteIndented = true` and the default
//! `JavaScriptEncoder`, so a config the C# version wrote comes back byte for byte:
//!
//! * two-space indent, `"key": value`, empty arrays/objects as `[]` / `{}`;
//! * line breaks are `Environment.NewLine` (`\r\n` on Windows);
//! * strings escape everything outside printable ASCII as `\uXXXX` (upper-case hex, UTF-16
//!   surrogate pairs), plus the HTML-sensitive `"` `&` `'` `+` `<` `>` and backtick; `\\`,
//!   `\n`, `\r`, `\t`, `\b`, `\f` keep their short forms;
//! * numbers use the shortest round-trip form with no `.0` on whole values (`1`, `0.7`).

use std::io::{self, Write};

use serde::Serialize;
use serde_json::ser::{CharEscape, Formatter};

/// The platform's `Environment.NewLine`.
pub const NEWLINE: &str = if cfg!(windows) { "\r\n" } else { "\n" };

/// Serialize `value` the way `JsonSerializer.Serialize(value, JsonDefaults.Options)` does.
pub fn to_string_dotnet<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<String> {
    to_string_dotnet_with_newline(value, NEWLINE)
}

/// [`to_string_dotnet`] with an explicit line break (tests compare both platforms' output).
pub fn to_string_dotnet_with_newline<T: Serialize + ?Sized>(
    value: &T,
    newline: &str,
) -> serde_json::Result<String> {
    let mut out = Vec::with_capacity(4096);
    let mut ser = serde_json::Serializer::with_formatter(&mut out, DotNetFormatter::new(newline));
    value.serialize(&mut ser)?;
    // The formatter only ever writes valid UTF-8 (ASCII, in fact).
    Ok(String::from_utf8(out).expect("formatter writes ASCII"))
}

pub struct DotNetFormatter<'a> {
    indent: usize,
    has_value: bool,
    newline: &'a str,
}

impl<'a> DotNetFormatter<'a> {
    pub fn new(newline: &'a str) -> Self {
        Self {
            indent: 0,
            has_value: false,
            newline,
        }
    }

    fn write_indent<W: ?Sized + Write>(&self, w: &mut W) -> io::Result<()> {
        for _ in 0..self.indent {
            w.write_all(b"  ")?;
        }
        Ok(())
    }
}

fn write_float<W: ?Sized + Write>(w: &mut W, text: String) -> io::Result<()> {
    // Rust's Display already gives the shortest round-trip digits and drops ".0".
    w.write_all(text.as_bytes())
}

impl Formatter for DotNetFormatter<'_> {
    fn write_f32<W: ?Sized + Write>(&mut self, w: &mut W, value: f32) -> io::Result<()> {
        write_float(w, format!("{value}"))
    }

    fn write_f64<W: ?Sized + Write>(&mut self, w: &mut W, value: f64) -> io::Result<()> {
        write_float(w, format!("{value}"))
    }

    fn write_string_fragment<W: ?Sized + Write>(
        &mut self,
        w: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        for c in fragment.chars() {
            if is_allowed(c) {
                w.write_all(&[c as u8])?;
            } else {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    write!(w, "\\u{:04X}", unit)?;
                }
            }
        }
        Ok(())
    }

    fn write_char_escape<W: ?Sized + Write>(
        &mut self,
        w: &mut W,
        escape: CharEscape,
    ) -> io::Result<()> {
        let s: String = match escape {
            CharEscape::Quote => "\\u0022".into(),
            CharEscape::ReverseSolidus => "\\\\".into(),
            CharEscape::Solidus => "/".into(),
            CharEscape::Backspace => "\\b".into(),
            CharEscape::FormFeed => "\\f".into(),
            CharEscape::LineFeed => "\\n".into(),
            CharEscape::CarriageReturn => "\\r".into(),
            CharEscape::Tab => "\\t".into(),
            CharEscape::AsciiControl(b) => format!("\\u{:04X}", b),
        };
        w.write_all(s.as_bytes())
    }

    fn begin_array<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.indent += 1;
        self.has_value = false;
        w.write_all(b"[")
    }

    fn end_array<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.indent -= 1;
        if self.has_value {
            w.write_all(self.newline.as_bytes())?;
            self.write_indent(w)?;
        }
        w.write_all(b"]")
    }

    fn begin_array_value<W: ?Sized + Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        if !first {
            w.write_all(b",")?;
        }
        w.write_all(self.newline.as_bytes())?;
        self.write_indent(w)
    }

    fn end_array_value<W: ?Sized + Write>(&mut self, _w: &mut W) -> io::Result<()> {
        self.has_value = true;
        Ok(())
    }

    fn begin_object<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.indent += 1;
        self.has_value = false;
        w.write_all(b"{")
    }

    fn end_object<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.indent -= 1;
        if self.has_value {
            w.write_all(self.newline.as_bytes())?;
            self.write_indent(w)?;
        }
        w.write_all(b"}")
    }

    fn begin_object_key<W: ?Sized + Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        if !first {
            w.write_all(b",")?;
        }
        w.write_all(self.newline.as_bytes())?;
        self.write_indent(w)
    }

    fn begin_object_value<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(b": ")
    }

    fn end_object_value<W: ?Sized + Write>(&mut self, _w: &mut W) -> io::Result<()> {
        self.has_value = true;
        Ok(())
    }
}

/// Characters `JavaScriptEncoder.Default` writes as themselves: printable ASCII except the
/// HTML-sensitive ones. (`"` and `\` never reach here: serde_json routes them to
/// [`Formatter::write_char_escape`].)
fn is_allowed(c: char) -> bool {
    matches!(c, ' '..='~') && !matches!(c, '"' | '&' | '\'' | '+' | '<' | '>' | '\\' | '`')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn escapes_like_javascript_encoder_default() {
        let s = to_string_dotnet_with_newline(&json!("k'+<>&\"\\\té😀`"), "\n").unwrap();
        let u = |h: &str| format!("{}u{h}", '\\');
        let expected = format!(
            "\"k{}{}{}{}{}{}\\\\\\t{}{}{}{}\"",
            u("0027"),
            u("002B"),
            u("003C"),
            u("003E"),
            u("0026"),
            u("0022"),
            u("00E9"),
            u("D83D"),
            u("DE00"),
            u("0060")
        );
        assert_eq!(s, expected);
    }

    #[test]
    fn indents_like_write_indented() {
        let v = json!({"a": [], "b": {}, "c": [1, 2.5, "s"], "d": {"e": true}});
        let s = to_string_dotnet_with_newline(&v, "\n").unwrap();
        assert_eq!(
            s,
            "{\n  \"a\": [],\n  \"b\": {},\n  \"c\": [\n    1,\n    2.5,\n    \"s\"\n  ],\n  \"d\": {\n    \"e\": true\n  }\n}"
        );
    }

    #[test]
    fn floats_have_no_trailing_zero() {
        #[derive(Serialize)]
        struct F {
            a: f32,
            b: f64,
            c: f32,
            d: f64,
        }
        let s = to_string_dotnet_with_newline(
            &F {
                a: 0.7,
                b: 1.0,
                c: 1.0,
                d: 0.1,
            },
            "\n",
        )
        .unwrap();
        assert_eq!(
            s,
            "{\n  \"a\": 0.7,\n  \"b\": 1,\n  \"c\": 1,\n  \"d\": 0.1\n}"
        );
    }

    #[test]
    fn uses_the_given_newline() {
        let s = to_string_dotnet_with_newline(&json!({"a": 1}), "\r\n").unwrap();
        assert_eq!(s, "{\r\n  \"a\": 1\r\n}");
    }
}
