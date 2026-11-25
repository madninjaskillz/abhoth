//! JSON helpers shared by the bridges: `JsonElement`-style getters and the compact writer
//! `JsonNode.ToJsonString()` uses (no indentation, `JavaScriptEncoder.Default` escaping).

use std::io::{self, Write};

use serde_json::Value;
use serde_json::ser::{CharEscape, Formatter};

/// `Str(obj, name)`: the property when `obj` is an object and the value is a string.
pub fn str_of<'a>(obj: &'a Value, name: &str) -> Option<&'a str> {
    obj.as_object()?.get(name)?.as_str()
}

/// `Int(obj, name)`: the property when it is a number that fits an `int` (`TryGetInt32`).
pub fn int_of(obj: &Value, name: &str) -> Option<i32> {
    obj.as_object()?
        .get(name)?
        .as_i64()
        .and_then(|i| i32::try_from(i).ok())
}

/// `GetInt64` of a number property (`None` when missing or not an integer).
pub fn long_of(obj: &Value, name: &str) -> Option<i64> {
    obj.as_object()?.get(name)?.as_i64()
}

/// `Has(e, name, out value)`: the property when `obj` is an object that has it (any kind,
/// `null` included).
pub fn prop<'a>(obj: &'a Value, name: &str) -> Option<&'a Value> {
    obj.as_object()?.get(name)
}

/// `JsonElement.GetRawText()` for a value we parsed: compact JSON. (The CLIs write compact
/// JSON, so this is their text except for escapes they chose differently.)
pub fn raw_text(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// `JsonNode.ToJsonString()`: compact, strings escaped like `JavaScriptEncoder.Default`
/// (non-ASCII as `\uXXXX`, plus `"` `&` `'` `+` `<` `>` and backtick).
pub fn to_compact_dotnet(v: &Value) -> String {
    let mut out = Vec::with_capacity(256);
    let mut ser = serde_json::Serializer::with_formatter(&mut out, CompactDotNet);
    serde::Serialize::serialize(v, &mut ser).expect("serializing a Value cannot fail");
    String::from_utf8(out).expect("formatter writes ASCII")
}

struct CompactDotNet;

impl Formatter for CompactDotNet {
    fn write_string_fragment<W: ?Sized + Write>(
        &mut self,
        w: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        for c in fragment.chars() {
            if matches!(c, ' '..='~')
                && !matches!(c, '"' | '&' | '\'' | '+' | '<' | '>' | '\\' | '`')
            {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compact_dotnet_escapes_like_the_default_encoder() {
        let v = json!({"a": "x+y <b> \"q\" é", "n": [1, 2.5, null, true]});
        let u = |h: &str| format!("{}u{h}", char::from(92u8));
        let expected = format!(
            r#"{{"a":"x{}y {}b{} {}q{} {}","n":[1,2.5,null,true]}}"#,
            u("002B"),
            u("003C"),
            u("003E"),
            u("0022"),
            u("0022"),
            u("00E9")
        );
        assert_eq!(to_compact_dotnet(&v), expected);
    }

    #[test]
    fn getters_follow_json_element_rules() {
        let v = json!({"s": "x", "i": 5, "f": 1.5, "big": 5_000_000_000_i64, "n": null});
        assert_eq!(str_of(&v, "s"), Some("x"));
        assert_eq!(str_of(&v, "i"), None);
        assert_eq!(int_of(&v, "i"), Some(5));
        assert_eq!(int_of(&v, "f"), None);
        assert_eq!(int_of(&v, "big"), None);
        assert_eq!(long_of(&v, "big"), Some(5_000_000_000));
        assert!(prop(&v, "n").is_some());
        assert!(prop(&json!("x"), "n").is_none());
    }
}
