//! The terminal panel (`PtyEndpoint`): a PTY per websocket.

use std::sync::Arc;

use axum::Router;

use crate::server::AppState;

#[derive(Default)]
pub struct PtyRegistry {}

impl PtyRegistry {
    pub fn close_all(&self) {}
}

pub fn is_supported() -> bool {
    true
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
}
