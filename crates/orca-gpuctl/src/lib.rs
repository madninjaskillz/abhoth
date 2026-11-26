//! GPU clocks and power for the Abhoth Supervisor (SUPERVISOR.md 6).
//!
//! This crate replaces three things on DeepThought:
//!
//! - **clockctl** (`:9877`): power limit, clock lock, the HA "GPU Max Clock" cap, unlock, status;
//! - **gpu-power** (`:9880`): idle clock-down to 135 MHz after 60 s, wake by sniffing, NVML
//!   polling or HTTP, holds, the dead-man cron;
//! - **boot.sh** (PREINIT): persistence mode and card B's application clocks.
//!
//! It is pure policy plus one IO seam, [`nvml::GpuControl`]. hostd (`orca-hostd`) owns the
//! process, the socket protocol and the timers; it feeds samples and messages into a
//! [`controller::Controller`] and applies what comes out. Nothing here is wired into a running
//! binary yet.
//!
//! Modules:
//! - [`config`]: `[cards.X.power]` (persistence, power limit, app clocks, `serving`, `idle`,
//!   `reassert`, `on_mismatch`, `allow_cap`, drift thresholds);
//! - [`limits`]: the root-owned limits file and the guard that refuses anything outside it;
//! - [`nvml`]: the command set, readings, the recording fake and the `nvidia-smi` adapter;
//! - [`controller`]: the per-card state machine (serving / low), wake, holds, cap overrides,
//!   assert and drift detection;
//! - [`plans`]: `hostd boot` and `hostd failsafe`;
//! - [`sniff`]: the classic BPF filter and packet classifier for the sniff wake source.
//!
//! # Parity (read-only survey of DeepThought, 2026-11-26)
//!
//! | Today | Here | Status |
//! |---|---|---|
//! | boot.sh `nvidia-smi -pm 1` (all cards) | `persistence = true`, [`plans::boot_plan`], assert | done |
//! | boot.sh `-ac 877,1380` on card B | `app_clocks = [877, 1380]`, boot + assert | done |
//! | boot.sh sysext / modprobe / ldconfig | not clocks: stays in boot.sh (or a hostd boot step) | out of scope |
//! | clockctl `/pl?w=` (150-400 W, every visible card) | `power_limit_w` per card, bounded by the limits file | done |
//! | clockctl `/pl?reset=1` (default PL) | set `power_limit_w` to the default | config |
//! | clockctl `/lock?mhz=` (500-1600, `-lgc n,n`) | `serving = { pin = n }` or `{ range }` | done |
//! | clockctl `/maxclock?mhz=` (800-1600, `-lgc 135,n`, A and B) | per-card cap override with TTL ≤ 1 h; refused where `allow_cap = false` (A) | done |
//! | clockctl `/maxclock?reset=1`, `/unlock` (re-applies the cap) | cap `None`; `serving = "unlocked"` (`-rgc`) | done |
//! | clockctl `/status` | [`controller::Controller::status`] | done (HTTP is the supervisor's) |
//! | clockctl `exclude=` per call | per-card config; unlisted UUIDs refused by the limits file | done |
//! | gpu-power idle 60 s at util ≤ 0 → `-lgc 135,135` | `idle = { after, lock_mhz, util_at_most }` | done |
//! | gpu-power wake: sniff (AF_PACKET + cBPF, POST/PUT) | [`sniff`] filter + classifier; socket in hostd | logic done |
//! | gpu-power wake: NVML util, compute-process list change | `util` sample, `WakeSource::Procs` | done |
//! | gpu-power `GET /wake?gpu=` | [`controller::Controller::wake`] (`Admin`), plus `Router` at admission (new) | done |
//! | gpu-power port discovery from /proc + `EXTRA_PORTS` | engine ports from config + `sniff_ports` ([`controller::Controller::sniff_map`]) | done |
//! | gpu-power holds: `/run/gpu-power-hold[-idx/-uuid]`, `/hold?secs=` (≤ 24 h) | `hold`/`release` messages, TTL ≤ 1 h, by reason | done |
//! | gpu-power quiet-mode hold (`quiet.flag`) | gone (gpu-fan-control retired 2026-11-21); calendar `clock_profile` | n/a |
//! | gpu-power mode on / dry-run / off (`state/mode`) | [`controller::Mode`] | done |
//! | gpu-power external `-rgc` detection: re-lock, 3 in 10 min → 15 min back-off | same | done |
//! | gpu-power lock failure → back-off at max | same | done |
//! | gpu-power NVML unreadable while low → max | `WakeSource::Error` | done |
//! | gpu-power startup reset of lowered cards | [`controller::Controller::start`] applies serving on every card | done |
//! | gpu-power exit reset (SIGTERM) | hostd's job on shutdown: `set_mode(Off)` then reset lowered | hostd |
//! | gpu-power `low.json` + heartbeat; failsafe.sh (45 s, lowered cards) | [`controller::Controller::lowered_uuids`]; [`plans::failsafe_plan`] (120 s, all managed) | done |
//! | gpu-power saved Wh / low seconds / metrics | per-card `saved_wh`, `low_secs`, `low_count`, wakes in status | done (metrics export: hostd) |
//! | nothing re-checks A after a switch | assert on engine `Ready` and every 5 min; drift alert | new |
//! | gpu-power wake `-rgc` silently drops clockctl's cap | wake re-applies serving incl. the cap | fixed |

pub mod config;
pub mod controller;
pub mod limits;
pub mod nvml;
pub mod plans;
pub mod sniff;

pub use config::{CardPowerConfig, IdleConfig, OnMismatch, Reassert, Serving, WakeSource};
pub use controller::{AssertReason, ClockEvent, Controller, Mode};
pub use limits::{LimitedControl, Limits};
pub use nvml::{FakeNvml, GpuCommand, GpuControl, GpuReading};
