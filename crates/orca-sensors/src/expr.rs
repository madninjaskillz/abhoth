//! The expression language (SUPERVISOR.md 4.2). Small and total: no loops, no scripting.
//!
//! ```text
//! expr   := number | duration | source | "-" expr | expr (+|-|*|/) expr | "(" expr ")" | fn "(" args ")"
//! fn     := max | min | avg                    # strict: missing if any argument is missing
//!         | max_avail | min_avail | avg_avail  # ignore missing arguments; missing only if all are
//!         | first                              # the first argument that is not missing: fallbacks
//!         | ema(x, 4s)                         # time-constant smoothing
//!         | clamp(x, lo, hi)
//!         | scale(x, in_lo, in_hi, out_lo, out_hi)  # linear, clamped to [out_lo, out_hi]
//!         | gate(x, by, lo, hi)                # x * clamp((by - lo)/(hi - lo), 0, 1)
//!         | age(source)                        # seconds since the last reading (fresh or stale)
//! source := gpu.A.hbm | bmc."CPU1 Temp" | sensor.ctrl_A | gpu.*.hbm (pattern, variadic fns only)
//! duration := 500ms | 4s | 2m | 1h             # only as ema's second argument
//! ```
//!
//! **D-SV4: a missing result is never 0.** Evaluation returns `Err(Missing)` with the reason;
//! callers decide what missing means (for a fan: push nothing, or 100 % duty). Stale readings
//! are missing for every function except `age`. Non-finite results and division by zero are
//! missing too.
//!
//! Units are checked: `+ - max min avg first clamp` need compatible units (see
//! [`Unit::combine`]); `*` and `/` by a dimensionless value keep the unit. A mismatch is an
//! error in [`Expr::validate`] when units are known, and `Missing::UnitMismatch` at run time.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::id::{is_plain_char, parse_path};
use crate::reading::{Lookup, SensorCatalog, SensorSource, Unit};
use crate::{SensorId, TimestampMs};

/// A function of the language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Func {
    Max,
    Min,
    Avg,
    MaxAvail,
    MinAvail,
    AvgAvail,
    First,
    Ema,
    Clamp,
    Scale,
    Gate,
    Age,
}

