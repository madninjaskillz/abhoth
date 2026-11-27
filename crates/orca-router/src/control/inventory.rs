//! The inventory model (ROUTER-INVENTORY.md, D152): servers hold cards, cards say which engines
//! may run each model type, models list the engines that can serve them (one primary) and the
//! servers that have their files. The router places a requested model itself:
//!
//! 1. candidate engines are the model's artifacts, primary first, then file order;
//! 2. a card qualifies when it is managed and open to on-demand, its server has the files (or
//!    an archive restore can bring them back), the card allows that engine for the model's
//!    type, the engine runs that type and has a port on the card, and artifact + engine
//!    profile fit the card's VRAM (less its reserve);
//! 3. the best option per card is kept; cards rank by engine order (primary first).
//!
//! Pins override: a route's `on_demand.pick` fixes the engine (and profile) and its `cards` limit
//! the cards, as before the inventory; then only the hard rules apply (managed, port, files) and
//! the VRAM check is a warning, so a pin that worked before still works. Saved layouts are
//! presets: when one runs this model on a card with an engine, its engine profile is the one used
//! there. Busy cards, leases, guards and the switch brakes stay in [`crate::ondemand::decide`].
//!
//! [`migrate_edits`] writes the defaults this module assumes for older files explicitly (types,
//! primaries, servers with files, engines per card), so the admin pages show what the router does.

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Value, json};

use super::schema::{ColdPolicy, ControlConfig, LayoutCard, ModelKind, PerCard, RouteOnDemand};

/// The inventory version a migrated file carries (`[inventory] version`).
pub const INVENTORY_VERSION: u32 = 1;

/// One card's answer for one model.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CardOption {
    pub card: String,
    pub server: String,
    /// The engine and engine profile it would run with (empty when nothing qualifies).
    pub engine: String,
    pub profile: String,
    /// 0 = the primary engine.
    pub rank: usize,
    pub ok: bool,
    /// VRAM the model needs here (artifact + profile), and what the card offers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub need_gib: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room_gib: Option<f64>,
    /// The files are not on the server but an archive restore brings them back.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub restore: bool,
    /// From a saved layout (the engine profile it uses there).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Why not (or warnings for a pinned option).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub why: Vec<String>,
}

/// A pin from a route: fixed engine (and profile), allowed cards.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pin {
    pub engine: Option<String>,
    pub profile: Option<String>,
    pub cards: Vec<String>,
}

impl Pin {
    pub fn of(od: Option<&RouteOnDemand>) -> Pin {
        let Some(od) = od else {
            return Pin::default();
        };
        Pin {
            engine: od.pick.as_ref().map(|p| p.engine.clone()),
            profile: od.pick.as_ref().and_then(|p| p.profile.clone()),
            cards: od.cards.clone(),
        }
    }
    fn pinned(&self) -> bool {
        self.engine.is_some()
    }
}

/// Engines a card allows for `kind` (None: the pre-inventory rule, any engine with a port).
pub fn card_allows(c: &ControlConfig, card: &str, kind: ModelKind, engine: &str) -> bool {
    let Some(cd) = c.cards.get(card) else {
        return false;
    };
    match &cd.engines {
        None => kind == ModelKind::Llm && has_port(c, engine, card),
        Some(m) => m
            .get(kind.as_str())
            .is_some_and(|l| l.iter().any(|e| e == engine || e == "*")),
    }
}

fn has_port(c: &ControlConfig, engine: &str, card: &str) -> bool {
    c.engines.get(engine).is_some_and(|e| match &e.port {
        PerCard::All(_) => true,
        PerCard::Each(m) => m.contains_key(card),
    })
}

/// The engine profile a saved (visible) layout uses for `model` with `engine` on `card`.
fn preset_profile(c: &ControlConfig, card: &str, engine: &str, model: &str) -> Option<String> {
    for l in c.layouts.values().filter(|l| !l.hidden) {
        let mut prev: Option<&super::schema::CardSpec> = None;
        for (cid, lc) in &l.cards {
            let spec = match lc {
                LayoutCard::Spec(s) => Some(s),
                LayoutCard::Word(w) if w == "same" => prev,
                LayoutCard::Word(_) => None,
            };
            if cid == card
                && let Some(s) = spec
                && s.engine == engine
                && s.model == model
            {
                return Some(s.profile.clone());
            }
            prev = spec;
        }
    }
    None
}

