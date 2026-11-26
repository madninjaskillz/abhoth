//! The hostd protocol (SUPERVISOR.md 2.4): framed JSON over the hostd SSH channel, relayed by
//! `hostctl serve` to `/run/abhoth/hostd.sock`. **Typed messages only; no command text.**
//!
//! Framing: one JSON object per line (NDJSON), at most [`MAX_FRAME`] bytes. Every frame is an
//! [`Envelope`]: an optional `id` plus the message, tagged by `type`:
//!
//! ```text
//! → {"id":1,"type":"hello","proto":1,"version":"0.16.6","client":"supervisor"}
//! ← {"id":1,"type":"hello","proto":1,"version":"0.16.6","limits_hash":"…","policy_rev":12}
//! → {"id":2,"type":"telemetry.subscribe","gpu_ms":500}
//! ← {"id":2,"type":"ack"}
//! ← {"type":"telemetry","ts":…,"gpus":[…]}          (stream frames carry no id)
//! → {"id":3,"type":"policy.put","rev":13,"cards":{…},"sensors":{…},"fans":{…},"power":{…}}
//! ← {"id":3,"type":"policy.refused","rev":13,"reasons":["fans.duct0: min_duty 20 < limit 30"]}
//! ```
//!
//! A reply carries the request's `id`. Unknown message types are answered with
//! `error {code: "bad_request"}` and the connection stays open, so a newer supervisor can
//! probe an older hostd. `proto` is bumped on incompatible changes only.

use std::collections::BTreeMap;

use orca_sensors::samples::{BmcSample, FanSample, GpuSample, HostSample, HwmonSample};
use orca_sensors::{Expr, TimestampMs};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The protocol version. Bumped on incompatible changes only.
pub const PROTOCOL_VERSION: u32 = 1;

/// The largest frame either side accepts (bytes, without the newline).
pub const MAX_FRAME: usize = 4 * 1024 * 1024;

/// The longest TTL any hold or override may have (SUPERVISOR.md 2.3: overrides expire).
pub const MAX_TTL_S: u64 = 3600;

/// One frame: an optional correlation id and a message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope<T> {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(flatten)]
    pub msg: T,
}

impl<T> Envelope<T> {
    pub fn new(id: Option<u64>, msg: T) -> Envelope<T> {
        Envelope { id, msg }
    }
}

// ── supervisor → hostd ────────────────────────────────────────────────────────────────────

/// A card label (`A`) or every managed card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    All,
    Card(String),
}

/// Telemetry rates (ms). hostd clamps them to its own minimums.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rates {
    #[serde(default = "Rates::d_gpu")]
    pub gpu_ms: u64,
    #[serde(default = "Rates::d_host")]
    pub host_ms: u64,
    #[serde(default = "Rates::d_bmc")]
    pub bmc_ms: u64,
    #[serde(default = "Rates::d_hwmon")]
    pub hwmon_ms: u64,
    #[serde(default = "Rates::d_facts")]
    pub facts_ms: u64,
}

impl Rates {
    fn d_gpu() -> u64 {
        500
    }
    fn d_host() -> u64 {
        5_000
    }
    fn d_bmc() -> u64 {
        10_000
    }
    fn d_hwmon() -> u64 {
        2_000
    }
    fn d_facts() -> u64 {
        5_000
    }
    /// The smallest rates hostd accepts; faster requests are slowed to these.
    pub const MIN: Rates = Rates {
        gpu_ms: 250,
        host_ms: 1_000,
        bmc_ms: 5_000,
        hwmon_ms: 1_000,
        facts_ms: 5_000,
    };

    pub fn clamped(&self) -> Rates {
        Rates {
            gpu_ms: self.gpu_ms.max(Rates::MIN.gpu_ms),
            host_ms: self.host_ms.max(Rates::MIN.host_ms),
            bmc_ms: self.bmc_ms.max(Rates::MIN.bmc_ms),
            hwmon_ms: self.hwmon_ms.max(Rates::MIN.hwmon_ms),
            facts_ms: self.facts_ms.max(Rates::MIN.facts_ms),
        }
    }
}