impl Func {
    fn parse(name: &str) -> Option<Func> {
        Some(match name {
            "max" => Func::Max,
            "min" => Func::Min,
            "avg" => Func::Avg,
            "max_avail" => Func::MaxAvail,
            "min_avail" => Func::MinAvail,
            "avg_avail" => Func::AvgAvail,
            "first" => Func::First,
            "ema" => Func::Ema,
            "clamp" => Func::Clamp,
            "scale" => Func::Scale,
            "gate" => Func::Gate,
            "age" => Func::Age,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Func::Max => "max",
            Func::Min => "min",
            Func::Avg => "avg",
            Func::MaxAvail => "max_avail",
            Func::MinAvail => "min_avail",
            Func::AvgAvail => "avg_avail",
            Func::First => "first",
            Func::Ema => "ema",
            Func::Clamp => "clamp",
            Func::Scale => "scale",
            Func::Gate => "gate",
            Func::Age => "age",
        }
    }

    /// Variadic functions accept patterns and any number (≥ 1) of arguments.
    pub fn is_variadic(self) -> bool {
        matches!(
            self,
            Func::Max
                | Func::Min
                | Func::Avg
                | Func::MaxAvail
                | Func::MinAvail
                | Func::AvgAvail
                | Func::First
        )
    }

    /// Strict: missing if any argument is missing.
    pub fn is_strict(self) -> bool {
        matches!(self, Func::Max | Func::Min | Func::Avg)
    }

    fn arity(self) -> Option<usize> {
        match self {
            Func::Ema => Some(2),
            Func::Clamp => Some(3),
            Func::Scale => Some(5),
            Func::Gate => Some(4),
            Func::Age => Some(1),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
}

/// The syntax tree.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Num(f64),
    /// A duration in seconds (only valid as `ema`'s time constant).
    Dur(f64),
    Src(SensorId),
    Pattern(SensorId),
    Neg(Box<Node>),
    Bin(BinOp, Box<Node>, Box<Node>),
    /// The `usize` is the state slot for `ema` (unused for other functions).
    Call(Func, Vec<Node>, usize),
}

/// Why an expression has no value. **Missing is never 0** (D-SV4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "why", rename_all = "snake_case")]
pub enum Missing {
    /// The source has never reported, or does not have this metric.
    Absent {
        id: SensorId,
    },
    /// The source's last reading is older than its stale limit.
    Stale {
        id: SensorId,
        age_ms: i64,
    },
    /// A pattern matched no source (strict functions).
    NoMatch {
        pattern: SensorId,
    },
    /// Every argument of an `_avail` function or `first` was missing.
    AllMissing,
    /// Two values with incompatible units met.
    UnitMismatch {
        a: Unit,
        b: Unit,
    },
    DivByZero,
    NotFinite,
    /// A run-time argument made the function meaningless (e.g. `clamp` with lo > hi).
    Invalid {
        what: String,
    },
}

impl fmt::Display for Missing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Missing::Absent { id } => write!(f, "{id} is absent"),
            Missing::Stale { id, age_ms } => {
                write!(f, "{id} is stale ({:.1} s old)", *age_ms as f64 / 1000.0)
            }
            Missing::NoMatch { pattern } => write!(f, "{pattern} matches no source"),
            Missing::AllMissing => f.write_str("every argument is missing"),
            Missing::UnitMismatch { a, b } => write!(f, "unit mismatch: {a:?} vs {b:?}"),
            Missing::DivByZero => f.write_str("division by zero"),
            Missing::NotFinite => f.write_str("result is not a finite number"),
            Missing::Invalid { what } => write!(f, "invalid: {what}"),
        }
    }
}

/// A computed value with its unit and the timestamp of its oldest input (`None` for constants).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Value {
    pub v: f64,
    pub unit: Unit,
    pub ts: Option<TimestampMs>,
}

impl Value {
    pub fn constant(v: f64) -> Value {
        Value {
            v,
            unit: Unit::None,
            ts: None,
        }
    }
}

fn oldest(a: Option<TimestampMs>, b: Option<TimestampMs>) -> Option<TimestampMs> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

/// A parse error with the byte offset in the source text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExprError {
    #[error("at byte {at}: {msg}")]
    Parse { at: usize, msg: String },
    #[error("`{0}` takes {1}")]
    Arity(&'static str, &'static str),
    #[error("expression is too deeply nested")]
    TooDeep,
}

/// A validation finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub error: bool,
    pub msg: String,
}

/// The outcome of [`Expr::validate`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Validation {
    /// The result unit, if it could be inferred.
    pub unit: Option<Unit>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Validation {
    pub fn is_ok(&self) -> bool {
        !self.diagnostics.iter().any(|d| d.error)
    }
    pub fn errors(&self) -> impl Iterator<Item = &str> {
        self.diagnostics
            .iter()
            .filter(|d| d.error)
            .map(|d| d.msg.as_str())
    }
    pub fn warnings(&self) -> impl Iterator<Item = &str> {
        self.diagnostics
            .iter()
            .filter(|d| !d.error)
            .map(|d| d.msg.as_str())
    }
}

const MAX_DEPTH: usize = 64;
/// `ema` restarts from its input after a gap longer than this between input samples.
pub const EMA_RESTART_MS: i64 = 10_000;
const MAX_LEN: usize = 4096;

/// A parsed expression. Serialises as its source text.
#[derive(Clone, PartialEq)]
pub struct Expr {
    text: String,
    root: Node,
    ema_slots: usize,
}

/// Per-instance evaluation state (the `ema` accumulators). Keep one per place an expression is
/// evaluated; a fresh state starts each `ema` at its first input.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EvalState {
    ema: Vec<Option<(f64, TimestampMs)>>,
}

