use super::*;
use crate::reading::{Reading, SensorStore};
use crate::{Namespace, SensorId};

fn id(s: &str) -> SensorId {
    SensorId::parse(s).unwrap()
}

/// Cards A (hbm + core), D (core only, like the M4000), all fresh at t = 10 s.
fn store() -> SensorStore {
    let mut s = SensorStore::new();
    let t = 10_000;
    s.put(id("gpu.A.core"), Reading::new(60.0, Unit::Celsius, t, "t"));
    s.put(id("gpu.A.hbm"), Reading::new(70.0, Unit::Celsius, t, "t"));
    s.put(
        id("gpu.A.power_pct"),
        Reading::new(80.0, Unit::Percent, t, "t"),
    );
    s.put(id("gpu.B.hbm"), Reading::new(65.0, Unit::Celsius, t, "t"));
    s.put(id("gpu.D.core"), Reading::new(50.0, Unit::Celsius, t, "t"));
    s.put(
        id("gpu.A.power_w"),
        Reading::new(200.0, Unit::Watts, t, "t"),
    );
    s
}

fn ev(e: &str, s: &SensorStore, now: i64) -> Result<f64, Missing> {
    Expr::parse(e).unwrap().eval_once(s, now).map(|v| v.v)
}

#[test]
fn design_examples() {
    let s = store();
    let now = 10_500;
    // The M4000 rule: no hbm, so core.
    assert_eq!(ev("first(gpu.D.hbm, gpu.D.core)", &s, now), Ok(50.0));
    // One fan for two cards, strict.
    assert_eq!(ev("max(gpu.A.hbm, gpu.B.hbm)", &s, now), Ok(70.0));
    assert!(matches!(
        ev("max(gpu.A.hbm, gpu.D.hbm)", &s, now),
        Err(Missing::Absent { .. })
    ));
    // Fallback when one value is missing.
    assert_eq!(
        ev("max_avail(gpu.D.hbm, gpu.D.core - 5)", &s, now),
        Ok(45.0)
    );
    // Today's load gate: 80 % load, temp 60 in [52, 64] → 80 * 8/12.
    let g = ev("gate(gpu.A.power_pct, gpu.A.core, 52, 64)", &s, now).unwrap();
    assert!((g - 80.0 * 8.0 / 12.0).abs() < 1e-9);
    // Pattern.
    assert_eq!(ev("max(gpu.*.hbm)", &s, now), Ok(70.0));
    assert_eq!(ev("min_avail(gpu.*.hbm, gpu.D.core)", &s, now), Ok(50.0));
    assert!(matches!(
        ev("max(gpu.*.hotspot)", &s, now),
        Err(Missing::NoMatch { .. })
    ));
    assert_eq!(ev("avg(gpu.A.hbm, gpu.B.hbm)", &s, now), Ok(67.5));
}

#[test]
fn missing_is_never_zero() {
    let s = store();
    // Everything over an absent source is missing, never 0 or a partial result.
    for e in [
        "gpu.D.hbm",
        "gpu.D.hbm + 1",
        "gpu.D.hbm * 0",
        "-gpu.D.hbm",
        "max(gpu.D.hbm, 0)",
        "avg(gpu.D.hbm, gpu.A.core)",
        "clamp(gpu.D.hbm, 0, 100)",
        "scale(gpu.D.hbm, 40, 80, 30, 100)",
        "gate(gpu.A.power_pct, gpu.D.hbm, 52, 64)",
        "ema(gpu.D.hbm, 4s)",
        "max_avail(gpu.D.hbm, gpu.C.hbm)",
        "first(gpu.D.hbm, gpu.C.core)",
        "age(gpu.D.hbm)",
    ] {
        assert!(ev(e, &s, 10_500).is_err(), "{e} should be missing");
    }
}