impl Default for Rates {
    fn default() -> Rates {
        Rates {
            gpu_ms: Rates::d_gpu(),
            host_ms: Rates::d_host(),
            bmc_ms: Rates::d_bmc(),
            hwmon_ms: Rates::d_hwmon(),
            facts_ms: Rates::d_facts(),
        }
    }
}

/// How a config card label maps to a GPU on this host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardBinding {
    pub uuid: String,
    /// `bb:dd.f`, for humans and as a cross-check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bus: Option<String>,
}

/// `policy.put`: everything hostd enforces, compiled from config. Validated against the
/// limits file as a whole; refused in full on any violation (the old policy stays).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyDoc {
    /// Monotonic revision; hostd refuses a rev lower than the one in force.
    pub rev: u64,
    /// Card label → identity. Sensors use labels (`gpu.A.hbm`).
    #[serde(default)]
    pub cards: BTreeMap<String, CardBinding>,
    /// `[sensors.<name>]` expressions, evaluated in hostd into `sensor.<name>`.
    #[serde(default)]
    pub sensors: BTreeMap<String, Expr>,
    #[serde(default)]
    pub fans: orca_fans::FansPolicy,
    /// `[cards.X.power]` by card label: persistence, power limit, app clocks, `serving`,
    /// `idle` (the design's `clocks` and `idle` parts, in orca-gpuctl's one type).
    #[serde(default)]
    pub power: BTreeMap<String, orca_gpuctl::CardPowerConfig>,
}

/// `override`: a temporary change with a TTL (≤ [`MAX_TTL_S`]). Floors and caps still apply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Override {
    /// Fixed duty for one fan (still at least `min_duty`, still 100 % at `max_temp`).
    FanDuty { fan: String, duty: f64, ttl_s: u64 },
    /// Cap a card's SM clock (refused on pinned cards without `allow_cap`).
    ClockCap { card: String, mhz: u32, ttl_s: u64 },
    /// Ends an override early.
    Clear {
        fan: Option<String>,
        card: Option<String>,
    },
}

/// Messages from the supervisor to hostd.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToHostd {
    #[serde(rename = "hello")]
    Hello {
        proto: u32,
        version: String,
        client: String,
    },
    #[serde(rename = "telemetry.subscribe")]
    TelemetrySubscribe(Rates),
    #[serde(rename = "telemetry.unsubscribe")]
    TelemetryUnsubscribe,
    #[serde(rename = "policy.put")]
    PolicyPut(PolicyDoc),
    /// The router is about to proxy to this card: leave idle clocks now.
    #[serde(rename = "wake")]
    Wake { card: String },
    /// No idle clock-down until released or expired.
    #[serde(rename = "hold")]
    Hold {
        target: Target,
        ttl_s: u64,
        reason: String,
    },
    #[serde(rename = "release")]
    Release { target: Target },
    #[serde(rename = "override")]
    Override(Override),
    /// Re-check and fix clocks, power limit and persistence now (after engine `Ready`).
    #[serde(rename = "assert")]
    Assert { card: String },
    #[serde(rename = "status")]
    Status,
}

// ── hostd → supervisor ────────────────────────────────────────────────────────────────────

/// Error codes in `error` replies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Unparseable frame or unknown message type.
    BadRequest,
    /// `hello` with an incompatible protocol version.
    Version,
    /// Valid, but the limits file forbids it (ttl too long, card not allowed, …).
    Forbidden,
    /// Unknown card, fan or target.
    NotFound,
    /// No policy yet, or the subsystem is not running.
    Unavailable,
    Internal,
}

/// Result of `assert`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssertReport {
    pub card: String,
    pub ok: bool,
    /// What was found wrong and fixed (or not), one line each.
    #[serde(default)]
    pub actions: Vec<String>,
    /// For the job step, e.g. "A pinned 1597, PL 350 W ✓".
    pub summary: String,
}

/// Health of one hostd loop or source.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LoopHealth {
    pub name: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok: Option<TimestampMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// `status` reply.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct HostdStatus {
    pub version: String,
    pub started: TimestampMs,
    pub limits_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_rev: Option<u64>,
    #[serde(default)]
    pub loops: Vec<LoopHealth>,
    /// Active holds and overrides, one line each with expiry.
    #[serde(default)]
    pub holds: Vec<String>,
    #[serde(default)]
    pub overrides: Vec<String>,
}

