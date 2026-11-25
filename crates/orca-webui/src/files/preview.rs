//! The HTML preview tab (`PreviewEndpoints`): files served under a signed, expiring token.

use std::sync::Arc;

use axum::Router;

use crate::server::AppState;

pub const PREFIX: &str = "/preview";

/// The key previews are signed with (random per server).
pub struct PreviewKeys {
    pub key: Vec<u8>,
}

impl PreviewKeys {
    pub fn new() -> Self {
        Self {
            key: crate::util::random_bytes(32),
        }
    }
}

impl Default for PreviewKeys {
    fn default() -> Self {
        Self::new()
    }
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
}