/// Profiles to try, in order: the pin's, a preset's, the artifact's default, the artifact's
/// fitting profiles, then the engine's.
fn profiles_to_try(
    c: &ControlConfig,
    card: &str,
    engine: &str,
    model: &str,
    pin: &Pin,
) -> (Vec<String>, Option<String>) {
    let Some(e) = c.engines.get(engine) else {
        return (Vec::new(), None);
    };
    let a = c
        .models
        .get(model)
        .and_then(|m| super::caps::artifact_for(c, m, engine));
    let mut out: Vec<String> = Vec::new();
    let mut push = |p: &str| {
        if e.profiles.contains_key(p) && !out.iter().any(|x| x == p) {
            out.push(p.to_string());
        }
    };
    if pin.engine.as_deref() == Some(engine)
        && let Some(p) = &pin.profile
    {
        push(p);
        return (out, None);
    }
    let preset = preset_profile(c, card, engine, model);
    if let Some(p) = &preset {
        push(p);
    }
    if let Some(a) = a {
        if let Some(p) = &a.default_profile {
            push(p);
        }
        for p in &a.profiles {
            push(p);
        }
    }
    for p in e.profiles.keys() {
        push(p);
    }
    (out, preset)
}

/// Every card of every server, answered for `model` (sorted: usable first, by rank).
pub fn place(c: &ControlConfig, model: &str, pin: &Pin) -> Vec<CardOption> {
    let mut out = Vec::new();
    let Some(m) = c.models.get(model) else {
        return out;
    };
    let kind = m.kind();
    let engines: Vec<String> = match &pin.engine {
        Some(e) => vec![e.clone()],
        None => super::caps::engine_order(c, m),
    };
    for (cid, cd) in &c.cards {
        let mut best: Option<CardOption> = None;
        let mut why_all: Vec<String> = Vec::new();
        let base = CardOption {
            card: cid.clone(),
            server: cd.server.clone(),
            engine: String::new(),
            profile: String::new(),
            rank: usize::MAX,
            ok: false,
            need_gib: None,
            room_gib: cd.memory_gib().map(|v| round1(v - cd.reserve_gib)),
            restore: false,
            preset: None,
            why: Vec::new(),
        };
        if !cd.managed {
            out.push(CardOption {
                why: vec![format!("card {cid} is not managed by the router")],
                ..base
            });
            continue;
        }
        if !cd.on_demand {
            out.push(CardOption {
                why: vec![format!("card {cid} is not open to on-demand switches")],
                ..base
            });
            continue;
        }
        if !pin.cards.is_empty() && !pin.cards.iter().any(|x| x == cid) {
            out.push(CardOption {
                why: vec![format!(
                    "the route limits it to cards {}",
                    pin.cards.join(", ")
                )],
                ..base
            });
            continue;
        }
        let on_server = m.on_server(&cd.server);
        for (rank, eid) in engines.iter().enumerate() {
            let Some(e) = c.engines.get(eid) else {
                why_all.push(format!("{eid} is not set up as an engine"));
                continue;
            };
            let Some(a) = super::caps::artifact_for(c, m, eid) else {
                why_all.push(format!("{model} has no file {eid} can run"));
                continue;
            };
            if !has_port(c, eid, cid) {
                why_all.push(format!("{eid} has no port for card {cid}"));
                continue;
            }
            let restore = !on_server && a.restore.is_some();
            if !on_server && !restore {
                why_all.push(format!("the files are not on {}", cd.server));
                continue;
            }
            let mut warn = Vec::new();
            // the engines in `engines` match by capability unless a route pinned one
            if pin.pinned()
                && let Some(sm) = super::caps::supported(c, m)
                    .into_iter()
                    .find(|x| &x.engine == eid && !x.ok)
            {
                warn.push(format!("{eid}: {} (pinned by the route)", sm.reason));
            }
            if !card_allows(c, cid, kind, eid) {
                if pin.pinned() {
                    warn.push(format!(
                        "card {cid} does not list {eid} for {} (pinned by the route)",
                        kind.as_str()
                    ));
                } else {
                    why_all.push(format!(
                        "card {cid} does not allow {eid} for {}",
                        kind.label()
                    ));
                    continue;
                }
            }
            let (profiles, preset) = profiles_to_try(c, cid, eid, model, pin);
            if profiles.is_empty() {
                why_all.push(format!("{eid} has no engine profile to run it with"));
                continue;
            }
            let room = cd.memory_gib().map(|v| v - cd.reserve_gib);
            let mut chosen: Option<(String, f64, bool)> = None;
            for p in &profiles {
                let need = a.vram_gib + e.profiles[p].vram_gib;
                let fits = room.is_none_or(|r| need <= r + 1e-9);
                if fits {
                    chosen = Some((p.clone(), need, true));
                    break;
                }
                if chosen.is_none() {
                    chosen = Some((p.clone(), need, false));
                }
            }
            let (profile, need, fits) = chosen.expect("profiles is not empty");
            if !fits {
                let msg = format!(
                    "needs {:.1} GiB with {eid}/{profile}; card {cid} has {:.1} GiB free",
                    need,
                    room.unwrap_or(0.0)
                );
                if pin.pinned() {
                    warn.push(msg);
                } else {
                    why_all.push(msg);
                    continue;
                }
            }
            let opt = CardOption {
                engine: eid.clone(),
                profile: profile.clone(),
                rank,
                ok: true,
                need_gib: Some(round1(need)),
                restore,
                preset: preset.filter(|p| *p == profile),
                why: warn,
                ..base.clone()
            };
            if best.as_ref().is_none_or(|b| opt.rank < b.rank) {
                best = Some(opt);
            }
            break;
        }
        match best {
            Some(b) => out.push(b),
            None => out.push(CardOption {
                why: if why_all.is_empty() {
                    vec![format!("{model} lists no engine")]
                } else {
                    why_all
                },
                ..base
            }),
        }
    }
    out.sort_by(|a, b| b.ok.cmp(&a.ok).then(a.rank.cmp(&b.rank)));
    out
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

impl ModelKind {
    /// Plain words for the admin pages.
    pub fn label(self) -> &'static str {
        match self {
            ModelKind::Llm => "language models",
            ModelKind::Tts => "text to speech",
            ModelKind::Stt => "speech to text",
            ModelKind::Image => "image models",
            ModelKind::Music => "music models",
            ModelKind::Embedding => "embeddings",
            ModelKind::Video => "video models",
        }
    }
}