/// One GPU process (for "which container uses this card").
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct GpuProcess {
    pub pid: u32,
    pub uuid: String,
    pub used_mib: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// The process name only (no arguments: they can hold secrets).
    pub comm: String,
}

/// Slow-changing host facts (ports `hoststat.py`). Only safe hints: no command lines.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct HostFacts {
    pub ts: TimestampMs,
    #[serde(default)]
    pub gpu_processes: Vec<GpuProcess>,
    /// `name → health` (`ONLINE`, `DEGRADED`, …) with alloc/size bytes.
    #[serde(default)]
    pub zpools: Vec<Zpool>,
    /// Recent OOM kills (process name and time).
    #[serde(default)]
    pub oom_kills: Vec<OomKill>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Zpool {
    pub name: String,
    pub health: String,
    pub size: u64,
    pub alloc: u64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct OomKill {
    pub ts: TimestampMs,
    pub comm: String,
}

/// One telemetry frame. Sections are present only when sampled in this tick.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TelemetryBatch {
    pub ts: TimestampMs,
    /// Card label (from the policy) → sample; cards without a label are keyed by UUID.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub gpus: BTreeMap<String, GpuSample>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostSample>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bmc: Option<BmcSample>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hwmon: Option<HwmonSample>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fans: Vec<FanSample>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<HostFacts>,
    /// Virtual sensors hostd evaluated (`sensor.<name>`), so the supervisor shows the same
    /// control temperatures the fans use.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sensors: BTreeMap<String, f64>,
}

/// A discrete hostd event (fan failsafe, clock mismatch fixed, policy applied, NVML re-init).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostdEvent {
    pub ts: TimestampMs,
    /// Dotted kind, e.g. `fan.failsafe`, `clock.mismatch`, `policy.applied`, `gpu.reinit`.
    pub kind: String,
    #[serde(default)]
    pub severity: orca_sensors::feed::Severity,
    pub text: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
}

/// Messages from hostd to the supervisor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum FromHostd {
    #[serde(rename = "hello")]
    Hello {
        proto: u32,
        version: String,
        /// sha256 of the limits file, so the supervisor can show which limits are in force.
        limits_hash: String,
        /// The policy revision on disk, if any.
        policy_rev: Option<u64>,
    },
    #[serde(rename = "ack")]
    Ack,
    #[serde(rename = "policy.applied")]
    PolicyApplied { rev: u64 },
    #[serde(rename = "policy.refused")]
    PolicyRefused { rev: u64, reasons: Vec<String> },
    #[serde(rename = "assert.result")]
    AssertResult(AssertReport),
    #[serde(rename = "status")]
    Status(HostdStatus),
    #[serde(rename = "error")]
    Error { code: ErrorCode, message: String },
    #[serde(rename = "telemetry")]
    Telemetry(Box<TelemetryBatch>),
    #[serde(rename = "event")]
    Event(HostdEvent),
}

// ── framing ───────────────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame longer than {MAX_FRAME} bytes")]
    TooLong,
    /// The line was read but did not decode. `id` is recovered when possible so the reply can
    /// carry it.
    #[error("bad frame: {err}")]
    Decode {
        id: Option<u64>,
        err: serde_json::Error,
    },
}

/// Reads one frame. `Ok(None)` at a clean end of stream. Blank lines are skipped.
pub async fn read_frame<R, T>(
    r: &mut R,
    buf: &mut Vec<u8>,
) -> Result<Option<Envelope<T>>, FrameError>
where
    R: AsyncBufRead + Unpin,
    T: DeserializeOwned,
{
    loop {
        buf.clear();
        let n = (&mut *r)
            .take(MAX_FRAME as u64 + 1)
            .read_until(b'\n', buf)
            .await?;
        if n == 0 {
            return Ok(None);
        }
        if buf.last() != Some(&b'\n') && buf.len() > MAX_FRAME {
            return Err(FrameError::TooLong);
        }
        let line = buf.trim_ascii();
        if line.is_empty() {
            continue;
        }
        return match serde_json::from_slice::<Envelope<T>>(line) {
            Ok(env) => Ok(Some(env)),
            Err(err) => {
                let id = serde_json::from_slice::<serde_json::Value>(line)
                    .ok()
                    .and_then(|v| v.get("id").and_then(|i| i.as_u64()));
                Err(FrameError::Decode { id, err })
            }
        };
    }
}

