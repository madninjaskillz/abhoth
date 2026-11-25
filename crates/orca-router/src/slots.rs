//! The pig-slots lease registry, served by the router (ROUTER.md SHOULD "Server-side slots"):
//! a port of `$AI_ROOT/pig-slots/server.py` with the same JSON, TTL, layout file
//! (slot list + freeze) and `leases.json`, so `orca-core::slots::HttpSlotRegistry`, PiG and the
//! bench's `--wait-registry` work against it unchanged.
//!
//! - `POST /claim {key, owner, host, pid, label, kind: main|agent, copy? | slot? | avoid?}`
//!   -> `{ok, slot, copy, ttl}` (`existing: true` for a key that holds one) or
//!   `{ok: false, reason: full|switching}`
//! - `POST /renew {keys}` -> `{renewed, lost}`
//! - `POST /release {key}` or `{host, pid}` -> `{ok, released}`
//! - `GET /list` (and `/`) -> `{ttl, slots, free, load, layout, frozen, orphans}`
//!
//! What is new: the router knows which card each lease is on, so placement can follow leases
//! (`lease_for_key`) and on-demand decisions read leases without an HTTP call.

use std::path::PathBuf;
use std::sync::Mutex;

use axum::http::Method;
use axum::response::Response;
use indexmap::IndexMap;
use orca_core::slots::registry::SlotClaim;
use serde_json::{Map, Value, json};

use crate::config::{FileSig, Snapshot};
use crate::leases::{GuardLease, LeaseTable, SlotView};
pub use crate::leases::{Lease, copy_of};
use crate::proxy::{json_response, now_epoch};
use crate::state::AppState;

#[derive(Debug, Default)]
struct LayoutFile {
    sig: Option<FileSig>,
    slots: Option<Vec<String>>,
    frozen: bool,
    layout: Option<String>,
    reason: Option<String>,
}

#[derive(Default)]
struct Inner {
    table: LeaseTable,
    layout: LayoutFile,
    /// Set by the control plane (ROUTER-CONFIG.md 8.2): the active layout's slots, withdrawn
    /// cards and the freeze, in memory. Wins over the layout file.
    control: Option<SlotView>,
    loaded: bool,
}

#[derive(Default)]
pub struct Registry {
    inner: Mutex<Inner>,
}

/// The claim fields, parsed into orca-core's wire type (the client side of the same protocol).
pub fn parse_claim(b: &Map<String, Value>) -> SlotClaim {
    let s = |k: &str| b.get(k).and_then(Value::as_str).map(str::to_string);
    SlotClaim {
        key: match b.get("key") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(v) => v.to_string(),
        },
        owner: s("owner").unwrap_or_default(),
        host: s("host").unwrap_or_default(),
        pid: b.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32,
        label: s("label").unwrap_or_default(),
        kind: s("kind").unwrap_or_default(),
        copy: s("copy"),
        slot: s("slot"),
        avoid: b.get("avoid").and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        }),
    }
}

impl Registry {
    /// The cards `/list` always shows: the configured upstream cards (pig-slots: A and B).
    fn base_cards(snap: &Snapshot) -> Vec<String> {
        snap.config.upstreams.keys().cloned().collect()
    }

    fn view(inner: &Inner, snap: &Snapshot) -> SlotView {
        if let Some(v) = &inner.control {
            return v.clone();
        }
        let cards = Self::base_cards(snap);
        if let Some(s) = &inner.layout.slots {
            return SlotView {
                slots: s.clone(),
                frozen: inner.layout.frozen,
                layout: inner.layout.layout.clone(),
                reason: inner.layout.reason.clone(),
                cards,
            };
        }
        let mut slots = Vec::new();
        for (card, u) in &snap.config.upstreams {
            if u.online {
                for i in 1..=u.lanes {
                    slots.push(format!("{card}{i}"));
                }
            }
        }
        SlotView {
            slots,
            cards,
            ..SlotView::default()
        }
    }

