//! Engine packages for Abhoth (ROUTER-MEDIA-PLAN.md 10 and 13.3, D153).
//!
//! - [`manifest`]: the `abhoth-engine.json` types;
//! - [`validate`]: `abhoth engine validate` (shape, templates, pins, metadata only);
//! - [`host`]: host facts, method choice and the hardware fit;
//! - [`render`]: templates and start plans (docker argv, compose, host process);
//! - [`pins`]: digest, hash and commit pins;
//! - [`store`]: store indexes and the embedded default store;
//! - [`runner`]: install, start, stop, update, uninstall, and process supervision;
//! - [`exec`]: how commands run on the host and how artifacts are fetched.
//!
//! Manual engines (no package) stay first-class in the router: nothing in placement or
//! routing needs this crate.

pub mod cli;
pub mod exec;
pub mod host;
pub mod manifest;
pub mod pins;
pub mod render;
pub mod runner;
pub mod state;
pub mod store;
pub mod validate;

pub use host::{CardSel, HostFacts, MethodFilter, choose_method, fit};
pub use manifest::{Manifest, Method, MethodType};
pub use render::{RunSpec, StartPlan, plan_start};
pub use runner::{LocalRunner, Runner};
pub use state::{Approvals, Handle, Installed, State};
pub use store::Package;
pub use validate::{Mode, Report, validate_path};

#[cfg(test)]
mod tests;
