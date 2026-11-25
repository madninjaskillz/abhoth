//! The web UI host (port of `OpenOrca.WebUi` and the CLI's `--hostui`): an axum server with
//! the session hub over a websocket (decision D4), the React client, projects, files, git, the
//! terminal and the bench runner.

pub mod audit;
pub mod auth;
pub mod bench;
pub mod files;
pub mod git;
pub mod hub;
pub mod live;
pub mod paths;
pub mod profiles;
pub mod projects;
pub mod pty;
pub mod server;
pub mod settings;
pub mod util;

pub use live::{AgentHost, LiveSession, SessionFactory};
pub use server::{WebUiOptions, WebUiServer};
