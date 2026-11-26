//! The web UI host (port of `OpenOrca.WebUi` and the CLI's `--hostui`): an axum server with
//! the session hub over a websocket (decision D4), the React client, projects, files, git, the
//! terminal and the bench runner.

pub mod audit;
pub mod auth;
pub mod bench;
pub mod bridge;
pub mod connection;
pub mod files;
pub mod git;
pub mod glance;
pub mod hostui;
pub mod hub;
pub mod key_requests;
pub mod live;
pub mod login;
pub mod native;
pub mod parts;
pub mod paths;
pub mod profiles;
pub mod projects;
pub mod pty;
pub mod router_admin;
pub mod server;
pub mod settings;
pub mod util;

pub use live::{AgentHost, LiveSession, SessionFactory};
pub use server::{WebUiOptions, WebUiServer};