/// The on-demand choice per card for a route: card -> (engine, profile, rank), usable only.
pub fn route_picks(
    c: &ControlConfig,
    model: &str,
    od: Option<&RouteOnDemand>,
) -> IndexMap<String, (String, String, usize)> {
    place(c, model, &Pin::of(od))
        .into_iter()
        .filter(|o| o.ok)
        .map(|o| (o.card, (o.engine, o.profile, o.rank)))
        .collect()
}

// ── migration ────────────────────────────────────────────────────────────────────────────────

/// Is the main file already in the inventory model?
pub fn migrated(c: &ControlConfig) -> bool {
    c.inventory
        .as_ref()
        .is_some_and(|i| i.version >= INVENTORY_VERSION)
}

/// The edits that write today's implicit behaviour as inventory fields, for items defined in the
/// main file (`in_main`: `section.item` -> true when the main file defines it). Lossless: every
/// value is what the router assumed without it, so placement and switching are unchanged.
pub fn migrate_edits(
    c: &ControlConfig,
    in_main: &dyn Fn(&str) -> bool,
) -> Vec<super::super::control_api::form::Edit> {
    use super::super::control_api::form::Edit;
    let mut edits = Vec::new();
    let mut put = |path: &[&str], value: Value| {
        edits.push(Edit {
            path: path.iter().map(|s| s.to_string()).collect(),
            value,
        })
    };
    for (cid, cd) in &c.cards {
        if cd.engines.is_some() || !in_main(&format!("cards.{cid}")) {
            continue;
        }
        let llm: Vec<&str> = c
            .engines
            .iter()
            .filter(|(eid, _)| {
                super::caps::runs_kind(c, eid, ModelKind::Llm) && has_port(c, eid, cid)
            })
            .map(|(eid, _)| eid.as_str())
            .collect();
        put(&["cards", cid, "engines"], json!({ "llm": llm }));
    }
    let servers: Vec<&str> = c.servers.keys().map(String::as_str).collect();
    for (mid, m) in &c.models {
        if !in_main(&format!("models.{mid}")) {
            continue;
        }
        if m.kind.is_none() {
            put(&["models", mid, "kind"], json!("llm"));
        }
        if m.primary.is_none()
            && let Some(first) = m.artifacts.keys().next()
        {
            put(&["models", mid, "primary"], json!(first));
        }
        // Today's engine list (the artifacts) becomes primary + preferences; an engine that
        // runs it today but would not match by capability is allowed explicitly.
        if m.prefer.is_empty() && m.artifacts.len() > 1 {
            put(
                &["models", mid, "prefer"],
                json!(m.artifacts.keys().skip(1).collect::<Vec<_>>()),
            );
        }
        let ok: Vec<String> = super::caps::supported(c, m)
            .into_iter()
            .filter(|x| x.ok)
            .map(|x| x.engine)
            .collect();
        let mut allow: Vec<String> = m.allow.clone();
        for e in m.artifacts.keys() {
            if c.engines.contains_key(e) && !ok.contains(e) && !allow.contains(e) {
                allow.push(e.clone());
            }
        }
        if allow != m.allow {
            put(&["models", mid, "allow"], json!(allow));
        }
        if m.servers.is_none() {
            put(&["models", mid, "servers"], json!(servers));
        }
    }
    put(&["inventory"], json!({ "version": INVENTORY_VERSION }));
    edits
}