impl Expr {
    /// Parses and checks arity and argument kinds.
    pub fn parse(text: &str) -> Result<Expr, ExprError> {
        if text.len() > MAX_LEN {
            return Err(ExprError::Parse {
                at: MAX_LEN,
                msg: "expression is too long".into(),
            });
        }
        let mut p = Parser {
            s: text,
            i: 0,
            slots: 0,
            depth: 0,
        };
        let root = p.expr()?;
        p.ws();
        if p.i != text.len() {
            return Err(p.err("unexpected input"));
        }
        if matches!(root, Node::Dur(_)) {
            return Err(ExprError::Parse {
                at: 0,
                msg: "a duration is only valid as ema's time constant".into(),
            });
        }
        if matches!(root, Node::Pattern(_)) {
            return Err(ExprError::Parse {
                at: 0,
                msg: "a pattern needs a function such as max_avail(…)".into(),
            });
        }
        Ok(Expr {
            text: text.trim().to_string(),
            root,
            ema_slots: p.slots,
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn root(&self) -> &Node {
        &self.root
    }

    /// A state sized for this expression.
    pub fn new_state(&self) -> EvalState {
        EvalState {
            ema: vec![None; self.ema_slots],
        }
    }

    /// Every source and pattern the expression reads, in order of appearance, deduplicated.
    pub fn sources(&self) -> Vec<SensorId> {
        let mut out = Vec::new();
        collect_sources(&self.root, &mut out);
        out
    }

    /// Evaluates at `now`. `state` must come from [`Expr::new_state`] of this expression (a
    /// wrong-sized state is resized).
    pub fn eval(
        &self,
        src: &dyn SensorSource,
        now: TimestampMs,
        state: &mut EvalState,
    ) -> Result<Value, Missing> {
        if state.ema.len() != self.ema_slots {
            state.ema.resize(self.ema_slots, None);
        }
        let v = eval(&self.root, src, now, state)?;
        if v.v.is_finite() {
            Ok(v)
        } else {
            Err(Missing::NotFinite)
        }
    }

    /// Evaluates once with a throwaway state (an `ema` then just returns its input).
    pub fn eval_once(&self, src: &dyn SensorSource, now: TimestampMs) -> Result<Value, Missing> {
        self.eval(src, now, &mut self.new_state())
    }

    /// Checks sources against what a server has, and units where they are known.
    pub fn validate(&self, cat: &dyn SensorCatalog) -> Validation {
        let mut v = Validation::default();
        v.unit = check(&self.root, cat, &mut v.diagnostics, false);
        v
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl fmt::Debug for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Expr({:?})", self.text)
    }
}

impl std::str::FromStr for Expr {
    type Err = ExprError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Expr::parse(s)
    }
}

impl Serialize for Expr {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.text)
    }
}

impl<'de> Deserialize<'de> for Expr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Expr::parse(&s).map_err(serde::de::Error::custom)
    }
}

fn collect_sources(n: &Node, out: &mut Vec<SensorId>) {
    match n {
        Node::Src(id) | Node::Pattern(id) => {
            if !out.contains(id) {
                out.push(id.clone());
            }
        }
        Node::Neg(a) => collect_sources(a, out),
        Node::Bin(_, a, b) => {
            collect_sources(a, out);
            collect_sources(b, out);
        }
        Node::Call(_, args, _) => args.iter().for_each(|a| collect_sources(a, out)),
        Node::Num(_) | Node::Dur(_) => {}
    }
}

// ── parser ────────────────────────────────────────────────────────────────────────────────

struct Parser<'a> {
    s: &'a str,
    i: usize,
    slots: usize,
    depth: usize,
}

