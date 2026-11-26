//! The settings' connection panel (D149, `docs/design/ROUTER-MODE.md`): what `lmStudio.baseUrl`
//! is, and help finding and using an Abhoth router. Admin-only (under `/api/settings`).
//!
//! - `GET /api/settings/connection`: the connection setting, the base URL and whether that URL
//!   identifies as a router (so the page can offer router mode; it never switches by itself);
//! - `POST /api/settings/connection/identify` `{url}`: is that URL a router?
//! - `GET /api/settings/connection/discover`: routers advertised on the LAN (mDNS), each checked
//!   with its identification document;
//! - `GET /api/settings/connection/models`: the configured router's models and their metadata,
//!   as the configured key sees them;
//! - `POST /api/settings/connection/use` `{url}`: router mode on that router (connection and
//!   base URL in one save).
//!
//! Every request to a router is a read-only GET with `x-abhoth-no-switch: 1` and no redirects.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use orca_core::config::ConfigManager;
use orca_core::config::schema::{self, CONNECTION_KEY};
use orca_core::router_mode::{self, RouterInfo};

use crate::audit::Caller;
use crate::server::AppState;

pub mod discover;

/// How long a LAN browse listens.
pub const BROWSE_FOR: Duration = Duration::from_millis(2500);

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/settings/connection", get(status))
        .route("/api/settings/connection/identify", post(identify))
        .route("/api/settings/connection/discover", get(discover_routers))
        .route("/api/settings/connection/models", get(models))
        .route("/api/settings/connection/use", post(use_router))
}

fn load(state: &AppState) -> ConfigManager {
    let mut m = ConfigManager::new(state.options.config_home.as_deref());
    // A broken or missing file is the defaults, as the settings page shows.
    let _ = m.load();
    m
}

fn bad(status: StatusCode, error: impl Into<String>) -> Response {
    (status, axum::Json(json!({"error": error.into()}))).into_response()
}

/// What the panel shows for the configured URL.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionStatus {
    /// "router" or "server".
    pub connection: String,
    pub base_url: String,
    /// The configured URL's identification document, when it is a router.
    pub router: Option<RouterInfo>,
    /// It did not answer (`router` is then unknown).
    pub error: Option<String>,
    /// Server mode on a URL that is a router: offer router mode (never forced).
    pub suggest_router: bool,
    /// The configured key's kind (`router_mode::key_kind`: none, placeholder, abhoth, other),
    /// never the key.
    pub api_key: String,
}

/// [`ConnectionStatus`] for a config and what identifying its URL gave.
pub fn status_of(
    connection_router: bool,
    base_url: &str,
    api_key: &str,
    identified: Result<Option<RouterInfo>, String>,
) -> ConnectionStatus {
    let (router, error) = match identified {
        Ok(r) => (r, None),
        Err(e) => (None, Some(e)),
    };
    ConnectionStatus {
        connection: if connection_router {
            "router"
        } else {
            "server"
        }
        .into(),
        base_url: base_url.to_string(),
        suggest_router: !connection_router && router.is_some(),
        api_key: router_mode::key_kind(api_key).into(),
        router,
        error,
    }
}

async fn status(State(state): State<Arc<AppState>>) -> Response {
    let m = load(&state);
    let lm = &m.config.lm_studio;
    let identified = router_mode::identify(&lm.base_url, &router_mode::probe_client()).await;
    axum::Json(status_of(
        lm.is_router(),
        &lm.base_url,
        &lm.resolve_api_key(),
        identified,
    ))
    .into_response()
}

#[derive(Deserialize)]
struct UrlBody {
    url: String,
}

/// A URL a probe may be sent to: http(s) with a host.
fn probe_url(url: &str) -> Result<String, String> {
    let root = router_mode::root_of(url);
    if !schema::is_http_url(&root) {
        return Err("Not an http:// or https:// URL.".into());
    }
    Ok(root)
}

async fn identify(axum::Json(b): axum::Json<UrlBody>) -> Response {
    let root = match probe_url(&b.url) {
        Ok(r) => r,
        Err(e) => return bad(StatusCode::BAD_REQUEST, e),
    };
    let r = router_mode::identify(&root, &router_mode::probe_client()).await;
    let (router, error) = match r {
        Ok(r) => (r, None),
        Err(e) => (None, Some(e)),
    };
    axum::Json(json!({
        "url": root,
        "baseUrl": router_mode::base_url_of(&root, router.as_ref()),
        "router": router,
        "error": error,
    }))
    .into_response()
}

