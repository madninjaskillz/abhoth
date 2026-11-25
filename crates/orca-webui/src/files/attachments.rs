//! Prompt attachments and raw file serving (`AttachmentEndpoints`).

use std::sync::Arc;

use axum::Router;

use crate::server::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
}