/// Writes one frame and flushes.
pub async fn write_frame<W, T>(w: &mut W, env: &Envelope<T>) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut line = serde_json::to_vec(env).map_err(std::io::Error::other)?;
    if line.len() > MAX_FRAME {
        return Err(std::io::Error::other("frame too long"));
    }
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_sensors::Unit;
    use orca_sensors::samples::{MemSample, SdrEntry};
    use tokio::io::BufReader;

    fn all_to_hostd() -> Vec<ToHostd> {
        let mut sensors = BTreeMap::new();
        sensors.insert(
            "ctrl_A".to_string(),
            Expr::parse("ema(first(gpu.A.hbm, gpu.A.core), 4s)").unwrap(),
        );
        let mut cards = BTreeMap::new();
        cards.insert(
            "A".to_string(),
            CardBinding {
                uuid: "GPU-aaaa".into(),
                bus: Some("21:00.0".into()),
            },
        );
        let fans = orca_fans::FansPolicy::compile(
            &orca_fans::config::FansConfig::from_toml(
                r#"
[fan_backends.esp]
kind = "esphome"
url = "http://esp.invalid"
[fans.duct1]
backend = "esp"
channel = 1
temp = "sensor.ctrl_A"
load = "gpu.A.power_pct"
curve = [[60, 30], [82, 100]]
"#,
            )
            .unwrap(),
            &orca_fans::FanLimits::default(),
            &|_| true,
        )
        .unwrap();
        vec![
            ToHostd::Hello {
                proto: PROTOCOL_VERSION,
                version: "0.0.0".into(),
                client: "supervisor".into(),
            },
            ToHostd::TelemetrySubscribe(Rates::default()),
            ToHostd::TelemetryUnsubscribe,
            ToHostd::PolicyPut(PolicyDoc {
                rev: 7,
                cards,
                sensors,
                fans,
                power: BTreeMap::new(),
            }),
            ToHostd::Wake { card: "A".into() },
            ToHostd::Hold {
                target: Target::All,
                ttl_s: 60,
                reason: "switch".into(),
            },
            ToHostd::Release {
                target: Target::Card("B".into()),
            },
            ToHostd::Override(Override::FanDuty {
                fan: "duct0".into(),
                duty: 100.0,
                ttl_s: 600,
            }),
            ToHostd::Override(Override::ClockCap {
                card: "B".into(),
                mhz: 1200,
                ttl_s: 600,
            }),
            ToHostd::Override(Override::Clear {
                fan: Some("duct0".into()),
                card: None,
            }),
            ToHostd::Assert { card: "A".into() },
            ToHostd::Status,
        ]
    }

    fn all_from_hostd() -> Vec<FromHostd> {
        let mut gpus = BTreeMap::new();
        gpus.insert(
            "A".to_string(),
            GpuSample {
                uuid: "GPU-aaaa".into(),
                core_c: Some(40.0),
                mem_temp_c: None,
                ts: 1,
                ..Default::default()
            },
        );
        vec![
            FromHostd::Hello {
                proto: 1,
                version: "0.0.0".into(),
                limits_hash: "abc".into(),
                policy_rev: Some(3),
            },
            FromHostd::Ack,
            FromHostd::PolicyApplied { rev: 7 },
            FromHostd::PolicyRefused {
                rev: 8,
                reasons: vec!["fans.duct0: min_duty 20 < 30".into()],
            },
            FromHostd::AssertResult(AssertReport {
                card: "A".into(),
                ok: true,
                actions: vec![],
                summary: "A pinned 1597, PL 350 W ✓".into(),
            }),
            FromHostd::Status(HostdStatus {
                version: "x".into(),
                ..Default::default()
            }),
            FromHostd::Error {
                code: ErrorCode::Forbidden,
                message: "ttl > 3600".into(),
            },
            FromHostd::Telemetry(Box::new(TelemetryBatch {
                ts: 1,
                gpus,
                host: Some(HostSample {
                    mem: MemSample {
                        total: 10,
                        ..Default::default()
                    },
                    ..Default::default()
                }),
                bmc: Some(BmcSample {
                    ts: 1,
                    sdr: vec![SdrEntry {
                        name: "CPU1 Temp".into(),
                        value: Some(40.0),
                        unit: Unit::Celsius,
                        unit_text: "degrees C".into(),
                        status: "ok".into(),
                    }],
                    fan_mode: None,
                }),
                ..Default::default()
            })),
            FromHostd::Event(HostdEvent {
                ts: 1,
                kind: "fan.failsafe".into(),
                severity: Default::default(),
                text: "x".into(),
                data: serde_json::Value::Null,
            }),
        ]
    }

    #[tokio::test]
    async fn roundtrip_every_message_over_a_pipe() {
        let (a, b) = tokio::io::duplex(1 << 16);
        let (ra, mut wa) = tokio::io::split(a);
        let (rb, mut wb) = tokio::io::split(b);
        let mut ra = BufReader::new(ra);
        let mut rb = BufReader::new(rb);
        let mut buf = Vec::new();
        for (i, m) in all_to_hostd().into_iter().enumerate() {
            let env = Envelope::new(Some(i as u64), m);
            write_frame(&mut wa, &env).await.unwrap();
            let got: Envelope<ToHostd> = read_frame(&mut rb, &mut buf).await.unwrap().unwrap();
            assert_eq!(got, env);
        }
        for m in all_from_hostd() {
            let env = Envelope::new(None, m);
            write_frame(&mut wb, &env).await.unwrap();
            let got: Envelope<FromHostd> = read_frame(&mut ra, &mut buf).await.unwrap().unwrap();
            assert_eq!(got, env);
        }
        // The read half keeps the duplex alive, so end the stream explicitly.
        wa.shutdown().await.unwrap();
        assert!(
            read_frame::<_, ToHostd>(&mut rb, &mut buf)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn wire_shape_is_stable() {
        let j = serde_json::to_value(Envelope::new(
            Some(2),
            ToHostd::Hold {
                target: Target::Card("A".into()),
                ttl_s: 60,
                reason: "x".into(),
            },
        ))
        .unwrap();
        assert_eq!(
            j,
            serde_json::json!({"id": 2, "type": "hold", "target": {"card": "A"}, "ttl_s": 60, "reason": "x"})
        );
        let j = serde_json::to_value(Envelope::new(None, ToHostd::Status)).unwrap();
        assert_eq!(j, serde_json::json!({"type": "status"}));
        let j = serde_json::to_value(Envelope::new(
            Some(1),
            ToHostd::TelemetrySubscribe(Rates::default()),
        ))
        .unwrap();
        assert_eq!(j["type"], "telemetry.subscribe");
        assert_eq!(j["gpu_ms"], 500);
        // Absent GPU metrics are absent keys, not null or 0.
        let s = serde_json::to_value(GpuSample {
            uuid: "u".into(),
            core_c: Some(1.0),
            ..Default::default()
        })
        .unwrap();
        assert!(s.get("mem_temp_c").is_none());
    }

    #[tokio::test]
    async fn bad_frames() {
        let input = b"\n{\"id\":5,\"type\":\"nope\"}\nnot json\n{\"type\":\"status\"}\n".to_vec();
        let mut r = BufReader::new(&input[..]);
        let mut buf = Vec::new();
        match read_frame::<_, ToHostd>(&mut r, &mut buf).await {
            Err(FrameError::Decode { id: Some(5), .. }) => {}
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            read_frame::<_, ToHostd>(&mut r, &mut buf).await,
            Err(FrameError::Decode { id: None, .. })
        ));
        let ok = read_frame::<_, ToHostd>(&mut r, &mut buf)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ok.msg, ToHostd::Status);
        let big = vec![b'x'; MAX_FRAME + 10];
        let mut r = BufReader::new(&big[..]);
        assert!(matches!(
            read_frame::<_, ToHostd>(&mut r, &mut buf).await,
            Err(FrameError::TooLong)
        ));
    }
}