/// Migrate `text` (the main file at `path`): the new text, or None when nothing is to do.
pub fn migrate_text(path: &std::path::Path, text: &str) -> Result<Option<String>, String> {
    let l = super::load::load_with(path, text.to_string()).map_err(|i| {
        i.iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    if migrated(&l.config) {
        return Ok(None);
    }
    let main = l.main.clone();
    let in_main = |item: &str| l.origin.get(item).is_none_or(|p| *p == main);
    let edits = migrate_edits(&l.config, &in_main);
    let new = super::super::control_api::form::apply_edits(text, &edits)?;
    let l2 = super::load::load_with(path, new.clone()).map_err(|i| {
        i.iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    // refuse only errors the migration would add (a file with errors stays as it is otherwise)
    let errors = |l: &super::load::Loaded| -> Vec<String> {
        super::validate::validate_loaded(l)
            .into_iter()
            .filter(|i| i.level == super::validate::Level::Error)
            .map(|i| format!("{}: {}", i.path, i.message))
            .collect()
    };
    let before = errors(&l);
    let errs: Vec<String> = errors(&l2)
        .into_iter()
        .filter(|e| !before.contains(e))
        .collect();
    if !errs.is_empty() {
        return Err(format!(
            "the migrated file would not validate: {}",
            errs.join("; ")
        ));
    }
    Ok(Some(new))
}

/// Migrate the main file on disk if it needs it: back it up to
/// `<state_dir>/backups/router.toml.pre-inventory-<time>`, write atomically. Returns a line for
/// the start-up log. Failures leave the file as it was (the router runs on the defaults, which
/// behave the same).
pub fn migrate_file(main: &std::path::Path) -> String {
    let text = match std::fs::read_to_string(main) {
        Ok(t) => t,
        Err(e) => return format!("inventory: {}: {e}", main.display()),
    };
    let new = match migrate_text(main, &text) {
        Ok(None) => return String::new(),
        Ok(Some(n)) => n,
        Err(e) => {
            return format!("inventory: not migrated ({e}); the router uses the same defaults");
        }
    };
    let state_dir = super::load::load_with(main, text.clone())
        .map(|l| std::path::PathBuf::from(&l.config.router.state_dir))
        .unwrap_or_else(|_| main.parent().unwrap_or(std::path::Path::new(".")).into());
    let dir = state_dir.join("backups");
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let name = main
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("router.toml");
    let backup = dir.join(format!("{name}.pre-inventory-{stamp}"));
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&backup, &text)) {
        return format!(
            "inventory: not migrated (backup {}: {e}); the router uses the same defaults",
            backup.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o600));
    }
    let tmp = main.with_extension("toml.inventory-tmp");
    let perms = std::fs::metadata(main).map(|m| m.permissions()).ok();
    let w = std::fs::write(&tmp, &new).and_then(|_| {
        if let Some(p) = perms {
            let _ = std::fs::set_permissions(&tmp, p);
        }
        std::fs::rename(&tmp, main)
    });
    match w {
        Ok(()) => format!(
            "inventory: {} migrated to the inventory model (backup {})",
            main.display(),
            backup.display()
        ),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            format!("inventory: not migrated (write: {e}); the router uses the same defaults")
        }
    }
}

