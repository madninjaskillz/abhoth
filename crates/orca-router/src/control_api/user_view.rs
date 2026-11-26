//! The view for everyone who is not an admin (and the app's "point at a supervisor" mode):
//! the models this principal may use with their live/cold state, the actions it is allowed,
//! and, when it may switch, the layouts and the running job. Same data as the admin view,
//! filtered by [`crate::access`]; never config text, audit, leases of others or telemetry
//! unless the principal may read control state.
//!
//! - `GET /v1/abhoth/view`: one [`UserView`];
//! - `GET /v1/abhoth/events`: SSE, `event: view` whenever the view changes, plus `event: job`
//!   (switch progress) for principals that may switch and `event: telemetry` for those that
//!   may read control state.
//!
//! The paths are provisional until SUPERVISOR.md fixes the supervisor API's names.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::header;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use super::model::*;
use super::{ControlBackend, ControlPlane, Event, sse};
use crate::access::{Perm, Principal};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Actions {
    pub chat: bool,
    pub models: bool,
    /// Plan and switch layouts.
    pub switch: bool,
    /// Requests may start an on-demand switch (on routes that allow it).
    pub on_demand: bool,
    pub leases: bool,
    pub read_control: bool,
    pub edit_control: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModel {
    /// The public name clients send as `model`.
    pub name: String,
    /// chat | image | audio (only chat routes exist so far).
    pub kind: String,
    /// live | cold | switching
    pub status: String,
    /// What a request does while cold: reject | fallback | on_demand.
    pub cold: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fallback: Vec<String>,
    /// This principal's request may start an on-demand switch for it.
    pub can_switch_on_demand: bool,
    /// Cards or upstreams serving it now (only for principals that may read control state).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub serving: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UserView {
    pub principal: String,
    pub admin: bool,
    pub actions: Actions,
    pub models: Vec<CatalogModel>,
    /// Only with `actions.switch`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub layouts: Vec<LayoutView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<JobSummary>,
    /// Only with `actions.readControl`: the cards (the same widgets as the admin pages).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cards: Vec<CardView>,
    pub rev: String,
}

pub fn actions(cp: &ControlPlane, p: &Principal) -> Actions {
    let a = |perm| cp.access.allows(p, perm);
    Actions {
        chat: a(Perm::Chat),
        models: a(Perm::Models),
        switch: a(Perm::Switch),
        on_demand: a(Perm::OnDemand),
        leases: a(Perm::Leases),
        read_control: a(Perm::ReadControl),
        edit_control: a(Perm::EditControl),
    }
}

/// The filtered view of `b`'s state for `p`.
pub fn view_for(cp: &ControlPlane, b: &dyn ControlBackend, p: &Principal) -> UserView {
    let acts = actions(cp, p);
    let st = b.state();
    let switching = st.job.is_some();
    let models = if acts.models {
        b.routes()
            .into_iter()
            .filter(|r| (p.is_admin() || !r.hidden) && p.may_use_route(&r.access))
            .map(|r| {
                let live = !r.serving_now.is_empty();
                CatalogModel {
                    kind: "chat".into(),
                    status: if live {
                        "live"
                    } else if switching && r.cold == "on_demand" {
                        "switching"
                    } else {
                        "cold"
                    }
                    .into(),
                    can_switch_on_demand: r.cold == "on_demand"
                        && acts.on_demand
                        && (p.is_admin() || r.allow_user_switch || matches!(p, Principal::Key(_))),
                    serving: if acts.read_control {
                        r.serving_now.clone()
                    } else {
                        vec![]
                    },
                    fallback: r.fallback.clone(),
                    cold: r.cold,
                    name: r.name,
                }
            })
            .collect()
    } else {
        vec![]
    };
    UserView {
        principal: p.label(),
        admin: p.is_admin(),
        layouts: if acts.switch { st.layouts.clone() } else { vec![] },
        job: if acts.switch || acts.read_control {
            st.job.clone()
        } else {
            None
        },
        cards: if acts.read_control {
            st.servers.iter().flat_map(|s| s.cards.clone()).collect()
        } else {
            vec![]
        },
        rev: st.rev.clone(),
        actions: acts,
        models,
    }
}

/// `GET /v1/abhoth/events` for `p`.
pub fn events(cp: Arc<ControlPlane>, p: Principal) -> Response {
    let rx = cp.backend.subscribe();
    let first = view_for(&cp, cp.backend.as_ref(), &p);
    let first_chunk = sse(&Event {
        kind: "view",
        data: serde_json::to_value(&first).unwrap_or_default(),
    });
    struct S {
        rx: broadcast::Receiver<Event>,
        cp: Arc<ControlPlane>,
        p: Principal,
        last: UserView,
        first: Option<Bytes>,
    }
    let s = S {
        rx,
        cp,
        p,
        last: first,
        first: Some(first_chunk),
    };
    let stream = futures_util::stream::unfold(s, |mut s| async move {
        if let Some(f) = s.first.take() {
            return Some((Ok::<_, Infallible>(f), s));
        }
        loop {
            let next = tokio::time::timeout(Duration::from_secs(15), s.rx.recv()).await;
            let ev = match next {
                Err(_) => return Some((Ok(Bytes::from_static(b": keepalive\n\n")), s)),
                Ok(Err(broadcast::error::RecvError::Closed)) => return None,
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => None,
                Ok(Ok(e)) => Some(e),
            };
            let acts = &s.last.actions;
            if let Some(e) = &ev {
                if e.kind == "telemetry" && acts.read_control {
                    return Some((Ok(sse(e)), s));
                }
                if e.kind == "job" && (acts.switch || acts.read_control) {
                    return Some((Ok(sse(e)), s));
                }
                if e.kind == "audit" || e.kind == "telemetry" {
                    continue;
                }
            }
            let v = view_for(&s.cp, s.cp.backend.as_ref(), &s.p);
            if v != s.last {
                s.last = v.clone();
                let chunk = sse(&Event {
                    kind: "view",
                    data: serde_json::to_value(&v).unwrap_or_default(),
                });
                return Some((Ok(chunk), s));
            }
        }
    });
    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// `GET /v1/abhoth/view` and `/v1/abhoth/events`.
pub async fn handle(
    cp: &Arc<ControlPlane>,
    path: &str,
    headers: &axum::http::HeaderMap,
    peer: Option<std::net::IpAddr>,
) -> Response {
    use crate::access::AuthError;
    let p = match cp.access.authenticate(headers, peer, "GET", path).await {
        Ok(p) => p,
        Err(AuthError::Unauthorized(m)) => return super::err(401, m),
        Err(AuthError::Unavailable) => {
            return super::err(503, "the accounts server can't vouch for this key right now");
        }
    };
    if p == (Principal::Anonymous { lan: false }) {
        return super::err(401, "a key is needed");
    }
    if path.ends_with("/events") {
        events(cp.clone(), p)
    } else {
        super::ok(&view_for(cp, cp.backend.as_ref(), &p))
    }
}
