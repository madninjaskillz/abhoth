//! .NET text helpers the web tools use: `WebUtility.HtmlDecode`, `Uri.EscapeDataString` and
//! `Uri.UnescapeDataString`.

/// `WebUtility.HtmlDecode`: `&name;` for the 253 HTML 4 entities (plus `apos`), `&#NNN;` and
/// `&#xHH;`. An `&` followed by another `&` before any `;` is left alone; unknown named
/// entities stay as written; a numeric entity that does not parse leaves the `&` literal.
pub fn html_decode(value: &str) -> String {
    if !value.contains('&') {
        return value.to_string();
    }
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '&' {
            let rest = &chars[i + 1..];
            if let Some(len) = rest.iter().position(|&c| c == ';' || c == '&')
                && rest[len] == ';'
            {
                let end = i + 1 + len;
                if len > 1 && rest[0] == '#' {
                    let parsed = if rest[1] == 'x' || rest[1] == 'X' {
                        parse_hex(&rest[2..len])
                    } else {
                        parse_decimal(&rest[1..len])
                    };
                    // U+0000 .. U+10FFFF without surrogates.
                    if let Some(c) = parsed.and_then(char::from_u32) {
                        out.push(c);
                        i = end + 1;
                        continue;
                    }
                } else {
                    let name: String = rest[..len].iter().collect();
                    i = end + 1;
                    match lookup_entity(&name) {
                        Some(c) => out.push(c),
                        None => {
                            out.push('&');
                            out.push_str(&name);
                            out.push(';');
                        }
                    }
                    continue;
                }
            }
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// `uint.TryParse(s, NumberStyles.AllowHexSpecifier)`: hex digits only.
fn parse_hex(s: &[char]) -> Option<u32> {
    if s.is_empty() || !s.iter().all(char::is_ascii_hexdigit) {
        return None;
    }
    u32::from_str_radix(&s.iter().collect::<String>(), 16).ok()
}

/// `uint.TryParse(s, NumberStyles.Integer)`: optional surrounding whitespace and a sign.
fn parse_decimal(s: &[char]) -> Option<u32> {
    let text: String = s.iter().collect();
    let t =
        text.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r'));
    let (negative, digits) = match t.as_bytes().first() {
        Some(b'+') => (false, &t[1..]),
        Some(b'-') => (true, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v: u32 = digits.parse().ok()?;
    // "-0" parses as 0; any other negative value is out of range for uint.
    if negative && v != 0 { None } else { Some(v) }
}

fn lookup_entity(name: &str) -> Option<char> {
    ENTITIES
        .iter()
        .find(|(n, _)| *n == name)
        .and_then(|&(_, c)| char::from_u32(c))
}

/// `WebUtility.HtmlEntities`: HTML 4.01 (Latin-1, symbols, special) plus `apos`.
#[rustfmt::skip]
const ENTITIES: &[(&str, u32)] = &[
    ("quot", 34), ("amp", 38), ("apos", 39), ("lt", 60), ("gt", 62),
    ("nbsp", 160), ("iexcl", 161), ("cent", 162), ("pound", 163), ("curren", 164),
    ("yen", 165), ("brvbar", 166), ("sect", 167), ("uml", 168), ("copy", 169),
    ("ordf", 170), ("laquo", 171), ("not", 172), ("shy", 173), ("reg", 174),
    ("macr", 175), ("deg", 176), ("plusmn", 177), ("sup2", 178), ("sup3", 179),
    ("acute", 180), ("micro", 181), ("para", 182), ("middot", 183), ("cedil", 184),
    ("sup1", 185), ("ordm", 186), ("raquo", 187), ("frac14", 188), ("frac12", 189),
    ("frac34", 190), ("iquest", 191), ("Agrave", 192), ("Aacute", 193), ("Acirc", 194),
    ("Atilde", 195), ("Auml", 196), ("Aring", 197), ("AElig", 198), ("Ccedil", 199),
    ("Egrave", 200), ("Eacute", 201), ("Ecirc", 202), ("Euml", 203), ("Igrave", 204),
    ("Iacute", 205), ("Icirc", 206), ("Iuml", 207), ("ETH", 208), ("Ntilde", 209),
    ("Ograve", 210), ("Oacute", 211), ("Ocirc", 212), ("Otilde", 213), ("Ouml", 214),
    ("times", 215), ("Oslash", 216), ("Ugrave", 217), ("Uacute", 218), ("Ucirc", 219),
    ("Uuml", 220), ("Yacute", 221), ("THORN", 222), ("szlig", 223), ("agrave", 224),
    ("aacute", 225), ("acirc", 226), ("atilde", 227), ("auml", 228), ("aring", 229),
    ("aelig", 230), ("ccedil", 231), ("egrave", 232), ("eacute", 233), ("ecirc", 234),
    ("euml", 235), ("igrave", 236), ("iacute", 237), ("icirc", 238), ("iuml", 239),
    ("eth", 240), ("ntilde", 241), ("ograve", 242), ("oacute", 243), ("ocirc", 244),
    ("otilde", 245), ("ouml", 246), ("divide", 247), ("oslash", 248), ("ugrave", 249),
    ("uacute", 250), ("ucirc", 251), ("uuml", 252), ("yacute", 253), ("thorn", 254),
    ("yuml", 255), ("OElig", 338), ("oelig", 339), ("Scaron", 352), ("scaron", 353),
    ("Yuml", 376), ("fnof", 402), ("circ", 710), ("tilde", 732), ("Alpha", 913),
    ("Beta", 914), ("Gamma", 915), ("Delta", 916), ("Epsilon", 917), ("Zeta", 918),
    ("Eta", 919), ("Theta", 920), ("Iota", 921), ("Kappa", 922), ("Lambda", 923),
    ("Mu", 924), ("Nu", 925), ("Xi", 926), ("Omicron", 927), ("Pi", 928),
    ("Rho", 929), ("Sigma", 931), ("Tau", 932), ("Upsilon", 933), ("Phi", 934),
    ("Chi", 935), ("Psi", 936), ("Omega", 937), ("alpha", 945), ("beta", 946),
    ("gamma", 947), ("delta", 948), ("epsilon", 949), ("zeta", 950), ("eta", 951),
    ("theta", 952), ("iota", 953), ("kappa", 954), ("lambda", 955), ("mu", 956),
    ("nu", 957), ("xi", 958), ("omicron", 959), ("pi", 960), ("rho", 961),
    ("sigmaf", 962), ("sigma", 963), ("tau", 964), ("upsilon", 965), ("phi", 966),
    ("chi", 967), ("psi", 968), ("omega", 969), ("thetasym", 977), ("upsih", 978),
    ("piv", 982), ("ensp", 8194), ("emsp", 8195), ("thinsp", 8201), ("zwnj", 8204),
    ("zwj", 8205), ("lrm", 8206), ("rlm", 8207), ("ndash", 8211), ("mdash", 8212),
    ("lsquo", 8216), ("rsquo", 8217), ("sbquo", 8218), ("ldquo", 8220), ("rdquo", 8221),
    ("bdquo", 8222), ("dagger", 8224), ("Dagger", 8225), ("bull", 8226), ("hellip", 8230),
    ("permil", 8240), ("prime", 8242), ("Prime", 8243), ("lsaquo", 8249), ("rsaquo", 8250),
    ("oline", 8254), ("frasl", 8260), ("euro", 8364), ("image", 8465), ("weierp", 8472),
    ("real", 8476), ("trade", 8482), ("alefsym", 8501), ("larr", 8592), ("uarr", 8593),
    ("rarr", 8594), ("darr", 8595), ("harr", 8596), ("crarr", 8629), ("lArr", 8656),
    ("uArr", 8657), ("rArr", 8658), ("dArr", 8659), ("hArr", 8660), ("forall", 8704),
    ("part", 8706), ("exist", 8707), ("empty", 8709), ("nabla", 8711), ("isin", 8712),
    ("notin", 8713), ("ni", 8715), ("prod", 8719), ("sum", 8721), ("minus", 8722),
    ("lowast", 8727), ("radic", 8730), ("prop", 8733), ("infin", 8734), ("ang", 8736),
    ("and", 8743), ("or", 8744), ("cap", 8745), ("cup", 8746), ("int", 8747),
    ("there4", 8756), ("sim", 8764), ("cong", 8773), ("asymp", 8776), ("ne", 8800),
    ("equiv", 8801), ("le", 8804), ("ge", 8805), ("sub", 8834), ("sup", 8835),
    ("nsub", 8836), ("sube", 8838), ("supe", 8839), ("oplus", 8853), ("otimes", 8855),
    ("perp", 8869), ("sdot", 8901), ("lceil", 8968), ("rceil", 8969), ("lfloor", 8970),
    ("rfloor", 8971), ("lang", 9001), ("rang", 9002), ("loz", 9674), ("spades", 9824),
    ("clubs", 9827), ("hearts", 9829), ("diams", 9830),
];

/// `Uri.EscapeDataString`: UTF-8 percent-encoding of everything but `A-Z a-z 0-9 - _ . ~`.
pub fn escape_data_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `Uri.UnescapeDataString`: decodes `%XX` sequences. Escaped bytes that do not form valid
/// UTF-8 are left escaped, as .NET does.
pub fn unescape_data_string(value: &str) -> String {
    if !value.contains('%') {
        return value.to_string();
    }
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < bytes.len() {
        // A run of %XX escapes is decoded together (a UTF-8 sequence spans several).
        let start = i;
        let mut decoded = Vec::new();
        while i + 2 < bytes.len()
            && bytes[i] == b'%'
            && hex_pair(bytes[i + 1], bytes[i + 2]).is_some()
        {
            decoded.push(hex_pair(bytes[i + 1], bytes[i + 2]).unwrap());
            i += 3;
        }
        if i > start {
            push_decoded_run(&mut out, &decoded, &value[start..i]);
            continue;
        }
        let ch = value[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn push_decoded_run(out: &mut String, decoded: &[u8], escaped: &str) {
    let mut pos = 0;
    while pos < decoded.len() {
        match std::str::from_utf8(&decoded[pos..]) {
            Ok(s) => {
                out.push_str(s);
                return;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                out.push_str(std::str::from_utf8(&decoded[pos..pos + valid]).unwrap());
                pos += valid;
                let bad = e.error_len().unwrap_or(decoded.len() - pos);
                // Each byte is three characters (%XX) of the escaped text.
                out.push_str(&escaped[pos * 3..(pos + bad) * 3]);
                pos += bad;
            }
        }
    }
}

fn hex_pair(a: u8, b: u8) -> Option<u8> {
    let h = (a as char).to_digit(16)?;
    let l = (b as char).to_digit(16)?;
    Some((h * 16 + l) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_table_has_253_entries() {
        assert_eq!(ENTITIES.len(), 253);
    }

    #[test]
    fn decodes_named_and_numeric_entities() {
        assert_eq!(
            html_decode("a &amp; b &lt;c&gt; &quot;&apos;"),
            "a & b <c> \"'"
        );
        assert_eq!(html_decode("&#65;&#x42;&#X43;&nbsp;"), "ABC\u{a0}");
        assert_eq!(html_decode("&#128512;"), "\u{1F600}");
        assert_eq!(
            html_decode("&eacute;&Eacute;&hellip;&euro;"),
            "\u{e9}\u{c9}\u{2026}\u{20ac}"
        );
    }

    #[test]
    fn leaves_unknown_and_broken_entities() {
        assert_eq!(html_decode("&bogus; &amp"), "&bogus; &amp");
        assert_eq!(html_decode("a & b &amp; c"), "a & b & c");
        assert_eq!(html_decode("&&amp;"), "&&");
        assert_eq!(html_decode("&#xZZ;"), "&#xZZ;");
        assert_eq!(html_decode("&#;"), "&#;");
        assert_eq!(html_decode("&#xD800;"), "&#xD800;");
        assert_eq!(html_decode("&AMP;"), "&AMP;");
        assert_eq!(html_decode("&# 65;"), "A");
    }

    #[test]
    fn escapes_data_strings() {
        assert_eq!(escape_data_string("a b&c/é~"), "a%20b%26c%2F%C3%A9~");
        assert_eq!(unescape_data_string("a%20b%26c%2F%C3%A9"), "a b&c/é");
        assert_eq!(unescape_data_string("100%"), "100%");
        assert_eq!(unescape_data_string("%zz%41"), "%zzA");
        assert_eq!(unescape_data_string("%FFok"), "%FFok");
    }
}