    fn refresh(inner: &mut Inner, snap: &Snapshot) {
        let Some(path) = snap.config.slots.layout_file.as_deref() else {
            inner.layout = LayoutFile::default();
            return;
        };
        let sig = FileSig::of(std::path::Path::new(path));
        if sig == inner.layout.sig && inner.layout.sig.is_some() {
            return;
        }
        let mut lf = LayoutFile {
            sig,
            ..LayoutFile::default()
        };
        if sig.is_some() {
            match std::fs::read_to_string(path)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            {
                Some(d) => {
                    lf.slots = Some(
                        d.get("slots")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_str().map(str::to_string))
                                    .filter(|x| !x.is_empty())
                                    .collect()
                            })
                            .unwrap_or_default(),
                    );
                    lf.frozen = d.get("frozen").and_then(Value::as_bool).unwrap_or(false);
                    lf.layout = d.get("layout").and_then(Value::as_str).map(str::to_string);
                    lf.reason = d.get("reason").and_then(Value::as_str).map(str::to_string);
                }
                None => lf.sig = None, // half-written: try again next call
            }
        }
        inner.layout = lf;
    }

    fn state_path(snap: &Snapshot) -> Option<PathBuf> {
        snap.config.slots.state_file.as_deref().map(PathBuf::from)
    }

    fn prepare(&self, snap: &Snapshot) -> std::sync::MutexGuard<'_, Inner> {
        let mut g = self.inner.lock().unwrap();
        if !g.loaded {
            g.loaded = true;
            if let Some(p) = Self::state_path(snap)
                && let Ok(t) = std::fs::read_to_string(&p)
                && let Ok(m) = serde_json::from_str::<IndexMap<String, Lease>>(&t)
            {
                Self::refresh(&mut g, snap);
                let slots = Self::view(&g, snap).slots;
                for (s, l) in m {
                    if slots.contains(&s) {
                        g.table.leases.insert(s, l);
                    }
                }
            }
        }
        Self::refresh(&mut g, snap);
        g
    }

    fn save(inner: &Inner, snap: &Snapshot) {
        let Some(p) = Self::state_path(snap) else {
            return;
        };
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let tmp = p.with_extension("json.tmp");
        if let Ok(text) = serde_json::to_string(&inner.table.leases)
            && std::fs::write(&tmp, text).is_ok()
        {
            let _ = std::fs::rename(&tmp, &p);
        }
    }

    fn run(
        &self,
        snap: &Snapshot,
        f: impl FnOnce(&mut LeaseTable, &SlotView, f64, f64) -> (u16, Value, bool),
    ) -> (u16, Value) {
        let now = now_epoch();
        let ttl = snap.config.slots.ttl_seconds;
        let mut g = self.prepare(snap);
        let view = Self::view(&g, snap);
        let (code, v, changed) = f(&mut g.table, &view, now, ttl);
        if changed {
            Self::save(&g, snap);
        }
        (code, v)
    }

    pub fn claim(&self, snap: &Snapshot, b: &Map<String, Value>) -> (u16, Value) {
        self.claim_as(snap, b, None)
    }

    /// A claim that records the principal (user or key id) making it.
    pub fn claim_as(
        &self,
        snap: &Snapshot,
        b: &Map<String, Value>,
        principal: Option<&str>,
    ) -> (u16, Value) {
        self.run(snap, |t, v, now, ttl| t.claim(v, b, now, ttl, principal))
    }

    pub fn renew(&self, snap: &Snapshot, b: &Map<String, Value>) -> (u16, Value) {
        self.run(snap, |t, _, now, ttl| t.renew(b, now, ttl))
    }

    pub fn release(&self, snap: &Snapshot, b: &Map<String, Value>) -> (u16, Value) {
        self.run(snap, |t, _, _, _| t.release(b))
    }

    pub fn list(&self, snap: &Snapshot) -> Value {
        self.run(snap, |t, v, now, ttl| {
            let (v, changed) = t.list(v, now, ttl);
            (200, v, changed)
        })
        .1
    }

    /// The card and kind of the lease held by `key`, if any (placement).
    pub fn lease_for_key(&self, snap: &Snapshot, key: &str) -> Option<(String, String)> {
        let now = now_epoch();
        let g = self.prepare(snap);
        g.table
            .leases
            .iter()
            .find(|(_, l)| l.key == key && now - l.renewed <= snap.config.slots.ttl_seconds)
            .map(|(s, l)| (copy_of(s), l.kind.clone()))
    }

    /// Cards holding a live `kind: main` lease (Lead reservation).
    pub fn lead_cards(&self, snap: &Snapshot) -> Vec<String> {
        let now = now_epoch();
        let g = self.prepare(snap);
        let mut v: Vec<String> = g
            .table
            .leases
            .iter()
            .filter(|(_, l)| l.kind == "main" && now - l.renewed <= snap.config.slots.ttl_seconds)
            .map(|(s, _)| copy_of(s))
            .collect();
        v.dedup();
        v
    }

    /// `[(slot, who)]` of live leases (on-demand decisions; layoutctl.leases()).
    pub fn held(&self, snap: &Snapshot) -> Vec<(String, String)> {
        let now = now_epoch();
        let g = self.prepare(snap);
        g.table
            .leases
            .iter()
            .filter(|(_, l)| now - l.renewed <= snap.config.slots.ttl_seconds)
            .map(|(s, l)| (s.clone(), format!("{}@{}", l.owner, l.host)))
            .collect()
    }

    // ── the control plane's side (ROUTER-CONFIG.md 5.4 steps 4-5, 8.2, 8.3) ────────────────

    /// Replace the slot view (the active layout's slots, withdrawn cards, the freeze). `None`
    /// goes back to the layout file or the upstreams' lanes.
    pub fn set_control_view(&self, view: Option<SlotView>) {
        self.inner.lock().unwrap().control = view;
    }

    /// The view the control plane set, if any.
    pub fn control_view(&self) -> Option<SlotView> {
        self.inner.lock().unwrap().control.clone()
    }

    /// The view claims see now.
    pub fn current_view(&self, snap: &Snapshot) -> SlotView {
        let g = self.prepare(snap);
        Self::view(&g, snap)
    }

    /// Live leases that refuse a switch of `cards` (`LeaseTable::held_on`).
    pub fn held_on(&self, snap: &Snapshot, cards: &[String]) -> Vec<(String, String)> {
        let now = now_epoch();
        let g = self.prepare(snap);
        let view = Self::view(&g, snap);
        g.table
            .held_on(&view, cards, now, snap.config.slots.ttl_seconds)
    }

    /// Claim or renew a guard lease (the native lease API, section 8.3).
    pub fn claim_guard(
        &self,
        snap: &Snapshot,
        key: &str,
        reason: &str,
        owner: &str,
        principal: Option<&str>,
    ) -> GuardLease {
        let mut g = self.prepare(snap);
        g.table.claim_guard(
            key,
            reason,
            owner,
            principal,
            now_epoch(),
            snap.config.slots.ttl_seconds,
        )
    }

    /// Live guard leases (they block switches and on-demand).
    pub fn live_guards(&self, snap: &Snapshot) -> Vec<GuardLease> {
        let g = self.prepare(snap);
        g.table
            .live_guards(now_epoch(), snap.config.slots.ttl_seconds)
    }
}

