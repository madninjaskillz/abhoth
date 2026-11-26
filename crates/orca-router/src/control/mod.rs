//! The control plane (ROUTER-CONFIG.md): the configuration (`router.toml`), the runtime state
//! store (`router.db`), command execution on GPU hosts (local and forced-command SSH), engine
//! drivers and the engine lifecycle. Everything here is behind `control.enabled` (off by
//! default): with it off the router is the data plane of ROUTER.md.
//!
//! Module map (owners in brackets while three agents build it):
//! - [`schema`], [`load`], [`validate`], [`edit`], [`template`]: the config (C1).
//! - [`store`]: `router.db` (core tables; other parts add their own with `migrate_part`).
//! - [`exec`], [`ssh`], [`hostctl`], [`builtins`], [`telemetry`]: execution (C3).
//! - [`driver`], [`drivers`], [`resolve`], [`lifecycle`], [`manager`]: engines (C4).
//! - [`import`]: the importer from DeepThought's files (C2).

pub mod browse;
pub mod builtins;
pub mod caps;
pub mod downloads;
pub mod driver;
pub mod drivers;
pub mod exec;
pub mod fetch;
pub mod hf;
pub mod host;
pub mod hostctl;
pub mod hostd;
pub mod import;
pub mod inventory;
pub mod lifecycle;
pub mod load;
pub mod reconcile;
pub mod resolve;
pub mod schema;
pub mod ssh;
pub mod startup;
pub mod store;
pub mod telemetry;
pub mod template;
pub mod validate;
