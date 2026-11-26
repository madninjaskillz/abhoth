//! Command templates (ROUTER-CONFIG.md 4.5).
//!
//! `{name}` is replaced by a variable; `{{` and `}}` are literal braces. In shell mode every
//! substitution is quoted as one word, and a list variable becomes several quoted words, so a
//! value can never add shell syntax. In word mode (argv elements, paths) values are inserted as
//! they are, and a list variable is an error unless it is the whole element.
//!
//! Unknown variables are errors, which validation reports before anything runs.

use std::collections::BTreeMap;

/// A variable's value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TplValue {
    Word(String),
    List(Vec<String>),
}

impl From<&str> for TplValue {
    fn from(s: &str) -> Self {
        TplValue::Word(s.to_string())
    }
}
impl From<String> for TplValue {
    fn from(s: String) -> Self {
        TplValue::Word(s)
    }
}
impl From<Vec<String>> for TplValue {
    fn from(v: Vec<String>) -> Self {
        TplValue::List(v)
    }
}

/// Variables by name (`card`, `var:ai_root`, `secret_file:engine_key`, …).
pub type Vars = BTreeMap<String, TplValue>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TplError {
    Unknown(String),
    Unbalanced(usize),
    ListInWord(String),
}

impl std::fmt::Display for TplError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TplError::Unknown(v) => write!(f, "unknown template variable {{{v}}}"),
            TplError::Unbalanced(at) => write!(
                f,
                "unbalanced brace at byte {at} (use {{{{ or }}}} for a literal brace)"
            ),
            TplError::ListInWord(v) => write!(f, "list variable {{{v}}} must be a whole argument"),
        }
    }
}

impl std::error::Error for TplError {}

/// One piece of a parsed template.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece<'a> {
    Lit(String),
    Var(&'a str),
}

fn parse(tpl: &str) -> Result<Vec<Piece<'_>>, TplError> {
    let b = tpl.as_bytes();
    let mut out = Vec::new();
    let mut lit = String::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'{' if b.get(i + 1) == Some(&b'{') => {
                lit.push('{');
                i += 2;
            }
            b'}' if b.get(i + 1) == Some(&b'}') => {
                lit.push('}');
                i += 2;
            }
            b'{' => {
                let end = tpl[i + 1..].find('}').ok_or(TplError::Unbalanced(i))? + i + 1;
                let name = &tpl[i + 1..end];
                if name.is_empty() || name.contains('{') || name.contains(char::is_whitespace) {
                    return Err(TplError::Unbalanced(i));
                }
                if !lit.is_empty() {
                    out.push(Piece::Lit(std::mem::take(&mut lit)));
                }
                out.push(Piece::Var(name));
                i = end + 1;
            }
            b'}' => return Err(TplError::Unbalanced(i)),
            _ => {
                let ch = tpl[i..].chars().next().unwrap_or('\u{fffd}');
                lit.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    if !lit.is_empty() {
        out.push(Piece::Lit(lit));
    }
    Ok(out)
}

/// The variable names a template uses (for validation and allowlist export).
pub fn variables(tpl: &str) -> Result<Vec<String>, TplError> {
    Ok(parse(tpl)?
        .into_iter()
        .filter_map(|p| match p {
            Piece::Var(v) => Some(v.to_string()),
            Piece::Lit(_) => None,
        })
        .collect())
}

/// POSIX shell quoting: a bare word when it is safe, else single quotes.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_./:=,@%+".contains(&c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Render for `/bin/sh -c`: each substitution is one quoted word (lists: several).
pub fn render_shell(tpl: &str, vars: &Vars) -> Result<String, TplError> {
    let mut out = String::new();
    for p in parse(tpl)? {
        match p {
            Piece::Lit(l) => out.push_str(&l),
            Piece::Var(v) => match vars.get(v) {
                Some(TplValue::Word(w)) => out.push_str(&shell_quote(w)),
                Some(TplValue::List(l)) => out.push_str(
                    &l.iter()
                        .map(|w| shell_quote(w))
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                None => return Err(TplError::Unknown(v.to_string())),
            },
        }
    }
    Ok(out)
}

/// Render one argv element or path: values inserted as they are.
pub fn render_word(tpl: &str, vars: &Vars) -> Result<String, TplError> {
    let mut out = String::new();
    for p in parse(tpl)? {
        match p {
            Piece::Lit(l) => out.push_str(&l),
            Piece::Var(v) => match vars.get(v) {
                Some(TplValue::Word(w)) => out.push_str(w),
                Some(TplValue::List(_)) => return Err(TplError::ListInWord(v.to_string())),
                None => return Err(TplError::Unknown(v.to_string())),
            },
        }
    }
    Ok(out)
}

/// Render an argv list: an element that is exactly `{list}` expands to the list's words.
pub fn render_argv(items: &[String], vars: &Vars) -> Result<Vec<String>, TplError> {
    let mut out = Vec::new();
    for it in items {
        let pieces = parse(it)?;
        if let [Piece::Var(v)] = pieces.as_slice()
            && let Some(TplValue::List(l)) = vars.get(*v)
        {
            out.extend(l.iter().cloned());
            continue;
        }
        out.push(render_word(it, vars)?);
    }
    Ok(out)
}

/// Join an argv as one shell command line (each word quoted).
pub fn argv_to_shell(argv: &[String]) -> String {
    argv.iter()
        .map(|w| shell_quote(w))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, TplValue)]) -> Vars {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn quotes_hostile_values() {
        let v = vars(&[
            ("a", "x y".into()),
            ("b", "it's".into()),
            ("c", "$(rm -rf /)".into()),
            ("d", "`id`\nnext".into()),
            ("e", "".into()),
        ]);
        let s = render_shell("echo {a} {b} {c} {d} {e}", &v).unwrap();
        assert_eq!(s, "echo 'x y' 'it'\\''s' '$(rm -rf /)' '`id`\nnext' ''");
    }

    #[test]
    fn lists_expand_to_words_and_braces_escape() {
        let v = vars(&[("args", TplValue::List(vec!["--a".into(), "b c".into()]))]);
        assert_eq!(
            render_shell("run {args} {{literal}}", &v).unwrap(),
            "run --a 'b c' {literal}"
        );
        assert_eq!(
            render_argv(&["x".into(), "{args}".into(), "--p={args}".into()], &v),
            Err(TplError::ListInWord("args".into()))
        );
    }

    #[test]
    fn unknown_and_unbalanced() {
        assert_eq!(
            render_shell("a {nope}", &Vars::new()),
            Err(TplError::Unknown("nope".into()))
        );
        assert!(matches!(
            render_shell("a {b", &Vars::new()),
            Err(TplError::Unbalanced(_))
        ));
        assert!(matches!(
            render_shell("a } b", &Vars::new()),
            Err(TplError::Unbalanced(_))
        ));
        assert_eq!(variables("{a}-{var:x}{{").unwrap(), vec!["a", "var:x"]);
    }

    #[test]
    fn word_mode_keeps_values() {
        let v = vars(&[("card_lc", "a".into()), ("var:ninfer", "/x y".into())]);
        assert_eq!(
            render_word("{var:ninfer}/req-{card_lc}.jsonl", &v).unwrap(),
            "/x y/req-a.jsonl"
        );
    }
}