// ── the admin view ───────────────────────────────────────────────────────────────────────────

/// The inventory as the admin pages show it: servers with their cards and allowed engines, the
/// engine catalog, and every model with its engines, files and where it can run.
pub fn view(c: &ControlConfig) -> Value {
    let servers: Vec<Value> = c
        .servers
        .iter()
        .map(|(sid, s)| {
            let cards: Vec<Value> = c
                .cards
                .iter()
                .filter(|(_, cd)| cd.server == *sid)
                .map(|(cid, cd)| {
                    let allowed: serde_json::Map<String, Value> = ModelKind::ALL
                        .iter()
                        .map(|k| {
                            let list: Vec<&str> = match &cd.engines {
                                Some(m) => m
                                    .get(k.as_str())
                                    .map(|l| l.iter().map(String::as_str).collect())
                                    .unwrap_or_default(),
                                None if *k == ModelKind::Llm => c
                                    .engines
                                    .iter()
                                    .filter(|(e, _)| super::caps::runs_kind(c, e, ModelKind::Llm) && has_port(c, e, cid))
                                    .map(|(e, _)| e.as_str())
                                    .collect(),
                                None => Vec::new(),
                            };
                            (k.as_str().to_string(), json!(list))
                        })
                        .collect();
                    json!({
                        "id": cid, "label": cd.label, "gpuUuid": cd.gpu_uuid,
                        "vramGib": cd.vram_gib, "device": if cd.is_cpu() { "cpu" } else { "gpu" }, "ramGib": cd.ram_gib, "cores": cd.cores, "reserveGib": cd.reserve_gib,
                        "managed": cd.managed, "onDemand": cd.on_demand,
                        "explicit": cd.engines.is_some(), "engines": allowed,
                    })
                })
                .collect();
            json!({
                "id": sid, "title": s.title, "host": s.host,
                "exec": match s.exec { super::schema::ExecKind::Local => "local", super::schema::ExecKind::Ssh => "ssh", super::schema::ExecKind::Bridge => "bridge" },
                "modelDir": s.model_dir, "comfyRoot": s.comfy_root, "cards": cards,
                // `bridge:<id>`: read-only here, managed by that bridge (ROUTER-MEDIA-PLAN.md 13.4).
                "origin": s.origin,
            })
        })
        .collect();
    let engines: Vec<Value> = c
        .engines
        .iter()
        .map(|(eid, e)| {
            let cp = super::caps::caps(c, eid);
            let kinds: Vec<&str> = cp.kinds.iter().map(|k| k.as_str()).collect();
            let probe = super::caps::probe_of(c, eid);
            let ports: Vec<String> = match &e.port {
                PerCard::All(_) => vec!["*".into()],
                PerCard::Each(m) => m.keys().cloned().collect(),
            };
            json!({
                "id": eid, "title": e.title, "kinds": kinds, "driver": format!("{:?}", e.driver).to_lowercase(),
                "caps": cp, "overrides": e.caps, "probe": probe, "probeHow": super::caps::probe_how(c, eid),
                "usedBy": c.models.iter().filter(|(_, m)| super::caps::engine_order(c, m).contains(eid)).map(|(k, _)| k.clone()).collect::<Vec<_>>(),
                "image": e.image, "cards": ports,
                "profiles": e.profiles.iter().map(|(pid, p)| json!({"id": pid, "title": p.title, "lanes": p.lanes, "ctx": p.ctx, "vramGib": p.vram_gib})).collect::<Vec<_>>(),
            })
        })
        .collect();
    let models: Vec<Value> = c
        .models
        .iter()
        .map(|(mid, m)| {
            let order = super::caps::engine_order(c, m);
            let supported = super::caps::supported(c, m);
            let routes: Vec<Value> = c
                .routes
                .iter()
                .filter(|(_, r)| r.targets.iter().any(|t| t.model.as_deref() == Some(mid)))
                .map(|(name, r)| {
                    let cold = r.cold.unwrap_or(c.route_defaults.cold);
                    json!({
                        "name": name,
                        "onDemand": cold == ColdPolicy::OnDemand,
                        "auto": r.on_demand.as_ref().is_some_and(|o| o.layout_from == "auto"),
                        "pin": r.on_demand.as_ref().and_then(|o| o.pick.as_ref()).map(|p| json!({"engine": p.engine, "profile": p.profile})),
                    })
                })
                .collect();
            let pin_od = c
                .routes
                .values()
                .filter(|r| r.targets.iter().any(|t| t.model.as_deref() == Some(mid)))
                .find_map(|r| r.on_demand.as_ref());
            json!({
                "id": mid, "title": m.title, "description": m.description, "kind": m.kind().as_str(),
                "vision": m.vision, "primary": order.first(), "primaryChoice": m.primary, "prefer": m.prefer,
                "allow": m.allow, "deny": m.deny, "format": m.format, "arch": m.arch, "quant": m.quant,
                "order": order, "supported": supported,
                "files": m.artifacts.iter().map(|(k, a)| {
                    let d = super::caps::describe(m, a);
                    json!({"key": k, "file": a.file, "servedId": a.served_id, "vramGib": a.vram_gib,
                        "profiles": a.profiles, "defaultProfile": a.default_profile, "restore": a.restore.is_some(),
                        "format": d.format, "arch": d.arch, "quant": d.quant})
                }).collect::<Vec<_>>(),
                "servers": m.servers, "context": m.context, "sizeGib": m.size_gib, "licence": m.licence,
                "source": m.source.as_ref().map(|s| json!({"hf": s.hf, "revision": s.revision, "files": s.files, "gated": s.gated})),
                "routes": routes,
                "role": m.role, "needs": m.needs, "aliases": m.aliases,
                "present": component_presence(c, m),
                "needsMet": needs_met(c, m),
                "placement": place(c, mid, &Pin::default()),
                "pinned": pin_od.filter(|o| o.pick.is_some()).map(|o| place(c, mid, &Pin::of(Some(o)))),
            })
        })
        .collect();
    json!({
        "migrated": migrated(c),
        "catalog": super::caps::catalog_view(),
        "kinds": ModelKind::ALL.iter().map(|k| json!({"id": k.as_str(), "label": k.label()})).collect::<Vec<_>>(),
        "roles": super::schema::COMPONENT_ROLES,
        "servers": servers,
        "engines": engines,
        "models": models,
    })
}

