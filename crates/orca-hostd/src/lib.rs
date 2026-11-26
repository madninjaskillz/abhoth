//! `abhoth hostd`: the root host daemon of the Abhoth Supervisor (SUPERVISOR.md 2.3, 2.4).
//!
//! - [`protocol`]: the typed hostd protocol (framed JSON) and its framing.
//! - [`limits`]: the root-owned limits file that bounds every policy.
//!
//! The daemon itself (sampler, socket server, last-known-good policy, watchdog, `boot` and
//! `failsafe`) is built on these. Not wired into the `abhoth` CLI yet: the `abhoth-hostd`
//! binary of this crate runs it standalone.

pub mod limits;
pub mod protocol;
