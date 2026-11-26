//! A tiny evaluator for the module-level constants of `layoutctl.py` (`NAME = <literal>`):
//! strings, numbers, True/False/None, tuples, lists, dicts, names of earlier string constants
//! and `+` between strings. Enough to read CARDS, NINFER_DEFAULTS and friends from the file
//! itself instead of copying them into code (the importer must follow the live file).

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// `NAME -> value` for every top-level assignment whose right side evaluates.
pub fn constants(src: &str) -> BTreeMap<String, Value> {
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    let lines: Vec<&str> = src.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        let name: String = l
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
            .collect();
        let rest = &l[name.len()..];
        if name.is_empty()
            || !name.starts_with(|c: char| c.is_ascii_uppercase())
            || !rest.starts_with(" = ")
        {
            i += 1;
            continue;
        }
        // Gather lines until brackets balance.
        let mut text = rest[3..].to_string();
        let mut j = i;
        while depth(&text) > 0 && j + 1 < lines.len() {
            j += 1;
            text.push('\n');
            text.push_str(lines[j]);
        }
        let mut p = Parser {
            s: text.as_bytes(),
            i: 0,
            names: &out,
        };
        if let Some(v) = p.expr() {
            p.ws();
            if p.i >= p.s.len() {
                out.insert(name, v);
            }
        }
        i = j + 1;
    }
    out
}

/// Bracket depth at the end of `t`, ignoring strings and comments.
fn depth(t: &str) -> i32 {
    let b = t.as_bytes();
    let (mut d, mut i) = (0, 0);
    while i < b.len() {
        match b[i] {
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            q @ (b'"' | b'\'') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'(' | b'[' | b'{' => d += 1,
            b')' | b']' | b'}' => d -= 1,
            _ => {}
        }
        i += 1;
    }
    d
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    names: &'a BTreeMap<String, Value>,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() {
            match self.s[self.i] {
                b' ' | b'\t' | b'\n' | b'\r' => self.i += 1,
                b'#' => {
                    while self.i < self.s.len() && self.s[self.i] != b'\n' {
                        self.i += 1;
                    }
                }
                _ => break,
            }
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expr(&mut self) -> Option<Value> {
        let mut v = self.atom()?;
        while self.eat(b'+') {
            let r = self.atom()?;
            v = match (v, r) {
                (Value::String(a), Value::String(b)) => Value::String(a + &b),
                _ => return None,
            };
        }
        Some(v)
    }

    fn seq(&mut self, close: u8) -> Option<Vec<Value>> {
        let mut out = Vec::new();
        loop {
            if self.eat(close) {
                return Some(out);
            }
            out.push(self.expr()?);
            if !self.eat(b',') {
                return if self.eat(close) { Some(out) } else { None };
            }
        }
    }

    fn atom(&mut self) -> Option<Value> {
        match self.peek()? {
            b'(' => {
                self.i += 1;
                // A parenthesised expression or a tuple.
                let items = self.seq(b')')?;
                Some(Value::Array(items))
            }
            b'[' => {
                self.i += 1;
                Some(Value::Array(self.seq(b']')?))
            }
            b'{' => {
                self.i += 1;
                let mut m = Map::new();
                loop {
                    if self.eat(b'}') {
                        return Some(Value::Object(m));
                    }
                    let k = match self.expr()? {
                        Value::String(s) => s,
                        Value::Number(n) => n.to_string(),
                        _ => return None,
                    };
                    if !self.eat(b':') {
                        return None;
                    }
                    let v = self.expr()?;
                    m.insert(k, v);
                    if !self.eat(b',') {
                        return if self.eat(b'}') {
                            Some(Value::Object(m))
                        } else {
                            None
                        };
                    }
                }
            }
            q @ (b'"' | b'\'') => {
                self.i += 1;
                let mut out = String::new();
                while self.i < self.s.len() && self.s[self.i] != q {
                    let c = self.s[self.i];
                    if c == b'\\' && self.i + 1 < self.s.len() {
                        self.i += 1;
                        out.push(match self.s[self.i] {
                            b'n' => '\n',
                            b't' => '\t',
                            other => other as char,
                        });
                        self.i += 1;
                        continue;
                    }
                    // UTF-8 pass-through.
                    let st = self.i;
                    self.i += 1;
                    while self.i < self.s.len() && (self.s[self.i] & 0xC0) == 0x80 {
                        self.i += 1;
                    }
                    out.push_str(std::str::from_utf8(&self.s[st..self.i]).ok()?);
                }
                self.i += 1;
                Some(Value::String(out))
            }
            c if c.is_ascii_digit() || c == b'-' => {
                let st = self.i;
                self.i += 1;
                while self.i < self.s.len()
                    && (self.s[self.i].is_ascii_alphanumeric()
                        || matches!(self.s[self.i], b'.' | b'_'))
                {
                    self.i += 1;
                }
                let t = std::str::from_utf8(&self.s[st..self.i])
                    .ok()?
                    .replace('_', "");
                if let Ok(n) = t.parse::<i64>() {
                    return Some(Value::from(n));
                }
                t.parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                    .map(Value::Number)
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let st = self.i;
                while self.i < self.s.len()
                    && (self.s[self.i].is_ascii_alphanumeric() || self.s[self.i] == b'_')
                {
                    self.i += 1;
                }
                match std::str::from_utf8(&self.s[st..self.i]).ok()? {
                    "True" => Some(Value::Bool(true)),
                    "False" => Some(Value::Bool(false)),
                    "None" => Some(Value::Null),
                    n => self.names.get(n).cloned(),
                }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_constants() {
        let src = r#"
import os
ROOT = os.path.dirname(os.path.abspath(__file__))
QFN = "/srv/qfn"
DEFAULTS = (("max-context", 131072), ("kv", "auto"), ("draft", True), ("x", None),   # comment
            ("t", 1.0))
FIXED = ("host", "port")
CARDS = {
    "A": {"uuid": "GPU-a", "cfg": QFN + "/a.json", "ninfer": {"port": 8094}},
}
QUIET = ((6, 45), (7, 30))
def f():
    X = 1
"#;
        let c = constants(src);
        assert!(!c.contains_key("ROOT"));
        assert_eq!(
            c["DEFAULTS"],
            json!([
                ["max-context", 131072],
                ["kv", "auto"],
                ["draft", true],
                ["x", null],
                ["t", 1.0]
            ])
        );
        assert_eq!(c["CARDS"]["A"]["cfg"], "/srv/qfn/a.json");
        assert_eq!(c["QUIET"], json!([[6, 45], [7, 30]]));
        assert_eq!(c["FIXED"], json!(["host", "port"]));
        assert!(!c.contains_key("X"));
    }
}