impl Parser<'_> {
    fn err(&self, msg: &str) -> ExprError {
        ExprError::Parse {
            at: self.i,
            msg: msg.into(),
        }
    }

    fn ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.i += c.len_utf8();
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<char> {
        self.s[self.i..].chars().next()
    }

    fn eat(&mut self, c: char) -> bool {
        self.ws();
        if self.peek() == Some(c) {
            self.i += c.len_utf8();
            true
        } else {
            false
        }
    }

    fn enter(&mut self) -> Result<(), ExprError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            Err(ExprError::TooDeep)
        } else {
            Ok(())
        }
    }

    fn expr(&mut self) -> Result<Node, ExprError> {
        self.enter()?;
        let mut lhs = self.term()?;
        loop {
            let op = if self.eat('+') {
                BinOp::Add
            } else if self.eat('-') {
                BinOp::Sub
            } else {
                break;
            };
            let rhs = self.term()?;
            lhs = Node::Bin(
                op,
                Box::new(no_dur(lhs, self)?),
                Box::new(no_dur(rhs, self)?),
            );
        }
        self.depth -= 1;
        Ok(lhs)
    }

    fn term(&mut self) -> Result<Node, ExprError> {
        let mut lhs = self.unary()?;
        loop {
            let op = if self.eat('*') {
                BinOp::Mul
            } else if self.eat('/') {
                BinOp::Div
            } else {
                break;
            };
            let rhs = self.unary()?;
            lhs = Node::Bin(
                op,
                Box::new(no_dur(lhs, self)?),
                Box::new(no_dur(rhs, self)?),
            );
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<Node, ExprError> {
        if self.eat('-') {
            self.enter()?;
            let inner = self.unary()?;
            self.depth -= 1;
            return Ok(Node::Neg(Box::new(no_dur(inner, self)?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Node, ExprError> {
        self.ws();
        let Some(c) = self.peek() else {
            return Err(self.err("expected a value"));
        };
        if c == '(' {
            self.i += 1;
            let e = self.expr()?;
            if !self.eat(')') {
                return Err(self.err("expected `)`"));
            }
            return Ok(e);
        }
        if c.is_ascii_digit()
            || (c == '.' && self.s[self.i + 1..].starts_with(|d: char| d.is_ascii_digit()))
        {
            return self.number();
        }
        if is_plain_char(c) || c == '"' {
            // A function name is a plain identifier followed by `(`.
            let start = self.i;
            let mut j = self.i;
            while j < self.s.len() && is_plain_char(self.s.as_bytes()[j] as char) {
                j += 1;
            }
            let name = &self.s[start..j];
            let rest = self.s[j..].trim_start();
            if !name.is_empty() && rest.starts_with('(') {
                let Some(func) = Func::parse(name) else {
                    return Err(self.err(&format!("unknown function `{name}`")));
                };
                self.i = j;
                self.eat('(');
                return self.call(func);
            }
            let (id, end) = parse_path(self.s, start).map_err(|e| ExprError::Parse {
                at: start,
                msg: e.to_string(),
            })?;
            self.i = end;
            return Ok(if id.is_pattern() {
                Node::Pattern(id)
            } else {
                Node::Src(id)
            });
        }
        Err(self.err("expected a number, a source or a function"))
    }

    fn number(&mut self) -> Result<Node, ExprError> {
        let start = self.i;
        let b = self.s.as_bytes();
        while self.i < b.len() && (b[self.i].is_ascii_digit() || b[self.i] == b'.') {
            self.i += 1;
        }
        let v: f64 = self.s[start..self.i]
            .parse()
            .map_err(|_| ExprError::Parse {
                at: start,
                msg: "bad number".into(),
            })?;
        let suf_start = self.i;
        while self.i < b.len() && b[self.i].is_ascii_alphabetic() {
            self.i += 1;
        }
        let secs = match &self.s[suf_start..self.i] {
            "" => return Ok(Node::Num(v)),
            "ms" => v / 1000.0,
            "s" => v,
            "m" => v * 60.0,
            "h" => v * 3600.0,
            _ => {
                return Err(ExprError::Parse {
                    at: suf_start,
                    msg: "unknown duration unit (ms, s, m, h)".into(),
                });
            }
        };
        Ok(Node::Dur(secs))
    }

    fn call(&mut self, func: Func) -> Result<Node, ExprError> {
        self.enter()?;
        let mut args = Vec::new();
        if !self.eat(')') {
            loop {
                args.push(self.expr()?);
                if self.eat(',') {
                    continue;
                }
                if self.eat(')') {
                    break;
                }
                return Err(self.err("expected `,` or `)`"));
            }
        }
        self.depth -= 1;
        match func.arity() {
            None if args.is_empty() => {
                return Err(ExprError::Arity(func.name(), "at least one argument"));
            }
            Some(n) if args.len() != n => {
                return Err(ExprError::Arity(
                    func.name(),
                    match n {
                        1 => "one argument",
                        2 => "two arguments",
                        3 => "three arguments",
                        4 => "four arguments",
                        _ => "five arguments",
                    },
                ));
            }
            _ => {}
        }
        for (k, a) in args.iter().enumerate() {
            let dur_ok = func == Func::Ema && k == 1;
            match a {
                Node::Dur(_) if !dur_ok => {
                    return Err(ExprError::Arity(
                        func.name(),
                        "a duration only as ema's second argument",
                    ));
                }
                Node::Pattern(_) if !func.is_variadic() => {
                    return Err(ExprError::Arity(
                        func.name(),
                        "no patterns (use max/min/avg/_avail/first)",
                    ));
                }
                _ => {}
            }
        }
        if func == Func::Ema {
            match args[1] {
                Node::Dur(s) if s > 0.0 => {}
                _ => {
                    return Err(ExprError::Arity(
                        "ema",
                        "a positive duration as its second argument, e.g. ema(x, 4s)",
                    ));
                }
            }
        }
        if func == Func::Age && !matches!(args[0], Node::Src(_)) {
            return Err(ExprError::Arity("age", "one source"));
        }
        let slot = if func == Func::Ema {
            self.slots += 1;
            self.slots - 1
        } else {
            0
        };
        Ok(Node::Call(func, args, slot))
    }
}

fn no_dur(n: Node, p: &Parser<'_>) -> Result<Node, ExprError> {
    match n {
        Node::Dur(_) => Err(p.err("a duration is only valid as ema's time constant")),
        Node::Pattern(_) => Err(p.err("a pattern needs a function such as max_avail(…)")),
        n => Ok(n),
    }
}

// ── evaluation ────────────────────────────────────────────────────────────────────────────

fn read(id: &SensorId, src: &dyn SensorSource, now: TimestampMs) -> Result<Value, Missing> {
    match src.lookup(id, now) {
        Lookup::Fresh(r) if r.value.is_finite() => Ok(Value {
            v: r.value,
            unit: r.unit,
            ts: Some(r.ts),
        }),
        Lookup::Fresh(_) => Err(Missing::NotFinite),
        Lookup::Stale { age_ms, .. } => Err(Missing::Stale {
            id: id.clone(),
            age_ms,
        }),
        Lookup::Absent => Err(Missing::Absent { id: id.clone() }),
    }
}

fn unit2(a: Unit, b: Unit) -> Result<Unit, Missing> {
    a.combine(b).ok_or(Missing::UnitMismatch { a, b })
}

fn finite(v: Value) -> Result<Value, Missing> {
    if v.v.is_finite() {
        Ok(v)
    } else {
        Err(Missing::NotFinite)
    }
}

fn eval(
    n: &Node,
    src: &dyn SensorSource,
    now: TimestampMs,
    st: &mut EvalState,
) -> Result<Value, Missing> {
    match n {
        Node::Num(v) => Ok(Value::constant(*v)),
        Node::Dur(s) => Ok(Value {
            v: *s,
            unit: Unit::Seconds,
            ts: None,
        }),
        Node::Src(id) => read(id, src, now),
        Node::Pattern(p) => Err(Missing::NoMatch { pattern: p.clone() }),
        Node::Neg(a) => {
            let a = eval(a, src, now, st)?;
            Ok(Value { v: -a.v, ..a })
        }
        Node::Bin(op, a, b) => {
            // Evaluate both sides even if one is missing, so every ema keeps its state current.
            let a = eval(a, src, now, st);
            let b = eval(b, src, now, st);
            let (a, b) = (a?, b?);
            let ts = oldest(a.ts, b.ts);
            let (v, unit) = match op {
                BinOp::Add => (a.v + b.v, unit2(a.unit, b.unit)?),
                BinOp::Sub => (a.v - b.v, unit2(a.unit, b.unit)?),
                BinOp::Mul => (
                    a.v * b.v,
                    match (a.unit, b.unit) {
                        (Unit::None, u) | (u, Unit::None) => u,
                        _ => Unit::Unknown,
                    },
                ),
                BinOp::Div => {
                    if b.v == 0.0 {
                        return Err(Missing::DivByZero);
                    }
                    (
                        a.v / b.v,
                        match (a.unit, b.unit) {
                            (u, Unit::None) => u,
                            (x, y) if x == y => Unit::None,
                            _ => Unit::Unknown,
                        },
                    )
                }
            };
            finite(Value { v, unit, ts })
        }
        Node::Call(f, args, slot) => call(*f, args, *slot, src, now, st),
    }
}

/// Evaluates variadic arguments, expanding patterns, in order.
fn variadic_args(
    args: &[Node],
    src: &dyn SensorSource,
    now: TimestampMs,
    st: &mut EvalState,
) -> Vec<Result<Value, Missing>> {
    let mut out = Vec::with_capacity(args.len());
    for a in args {
        match a {
            Node::Pattern(p) => {
                let ids = src.matching(p);
                if ids.is_empty() {
                    out.push(Err(Missing::NoMatch { pattern: p.clone() }));
                }
                for id in ids {
                    out.push(read(&id, src, now));
                }
            }
            n => out.push(eval(n, src, now, st)),
        }
    }
    out
}

fn fold(vals: &[Value], f: Func) -> Result<Value, Missing> {
    let mut unit = Unit::None;
    let mut ts = None;
    for v in vals {
        unit = unit2(unit, v.unit)?;
        ts = oldest(ts, v.ts);
    }
    let it = vals.iter().map(|v| v.v);
    let v = match f {
        Func::Max | Func::MaxAvail => it.fold(f64::NEG_INFINITY, f64::max),
        Func::Min | Func::MinAvail => it.fold(f64::INFINITY, f64::min),
        _ => it.sum::<f64>() / vals.len() as f64,
    };
    finite(Value { v, unit, ts })
}

fn call(
    f: Func,
    args: &[Node],
    slot: usize,
    src: &dyn SensorSource,
    now: TimestampMs,
    st: &mut EvalState,
) -> Result<Value, Missing> {
    match f {
        Func::Max | Func::Min | Func::Avg => {
            let vals = variadic_args(args, src, now, st)
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;
            fold(&vals, f)
        }
        Func::MaxAvail | Func::MinAvail | Func::AvgAvail => {
            let vals: Vec<Value> = variadic_args(args, src, now, st)
                .into_iter()
                .filter_map(Result::ok)
                .collect();
            if vals.is_empty() {
                return Err(Missing::AllMissing);
            }
            fold(&vals, f)
        }
        Func::First => {
            // All arguments are evaluated (ema state stays warm); the first present one wins.
            let vals = variadic_args(args, src, now, st);
            vals.into_iter()
                .find_map(Result::ok)
                .ok_or(Missing::AllMissing)
        }
        Func::Ema => {
            let Node::Dur(tau) = args[1] else {
                unreachable!("checked at parse")
            };
            let x = eval(&args[0], src, now, st);
            let cell = &mut st.ema[slot];
            match x {
                // Missing input: the result is missing too, but the history is kept, so a
                // short gap (a card's sample late or a sensor briefly stale) continues the
                // average when the input comes back, as the fan pusher does.
                Err(m) => Err(m),
                Ok(x) => {
                    // Time is the input's sample time, not the evaluation time: each sample is
                    // weighted over the interval since the previous sample, however often the
                    // expression is evaluated in between (pusher parity, SUPERVISOR.md 5.4).
                    let at = x.ts.unwrap_or(now);
                    let (v, at) = match *cell {
                        None => (x.v, at),
                        // A gap longer than EMA_RESTART_MS restarts the average (the pusher's
                        // STREAM_HANG): no old history leaks into later values.
                        Some((_, last)) if at - last > EMA_RESTART_MS => (x.v, at),
                        Some((prev, last)) => {
                            let dt = (at - last) as f64 / 1000.0;
                            if dt <= 0.0 {
                                (prev, last)
                            } else {
                                (prev + (1.0 - (-dt / tau).exp()) * (x.v - prev), at)
                            }
                        }
                    };
                    *cell = Some((v, at));
                    finite(Value { v, ..x })
                }
            }
        }
        Func::Clamp => {
            let x = eval(&args[0], src, now, st);
            let lo = eval(&args[1], src, now, st);
            let hi = eval(&args[2], src, now, st);
            let (x, lo, hi) = (x?, lo?, hi?);
            unit2(unit2(x.unit, lo.unit)?, hi.unit)?;
            if lo.v > hi.v {
                return Err(Missing::Invalid {
                    what: "clamp with lo > hi".into(),
                });
            }
            Ok(Value {
                v: x.v.clamp(lo.v, hi.v),
                unit: x.unit,
                ts: oldest(x.ts, oldest(lo.ts, hi.ts)),
            })
        }
        Func::Scale => {
            let vs: Vec<_> = args.iter().map(|a| eval(a, src, now, st)).collect();
            let vs = vs.into_iter().collect::<Result<Vec<_>, _>>()?;
            let (x, il, ih, ol, oh) = (vs[0], vs[1], vs[2], vs[3], vs[4]);
            unit2(unit2(x.unit, il.unit)?, ih.unit)?;
            let out_unit = unit2(ol.unit, oh.unit)?;
            let ts = vs.iter().fold(None, |t, v| oldest(t, v.ts));
            let frac = if ih.v == il.v {
                if x.v >= ih.v { 1.0 } else { 0.0 }
            } else {
                ((x.v - il.v) / (ih.v - il.v)).clamp(0.0, 1.0)
            };
            finite(Value {
                v: ol.v + frac * (oh.v - ol.v),
                unit: out_unit,
                ts,
            })
        }
        Func::Gate => {
            let vs: Vec<_> = args.iter().map(|a| eval(a, src, now, st)).collect();
            let vs = vs.into_iter().collect::<Result<Vec<_>, _>>()?;
            let (x, by, lo, hi) = (vs[0], vs[1], vs[2], vs[3]);
            unit2(unit2(by.unit, lo.unit)?, hi.unit)?;
            let ts = vs.iter().fold(None, |t, v| oldest(t, v.ts));
            let k = if hi.v == lo.v {
                if by.v >= hi.v { 1.0 } else { 0.0 }
            } else {
                ((by.v - lo.v) / (hi.v - lo.v)).clamp(0.0, 1.0)
            };
            finite(Value {
                v: x.v * k,
                unit: x.unit,
                ts,
            })
        }
        Func::Age => {
            let Node::Src(id) = &args[0] else {
                unreachable!("checked at parse")
            };
            match src.lookup(id, now) {
                Lookup::Fresh(r) | Lookup::Stale { reading: r, .. } => Ok(Value {
                    v: ((now - r.ts).max(0)) as f64 / 1000.0,
                    unit: Unit::Seconds,
                    ts: None,
                }),
                Lookup::Absent => Err(Missing::Absent { id: id.clone() }),
            }
        }
    }
}

// ── validation ────────────────────────────────────────────────────────────────────────────

fn diag(out: &mut Vec<Diagnostic>, error: bool, msg: String) {
    if !out.iter().any(|d| d.msg == msg) {
        out.push(Diagnostic { error, msg });
    }
}

fn combine_static(
    a: Option<Unit>,
    b: Option<Unit>,
    out: &mut Vec<Diagnostic>,
    what: &str,
) -> Option<Unit> {
    match (a, b) {
        (Some(x), Some(y)) => match x.combine(y) {
            Some(u) => Some(u),
            None => {
                diag(
                    out,
                    true,
                    format!("unit mismatch in {what}: {x:?} vs {y:?}"),
                );
                None
            }
        },
        (Some(Unit::None), None) | (None, Some(Unit::None)) => None,
        (x, None) => x,
        (None, y) => y,
    }
}

/// Returns the inferred unit (None if unknown). `strict` is true inside a strict context
/// (a strict function or an operator), for the often-absent warning.
fn check(
    n: &Node,
    cat: &dyn SensorCatalog,
    out: &mut Vec<Diagnostic>,
    strict: bool,
) -> Option<Unit> {
    match n {
        Node::Num(_) => Some(Unit::None),
        Node::Dur(_) => Some(Unit::Seconds),
        Node::Src(id) => {
            if !cat.exists(id) {
                diag(out, true, format!("{id} does not exist on this server"));
                return None;
            }
            if strict && cat.often_absent(id) {
                diag(
                    out,
                    false,
                    format!(
                        "{id} is often absent; a strict function over it is often missing (use first(…) or max_avail(…))"
                    ),
                );
            }
            cat.unit(id)
        }
        Node::Pattern(p) => {
            let ids = cat.expand(p);
            if ids.is_empty() {
                diag(out, true, format!("{p} matches no source on this server"));
                return None;
            }
            let mut u = Some(Unit::None);
            for id in ids {
                if strict && cat.often_absent(&id) {
                    diag(
                        out,
                        false,
                        format!(
                            "{id} (from {p}) is often absent; a strict function over it is often missing"
                        ),
                    );
                }
                u = combine_static(u, cat.unit(&id), out, &p.to_string());
            }
            u
        }
        Node::Neg(a) => check(a, cat, out, strict),
        Node::Bin(op, a, b) => {
            let ua = check(a, cat, out, true);
            let ub = check(b, cat, out, true);
            match op {
                BinOp::Add | BinOp::Sub => combine_static(ua, ub, out, "+/-"),
                BinOp::Mul => match (ua, ub) {
                    (Some(Unit::None), u) | (u, Some(Unit::None)) => u,
                    _ => Some(Unit::Unknown),
                },
                BinOp::Div => match (ua, ub) {
                    (u, Some(Unit::None)) => u,
                    (Some(x), Some(y)) if x == y => Some(Unit::None),
                    _ => Some(Unit::Unknown),
                },
            }
        }
        Node::Call(f, args, _) => {
            let strict_args = !matches!(
                f,
                Func::MaxAvail | Func::MinAvail | Func::AvgAvail | Func::First | Func::Age
            );
            let units: Vec<Option<Unit>> = args
                .iter()
                .map(|a| check(a, cat, out, strict_args))
                .collect();
            match f {
                Func::Max
                | Func::Min
                | Func::Avg
                | Func::MaxAvail
                | Func::MinAvail
                | Func::AvgAvail
                | Func::First => {
                    // Not try_fold: `None` (unknown) must not stop the check of later arguments.
                    let mut acc = Some(Unit::None);
                    for u in units {
                        acc = combine_static(acc, u, out, f.name());
                    }
                    acc
                }
                Func::Ema => units[0],
                Func::Clamp => {
                    let u = combine_static(units[0], units[1], out, "clamp");
                    combine_static(u, units[2], out, "clamp");
                    units[0]
                }
                Func::Scale => {
                    let u = combine_static(units[0], units[1], out, "scale input");
                    combine_static(u, units[2], out, "scale input");
                    combine_static(units[3], units[4], out, "scale output")
                }
                Func::Gate => {
                    let u = combine_static(units[1], units[2], out, "gate");
                    combine_static(u, units[3], out, "gate");
                    units[0]
                }
                Func::Age => Some(Unit::Seconds),
            }
        }
    }
}

#[cfg(test)]
mod tests;