#[test]
fn stale_is_missing_but_age_sees_it() {
    let s = store();
    let now = 20_000; // 10 s after the readings; gpu stale limit is 3 s
    assert!(matches!(
        ev("gpu.A.core", &s, now),
        Err(Missing::Stale { age_ms: 10_000, .. })
    ));
    assert_eq!(ev("age(gpu.A.core)", &s, now), Ok(10.0));
    assert_eq!(ev("first(gpu.A.hbm, 99)", &s, now), Ok(99.0));
}

#[test]
fn units() {
    let s = store();
    assert!(matches!(
        ev("gpu.A.core + gpu.A.power_w", &s, 10_500),
        Err(Missing::UnitMismatch { .. })
    ));
    assert!(matches!(
        ev("max(gpu.A.core, gpu.A.power_w)", &s, 10_500),
        Err(Missing::UnitMismatch { .. })
    ));
    let e = Expr::parse("gpu.A.core - 5").unwrap();
    assert_eq!(e.eval_once(&s, 10_500).unwrap().unit, Unit::Celsius);
    let e = Expr::parse("gpu.A.power_w / gpu.A.power_w").unwrap();
    assert_eq!(e.eval_once(&s, 10_500).unwrap().unit, Unit::None);
    let e = Expr::parse("scale(gpu.A.core, 45, 78, 30, 100)").unwrap();
    let v = e.eval_once(&s, 10_500).unwrap();
    assert_eq!(v.unit, Unit::None);
    assert!((v.v - (30.0 + (15.0 / 33.0) * 70.0)).abs() < 1e-9);
    assert_eq!(ev("age(gpu.A.core)", &s, 10_500), Ok(0.5));
    // The static check finds the same mismatch.
    let val = Expr::parse("gpu.A.core + gpu.A.power_w")
        .unwrap()
        .validate(&s);
    assert!(!val.is_ok());
}

#[test]
fn div_by_zero_and_invalid() {
    let s = store();
    assert_eq!(ev("gpu.A.core / 0", &s, 10_500), Err(Missing::DivByZero));
    assert!(matches!(
        ev("clamp(gpu.A.core, 10, 0)", &s, 10_500),
        Err(Missing::Invalid { .. })
    ));
}

#[test]
fn ema_tracks_with_time_constant() {
    let mut s = SensorStore::new();
    let e = Expr::parse("ema(gpu.A.core, 4s)").unwrap();
    let mut st = e.new_state();
    let core = id("gpu.A.core");
    s.put(core.clone(), Reading::new(40.0, Unit::Celsius, 0, "t"));
    assert_eq!(e.eval(&s, 0, &mut st).unwrap().v, 40.0);
    s.put(core.clone(), Reading::new(80.0, Unit::Celsius, 4_000, "t"));
    let v = e.eval(&s, 4_000, &mut st).unwrap().v;
    let want = 40.0 + (1.0 - (-1.0f64).exp()) * 40.0;
    assert!((v - want).abs() < 1e-9, "{v} vs {want}");
    // Same instant again: unchanged.
    assert!((e.eval(&s, 4_000, &mut st).unwrap().v - want).abs() < 1e-9);
    // A gap resets the average.
    s.remove(&core);
    assert!(e.eval(&s, 4_500, &mut st).is_err());
    s.put(core.clone(), Reading::new(30.0, Unit::Celsius, 5_000, "t"));
    assert_eq!(e.eval(&s, 5_000, &mut st).unwrap().v, 30.0);
}

#[test]
fn parse_errors() {
    for bad in [
        "",
        "max()",
        "ema(gpu.A.core)",
        "ema(gpu.A.core, 4)",
        "ema(gpu.A.core, 0s)",
        "clamp(gpu.A.core, 1)",
        "4s",
        "gpu.*.hbm",
        "gpu.*.hbm + 1",
        "clamp(gpu.*.hbm, 0, 1)",
        "age(1)",
        "foo(1)",
        "gpu.A.core +",
        "(gpu.A.core",
        "nope.A",
        "1 2",
        "max(1, 4s)",
        "4x",
    ] {
        assert!(Expr::parse(bad).is_err(), "`{bad}` should not parse");
    }
    let deep = format!("{}1{}", "(".repeat(200), ")".repeat(200));
    assert!(Expr::parse(&deep).is_err());
}