/// The file a component model is (its first artifact's, else its first source file).
pub fn component_file(m: &super::schema::Model) -> Option<String> {
    m.artifacts
        .values()
        .map(|a| a.file.clone())
        .find(|f| !f.is_empty())
        .or_else(|| m.source.as_ref().and_then(|s| s.files.first().cloned()))
}

/// Per server: is this component's file there, by the last ComfyUI probe (`null`: not known).
/// Empty for a model that is not a component.
pub fn component_presence(
    c: &ControlConfig,
    m: &super::schema::Model,
) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    let (Some(role), Some(file)) = (
        m.role.as_deref().and_then(super::schema::component_role),
        component_file(m),
    ) else {
        return out;
    };
    for sid in c.servers.keys() {
        out.insert(
            sid.clone(),
            json!(super::caps::component_present(c, role, &file, sid)),
        );
    }
    out
}

/// Per server, for a model that needs components: the ones missing there (by probe, else by
/// `servers`). Empty when it needs nothing.
pub fn needs_met(c: &ControlConfig, m: &super::schema::Model) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    if m.needs.is_empty() {
        return out;
    }
    for sid in c.servers.keys() {
        let missing: Vec<&String> = m
            .needs
            .iter()
            .filter(|n| {
                let Some(x) = c.models.get(*n) else {
                    return true;
                };
                match component_presence(c, x).get(sid).and_then(Value::as_bool) {
                    Some(p) => !p,
                    None => !x.on_server(sid),
                }
            })
            .collect();
        out.insert(
            sid.clone(),
            json!({"ok": missing.is_empty(), "missing": missing}),
        );
    }
    out
}

#[cfg(test)]
mod tests;