async fn discover_routers() -> Response {
    let found = tokio::task::spawn_blocking(|| discover::browse(BROWSE_FOR))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    match found {
        Ok(list) => {
            let checked = discover::verify(list, &router_mode::probe_client()).await;
            axum::Json(json!({"routers": checked})).into_response()
        }
        // Multicast unavailable here: say so; the panel still takes a pasted URL.
        Err(e) => axum::Json(json!({"routers": [], "error": e})).into_response(),
    }
}

async fn models(State(state): State<Arc<AppState>>) -> Response {
    let m = load(&state);
    let lm = &m.config.lm_studio;
    let key = lm.resolve_api_key();
    match router_mode::models_cached(&lm.base_url, &key, &router_mode::probe_client()).await {
        Ok(doc) => {
            let mut v = serde_json::to_value(&doc).unwrap_or_default();
            v["chosen"] = json!(lm.primary_model());
            axum::Json(v).into_response()
        }
        Err(e) => bad(StatusCode::BAD_GATEWAY, e),
    }
}

#[derive(Deserialize)]
struct UseBody {
    url: String,
    /// The default model (optional).
    #[serde(default)]
    model: Option<String>,
}

async fn use_router(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    principal: Option<axum::Extension<crate::login::Principal>>,
    axum::Json(b): axum::Json<UseBody>,
) -> Response {
    let root = match probe_url(&b.url) {
        Ok(r) => r,
        Err(e) => return bad(StatusCode::BAD_REQUEST, e),
    };
    // Only a URL that says it is a router (the page offers this after identifying it).
    let info = match router_mode::identify(&root, &router_mode::probe_client()).await {
        Ok(Some(i)) => i,
        Ok(None) => return bad(StatusCode::BAD_REQUEST, "That URL is not an Abhoth router."),
        Err(e) => {
            return bad(
                StatusCode::BAD_GATEWAY,
                format!("The router did not answer: {e}"),
            );
        }
    };
    let base = router_mode::base_url_of(&root, Some(&info));
    let mut m = load(&state);
    let mut changes: Vec<(String, Value)> = vec![
        (CONNECTION_KEY.into(), json!("router")),
        ("lmStudio.baseUrl".into(), json!(base)),
    ];
    if let Some(model) = b.model.as_deref().filter(|s| !s.trim().is_empty()) {
        changes.push(("lmStudio.models".into(), json!([model.trim()])));
    }
    for (k, v) in &changes {
        let c = schema::apply(&mut m.config, k, Some(v), false, None);
        if !c.ok {
            return bad(StatusCode::BAD_REQUEST, c.error.unwrap_or_default());
        }
    }
    if let Err(e) = m.save() {
        return bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    router_mode::clear_cache();
    let caller = Caller {
        ip: Some(addr.ip().to_string()),
        user: principal.as_ref().map(|p| p.username.clone()),
        admin: principal.as_ref().is_some_and(|p| p.admin),
        ..Default::default()
    };
    state.audit.write(
        Some(&caller),
        "settings.connection",
        json!({"connection": "router", "baseUrl": base, "router": info.name, "model": b.model}),
    );
    axum::Json(json!({"ok": true, "baseUrl": base, "router": info, "live": false})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_mode_is_offered_only_when_server_mode_points_at_a_router() {
        let info = RouterInfo {
            service: "abhoth-router".into(),
            name: "R".into(),
            ..Default::default()
        };
        let s = status_of(
            false,
            "http://r:8010/v1",
            "lm-studio",
            Ok(Some(info.clone())),
        );
        assert!(s.suggest_router);
        assert_eq!(s.connection, "server");
        let s = status_of(true, "http://r:8010/v1", "", Ok(Some(info)));
        assert!(!s.suggest_router, "already router mode");
        let s = status_of(false, "http://lmstudio:1234/v1", "lm-studio", Ok(None));
        assert!(!s.suggest_router);
        let s = status_of(
            false,
            "http://dead:1/v1",
            "abk_x",
            Err("could not connect".into()),
        );
        assert!(!s.suggest_router);
        assert_eq!(s.error.as_deref(), Some("could not connect"));
    }

    #[test]
    fn only_http_urls_are_probed() {
        assert_eq!(probe_url("http://r:8010/v1/").unwrap(), "http://r:8010");
        assert_eq!(probe_url("r.lan:8010").unwrap(), "http://r.lan:8010");
        assert!(probe_url("file:///etc/passwd").is_err());
        assert!(probe_url("ftp://x").is_err());
    }
}