#[test]
fn sources_and_validation() {
    let s = store();
    let e =
        Expr::parse("ema(first(gpu.A.hbm, gpu.A.core), 4s) + max(gpu.*.hbm) - gpu.A.core").unwrap();
    assert_eq!(
        e.sources(),
        vec![id("gpu.A.hbm"), id("gpu.A.core"), id("gpu.*.hbm")]
    );
    assert!(e.validate(&s).is_ok());
    assert_eq!(e.validate(&s).unit, Some(Unit::Celsius));
    let v = Expr::parse("max(gpu.Z.core, 1)").unwrap().validate(&s);
    assert_eq!(v.errors().count(), 1);
    // Often-absent warning only in strict context.
    struct Cat(SensorStore);
    impl SensorCatalog for Cat {
        fn exists(&self, id: &SensorId) -> bool {
            self.0.exists(id) || id.leaf() == "hbm"
        }
        fn unit(&self, id: &SensorId) -> Option<Unit> {
            self.0.unit(id)
        }
        fn often_absent(&self, id: &SensorId) -> bool {
            id.leaf() == "hbm"
        }
        fn expand(&self, p: &SensorId) -> Vec<SensorId> {
            self.0.expand(p)
        }
    }
    let c = Cat(store());
    assert_eq!(
        Expr::parse("max(gpu.D.hbm, gpu.A.core)")
            .unwrap()
            .validate(&c)
            .warnings()
            .count(),
        1
    );
    assert_eq!(
        Expr::parse("first(gpu.D.hbm, gpu.D.core)")
            .unwrap()
            .validate(&c)
            .warnings()
            .count(),
        0
    );
}

#[test]
fn serde_roundtrip() {
    let e = Expr::parse(" max_avail(gpu.A.hbm, bmc.\"CPU1 Temp\") ").unwrap();
    let j = serde_json::to_string(&e).unwrap();
    assert_eq!(j, "\"max_avail(gpu.A.hbm, bmc.\\\"CPU1 Temp\\\")\"");
    let back: Expr = serde_json::from_str(&j).unwrap();
    assert_eq!(back, e);
    let m = Missing::Stale {
        id: id("gpu.A.core"),
        age_ms: 5,
    };
    let j = serde_json::to_string(&m).unwrap();
    assert_eq!(serde_json::from_str::<Missing>(&j).unwrap(), m);
}

// ── property tests (deterministic PRNG, no extra dependency) ──────────────────────────────

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const SRCS: [&str; 6] = [
    "gpu.A.core",
    "gpu.A.hbm",
    "gpu.B.hbm",
    "gpu.D.core",
    "gpu.D.hbm",
    "gpu.C.core",
];

fn gen_expr(r: &mut Rng, depth: u32) -> String {
    if depth == 0 || r.below(3) == 0 {
        return match r.below(3) {
            0 => format!("{}", r.below(100)),
            _ => SRCS[r.below(SRCS.len() as u64) as usize].to_string(),
        };
    }
    let a = gen_expr(r, depth - 1);
    let b = gen_expr(r, depth - 1);
    match r.below(12) {
        0 => format!("({a} + {b})"),
        1 => format!("({a} - {b})"),
        2 => format!("({a} * 2)"),
        3 => format!("max({a}, {b})"),
        4 => format!("min({a}, {b})"),
        5 => format!("avg({a}, {b})"),
        6 => format!("max_avail({a}, {b})"),
        7 => format!("avg_avail({a}, {b})"),
        8 => format!("first({a}, {b})"),
        9 => format!("ema({a}, 4s)"),
        10 => format!("clamp({a}, 0, 100)"),
        _ => format!("gate({a}, {b}, 52, 64)"),
    }
}

