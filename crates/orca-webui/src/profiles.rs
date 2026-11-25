//! Inference profiles (`ProfileEndpoints`).

use std::sync::Arc;

use axum::Router;

use crate::server::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
}