/// The pig-slots HTTP surface (on `/slots/...` of the main listener, or every path of the
/// compatibility listener).
pub async fn http(st: &AppState, method: &Method, path: &str, raw: &[u8]) -> Response {
    let snap = st.snapshot();
    if !snap.config.slots.enabled {
        return json_response(404, &json!({"error": "not found"}));
    }
    let reg = &st.ext.registry;
    let path = path.split('?').next().unwrap_or("/");
    if *method == Method::GET {
        return match path {
            "/health" => json_response(
                200,
                &json!({"status": "ok", "service": "pig-slots", "impl": "abhoth-router"}),
            ),
            "/list" | "/" => json_response(200, &reg.list(&snap)),
            _ => json_response(404, &json!({"error": "not found"})),
        };
    }
    if *method != Method::POST {
        return json_response(404, &json!({"error": "not found"}));
    }
    let body: Map<String, Value> = if raw.is_empty() || raw.len() >= 65536 {
        Map::new()
    } else {
        match serde_json::from_slice::<Value>(raw) {
            Ok(Value::Object(m)) => m,
            _ => return json_response(400, &json!({"error": "bad json"})),
        }
    };
    let (code, obj) = match path {
        "/claim" => reg.claim(&snap, &body),
        "/renew" => reg.renew(&snap, &body),
        "/release" => reg.release(&snap, &body),
        _ => return json_response(404, &json!({"error": "not found"})),
    };
    json_response(code, &obj)
}