/// A store where each source is present with some probability, all in °C.
fn rand_store(r: &mut Rng) -> SensorStore {
    let mut s = SensorStore::new();
    for src in SRCS {
        match r.below(4) {
            0 => {}
            1 => s.put(
                id(src),
                Reading::new(r.below(90) as f64, Unit::Celsius, 0, "t"),
            ), // will be stale at now
            _ => s.put(
                id(src),
                Reading::new(r.below(90) as f64, Unit::Celsius, 9_000, "t"),
            ),
        }
    }
    s
}

/// Does `e` mention a source that is missing in `s` at `now`, outside any `_avail`/`first`?
fn strictly_needs_missing(n: &Node, s: &SensorStore, now: i64) -> bool {
    match n {
        Node::Src(i) => s.lookup(i, now).fresh().is_none(),
        Node::Neg(a) => strictly_needs_missing(a, s, now),
        Node::Bin(_, a, b) => {
            strictly_needs_missing(a, s, now) || strictly_needs_missing(b, s, now)
        }
        Node::Call(f, args, _)
            if f.is_strict() || matches!(f, Func::Ema | Func::Clamp | Func::Gate | Func::Scale) =>
        {
            args.iter().any(|a| strictly_needs_missing(a, s, now))
        }
        _ => false,
    }
}

#[test]
fn property_strict_missing_propagates_and_results_are_finite() {
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let now = 10_000;
    for _ in 0..5_000 {
        let text = gen_expr(&mut r, 4);
        let e = Expr::parse(&text).unwrap_or_else(|err| panic!("{text}: {err}"));
        let s = rand_store(&mut r);
        let got = e.eval_once(&s, now);
        if strictly_needs_missing(e.root(), &s, now) {
            assert!(got.is_err(), "{text} should be missing, got {got:?}");
        }
        if let Ok(v) = got {
            assert!(v.v.is_finite(), "{text} → {v:?}");
            // Every value is derived from fresh inputs only.
            if let Some(ts) = v.ts {
                assert!(now - ts <= 3_000, "{text} used a stale input");
            }
        }
        // Text round-trips through serde.
        let back: Expr = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back.eval_once(&s, now), got);
    }
}

#[test]
fn property_avail_bounds() {
    let mut r = Rng(42);
    for _ in 0..2_000 {
        let s = rand_store(&mut r);
        let now = 10_000;
        let present: Vec<f64> = SRCS
            .iter()
            .filter_map(|x| s.lookup(&id(x), now).fresh().map(|r| r.value))
            .collect();
        let args = SRCS.join(", ");
        let mx = ev(&format!("max_avail({args})"), &s, now);
        let mn = ev(&format!("min_avail({args})"), &s, now);
        let av = ev(&format!("avg_avail({args})"), &s, now);
        let fs = ev(&format!("first({args})"), &s, now);
        if present.is_empty() {
            for v in [mx, mn, av, fs] {
                assert_eq!(v, Err(Missing::AllMissing));
            }
        } else {
            let (mx, mn, av) = (mx.unwrap(), mn.unwrap(), av.unwrap());
            assert!(mn <= av + 1e-9 && av <= mx + 1e-9);
            assert_eq!(mx, present.iter().copied().fold(f64::MIN, f64::max));
            assert_eq!(fs.unwrap(), present[0]);
        }
        let strict = ev(&format!("max({args})"), &s, now);
        assert_eq!(strict.is_ok(), present.len() == SRCS.len());
    }
}

#[test]
fn property_ema_stays_between_inputs() {
    let mut r = Rng(7);
    let e = Expr::parse("ema(gpu.A.core, 4s)").unwrap();
    let mut st = e.new_state();
    let mut s = SensorStore::new();
    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
    for k in 0..1_000i64 {
        let x = r.below(100) as f64;
        lo = lo.min(x);
        hi = hi.max(x);
        let t = k * 500;
        s.put(
            SensorId::of(Namespace::Gpu, &["A", "core"]),
            Reading::new(x, Unit::Celsius, t, "t"),
        );
        let v = e.eval(&s, t, &mut st).unwrap().v;
        assert!(v >= lo - 1e-9 && v <= hi + 1e-9);
    }
}
