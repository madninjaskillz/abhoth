//! Validation (ROUTER-CONFIG.md 4.8). `abhoth router config check` and every UI save run the
//! same rules; each issue carries the TOML path and, once located, the file and line.
//!
//! Rules: references, uniqueness, capacity (VRAM), templates (rendered for every layout card),
//! safety (on-demand only on catalog-model routes, probes can never switch), secrets (values
//! never inline). Drift (what runs vs. the active layout) is reported elsewhere, not here.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::load::Loaded;
use super::schema::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Issue {
    pub level: Level,
    /// TOML path (`layouts.flash-x2.cards.A.profile`).
    pub path: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

impl Issue {
    pub fn err(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: Level::Error,
            path: path.into(),
            message: message.into(),
            file: None,
            line: None,
        }
    }
    pub fn warn(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: Level::Warning,
            path: path.into(),
            message: message.into(),
            file: None,
            line: None,
        }
    }
}

impl std::fmt::Display for Issue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lvl = match self.level {
            Level::Error => "error",
            Level::Warning => "warning",
        };
        match (&self.file, self.line) {
            (Some(file), Some(l)) => write!(f, "{lvl}: {file}:{l}: ")?,
            (Some(file), None) => write!(f, "{lvl}: {file}: ")?,
            _ => write!(f, "{lvl}: ")?,
        }
        if !self.path.is_empty() {
            write!(f, "{}: ", self.path)?;
        }
        f.write_str(&self.message)
    }
}

pub fn has_errors(issues: &[Issue]) -> bool {
    issues.iter().any(|i| i.level == Level::Error)
}

/// Quote a key for a TOML path when it contains dots or spaces.
pub fn key(k: &str) -> String {
    if k.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        k.to_string()
    } else {
        format!("\"{k}\"")
    }
}

/// Validate and attach file/line to each issue.
pub fn validate_loaded(l: &Loaded) -> Vec<Issue> {
    let mut out = validate(&l.config);
    for i in out.iter_mut() {
        if i.file.is_none() {
            let (f, line) = l.locate(&i.path);
            i.file = f;
            i.line = line;
        }
    }
    out
}

/// Every rule that needs only the config (no filesystem, no host).
pub fn validate(c: &ControlConfig) -> Vec<Issue> {
    let mut v = Vec::new();
    version(c, &mut v);
    servers(c, &mut v);
    cards(c, &mut v);
    engines(c, &mut v);
    models(c, &mut v);
    layouts(c, &mut v);
    routes(c, &mut v);
    upstreams(c, &mut v);
    guards(c, &mut v);
    inline_secrets(c, &mut v);
    templates(c, &mut v);
    v.sort_by(|a, b| b.level.cmp(&a.level).then(a.path.cmp(&b.path)));
    v
}

fn version(c: &ControlConfig, v: &mut Vec<Issue>) {
    if c.version == 0 || c.version > SCHEMA_VERSION {
        v.push(Issue::err(
            "version",
            format!("version must be {SCHEMA_VERSION}"),
        ));
    }
}

fn secret_ref(c: &ControlConfig, path: &str, r: &str, v: &mut Vec<Issue>) {
    match ControlConfig::secret_id(r) {
        None => v.push(Issue::err(
            path,
            format!("must be a secret reference \"secret:<id>\", not {r:?}"),
        )),
        Some(id) if !c.secrets.contains_key(id) => v.push(Issue::err(
            path,
            format!("secret `{id}` is not defined in [secrets]"),
        )),
        Some(_) => {}
    }
}

fn servers(c: &ControlConfig, v: &mut Vec<Issue>) {
    for (id, s) in &c.secrets {
        let p = format!("secrets.{}", key(id));
        match (&s.file, &s.env) {
            (Some(_), Some(_)) => v.push(Issue::err(&p, "give `file` or `env`, not both")),
            (None, None) => v.push(Issue::err(
                &p,
                "give `file = \"/path\"` or `env = \"NAME\"`",
            )),
            _ => {}
        }
    }
    for (id, s) in &c.servers {
        let p = format!("servers.{}", key(id));
        if s.host.trim().is_empty() {
            v.push(Issue::err(format!("{p}.host"), "host is empty"));
        }
        match (s.exec, &s.ssh) {
            (ExecKind::Ssh, None) => v.push(Issue::err(
                &p,
                "exec = \"ssh\" needs an `ssh = { user, key, host_key }` table",
            )),
            (ExecKind::Ssh, Some(ssh)) => {
                secret_ref(c, &format!("{p}.ssh.key"), &ssh.key, v);
                if !(ssh.host_key.starts_with("SHA256:") || ssh.host_key.starts_with("ssh-")) {
                    v.push(Issue::err(
                        format!("{p}.ssh.host_key"),
                        "pin the host key: a `SHA256:…` fingerprint or an `ssh-ed25519 AAAA…` line (never trust on first use)",
                    ));
                }
                if ssh.user == "root" {
                    v.push(Issue::warn(
                        format!("{p}.ssh.user"),
                        "use a dedicated user (abhoth-exec) with the forced command, not root",
                    ));
                }
            }
            (ExecKind::Local, Some(_)) => {
                v.push(Issue::warn(format!("{p}.ssh"), "ignored: exec = \"local\""))
            }
            _ => {}
        }
        if s.hostctl == HostctlMode::Trusted {
            v.push(Issue::warn(
                format!("{p}.hostctl"),
                "TRUSTED mode: the router sends command text to this host; use only for development hosts",
            ));
        }
        if let Some(st) = &s.state
            && let Some(l) = &st.initial_layout
            && !c.layouts.contains_key(l)
        {
            v.push(Issue::err(
                format!("{p}.state.initial_layout"),
                format!("no layout `{l}`"),
            ));
        }
        if let Some(pc) = &s.post_check
            && !c.routes.contains_key(&pc.route)
        {
            v.push(Issue::err(
                format!("{p}.post_check.route"),
                format!("no route `{}`", pc.route),
            ));
        }
        for (i, h) in s.ready.iter().enumerate() {
            hook(c, &format!("{p}.ready.{i}"), h, Some(s), None, v);
        }
    }
}

fn hook(
    c: &ControlConfig,
    p: &str,
    h: &Hook,
    server: Option<&Server>,
    engine: Option<&Engine>,
    v: &mut Vec<Issue>,
) {
    let _ = c;
    match (&h.builtin, &h.command) {
        (Some(_), Some(_)) => v.push(Issue::err(p, "a hook is `builtin` or `command`, not both")),
        (None, None) => v.push(Issue::err(
            p,
            "a hook needs `builtin = \"…\"` or `command = \"…\"`",
        )),
        (Some(b), None) => {
            if !super::builtins::known(b) {
                v.push(Issue::err(
                    format!("{p}.builtin"),
                    format!(
                        "unknown builtin `{b}` (known: {})",
                        super::builtins::NAMES.join(", ")
                    ),
                ));
            }
            let need: &[&str] = match b.as_str() {
                "numa-room" => &["node", "need_gib"],
                "json-set" => &["file", "key", "value"],
                "mounted" | "file-exists" => &["path"],
                _ => &[],
            };
            for k in need {
                if !h.params.contains_key(*k) {
                    v.push(Issue::err(p, format!("builtin `{b}` needs `{k}`")));
                }
            }
        }
        (None, Some(cmd)) => {
            let named = server.is_some_and(|s| s.templates.contains_key(cmd))
                || engine.and_then(|e| e.templates.as_ref()).is_some();
            // Anything that is not a named template is template text; check it parses.
            if !named && let Err(e) = super::template::variables(cmd) {
                v.push(Issue::err(format!("{p}.command"), e.to_string()));
            }
        }
    }
}

fn is_uuid(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("GPU-") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(n, p)| p.len() == *n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn cards(c: &ControlConfig, v: &mut Vec<Issue>) {
    let mut uuids: HashMap<String, &str> = HashMap::new();
    for (id, card) in &c.cards {
        let p = format!("cards.{}", key(id));
        if !c.servers.contains_key(&card.server) {
            v.push(Issue::err(
                format!("{p}.server"),
                format!("no server `{}`", card.server),
            ));
        }
        if !is_uuid(&card.gpu_uuid) {
            v.push(Issue::err(format!("{p}.gpu_uuid"), format!("{:?} is not a GPU UUID (GPU-xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx; see nvidia-smi -L)", card.gpu_uuid)));
        }
        if let Some(other) = uuids.insert(card.gpu_uuid.to_ascii_lowercase(), id) {
            v.push(Issue::err(
                format!("{p}.gpu_uuid"),
                format!("same GPU as card `{other}`"),
            ));
        }
        if card.managed && card.vram_gib.is_none() {
            v.push(Issue::err(
                format!("{p}.vram_gib"),
                "a managed card needs vram_gib (for the VRAM check)",
            ));
        }
        if !card.managed && card.on_demand {
            v.push(Issue::err(
                format!("{p}.on_demand"),
                "an unmanaged card can never be switched",
            ));
        }
    }
}

fn engines(c: &ControlConfig, v: &mut Vec<Issue>) {
    for (id, e) in &c.engines {
        let p = format!("engines.{}", key(id));
        if let Some(k) = &e.key {
            secret_ref(c, &format!("{p}.key"), k, v);
        }
        if e.key_delivery.mode == KeyMode::Argv {
            v.push(Issue::warn(
                format!("{p}.key_delivery.mode"),
                "argv: the engine key is visible in `ps` and `docker inspect`; use file_flag (or wrapper until the engine has a key-file flag)",
            ));
        }
        if e.key.is_some() && e.key_delivery.mode == KeyMode::None {
            v.push(Issue::warn(
                format!("{p}.key_delivery.mode"),
                "a key is set but never delivered to the engine",
            ));
        }
        for card in e.port.cards() {
            if !c.cards.contains_key(card) {
                v.push(Issue::err(
                    format!("{p}.port.{card}"),
                    format!("no card `{card}`"),
                ));
            }
        }
        match e.driver {
            DriverKind::Docker => {
                if e.image.is_none() {
                    v.push(Issue::err(
                        format!("{p}.image"),
                        "driver = \"docker\" needs an image",
                    ));
                }
                if e.container.is_none() {
                    v.push(Issue::err(
                        format!("{p}.container"),
                        "driver = \"docker\" needs a container name template",
                    ));
                }
                if e.run.is_none() {
                    v.push(Issue::err(
                        format!("{p}.run"),
                        "driver = \"docker\" needs [engines.x.run] with the docker words",
                    ));
                }
            }
            DriverKind::Compose if e.compose.is_none() => v.push(Issue::err(
                format!("{p}.compose"),
                "driver = \"compose\" needs compose = { project, file, service, container }",
            )),
            DriverKind::Systemd if e.systemd.is_none() => v.push(Issue::err(
                format!("{p}.systemd"),
                "driver = \"systemd\" needs systemd = { unit }",
            )),
            DriverKind::Process if e.process.is_none() => v.push(Issue::err(
                format!("{p}.process"),
                "driver = \"process\" needs process = { command }",
            )),
            DriverKind::Template if e.templates.is_none() => v.push(Issue::err(
                format!("{p}.templates"),
                "driver = \"template\" needs templates = { start, stop, status }",
            )),
            _ => {}
        }
        if let Some(b) = &e.default_build
            && !e.builds.contains_key(b)
        {
            v.push(Issue::err(
                format!("{p}.default_build"),
                format!("no build `{b}`"),
            ));
        }
        for peer in &e.restart_if_peer_starts {
            if !c.engines.contains_key(peer) {
                v.push(Issue::err(
                    format!("{p}.restart_if_peer_starts"),
                    format!("no engine `{peer}`"),
                ));
            }
        }
        for (i, h) in e.before_start.iter().enumerate() {
            hook(c, &format!("{p}.before_start.{i}"), h, None, Some(e), v);
        }
        for (i, h) in e.prepare.iter().enumerate() {
            hook(c, &format!("{p}.prepare.{i}"), h, None, Some(e), v);
        }
        let fixed: BTreeSet<&str> = e
            .run
            .as_ref()
            .map(|r| r.fixed.iter().map(String::as_str).collect())
            .unwrap_or_default();
        for (pid, prof) in &e.profiles {
            let pp = format!("{p}.profiles.{}", key(pid));
            if prof.lanes == 0 {
                v.push(Issue::err(
                    format!("{pp}.lanes"),
                    "lanes must be at least 1",
                ));
            }
            if let (Some(ctx), Some(max)) = (prof.ctx, e.max_context)
                && ctx > max
            {
                v.push(Issue::err(
                    format!("{pp}.ctx"),
                    format!("ctx {ctx} is over the engine's max_context {max}"),
                ));
            }
            if let Some(b) = &prof.build
                && !e.builds.contains_key(b)
            {
                v.push(Issue::err(
                    format!("{pp}.build"),
                    format!("engine `{id}` has no build `{b}`"),
                ));
            }
            for k in prof.options.keys() {
                if fixed.contains(k.as_str()) {
                    v.push(Issue::err(
                        format!("{pp}.options.{}", key(k)),
                        format!("`{k}` is fixed by the engine and cannot be set by a profile"),
                    ));
                }
            }
        }
    }
}

fn models(c: &ControlConfig, v: &mut Vec<Issue>) {
    for (mid, m) in &c.models {
        for (eid, a) in &m.artifacts {
            let p = format!("models.{}.artifacts.{}", key(mid), key(eid));
            let Some(e) = c.engines.get(eid) else {
                v.push(Issue::err(&p, format!("no engine `{eid}`")));
                continue;
            };
            for prof in &a.profiles {
                if !e.profiles.contains_key(prof) {
                    v.push(Issue::err(
                        format!("{p}.profiles"),
                        format!("engine `{eid}` has no profile `{prof}`"),
                    ));
                }
            }
            let fixed: BTreeSet<&str> = e
                .run
                .as_ref()
                .map(|r| r.fixed.iter().map(String::as_str).collect())
                .unwrap_or_default();
            for k in a.options.keys() {
                if fixed.contains(k.as_str()) {
                    v.push(Issue::err(
                        format!("{p}.options.{}", key(k)),
                        format!("`{k}` is fixed by the engine"),
                    ));
                }
            }
        }
    }
}

fn layouts(c: &ControlConfig, v: &mut Vec<Issue>) {
    let mut alias_owner: HashMap<&str, &str> = HashMap::new();
    for (lid, l) in &c.layouts {
        let p = format!("layouts.{}", key(lid));
        let mut first = true;
        let mut ports: BTreeMap<(String, u16), String> = BTreeMap::new();
        let mut servers_seen = BTreeSet::new();
        for (card_id, lc) in &l.cards {
            let cp = format!("{p}.cards.{}", key(card_id));
            let Some(card) = c.cards.get(card_id) else {
                v.push(Issue::err(&cp, format!("no card `{card_id}`")));
                continue;
            };
            servers_seen.insert(card.server.clone());
            if !card.managed && !matches!(lc, LayoutCard::Word(w) if w == "off") {
                v.push(Issue::err(
                    &cp,
                    format!("card `{card_id}` is not managed (managed = false)"),
                ));
            }
            match lc {
                LayoutCard::Word(w) if w == "off" => {}
                LayoutCard::Word(w) if w == "same" => {
                    if first {
                        v.push(Issue::err(&cp, "\"same\" needs an earlier card to copy"));
                    }
                }
                LayoutCard::Word(w) => v.push(Issue::err(
                    &cp,
                    format!("{w:?}: use \"off\", \"same\" or {{ engine, profile, model }}"),
                )),
                LayoutCard::Spec(_) => {}
            }
            first = false;
        }
        for (card_id, spec) in resolve_cards(l) {
            let Some(s) = spec else { continue };
            let Some(card) = c.cards.get(&card_id) else {
                continue;
            };
            let cp = format!("{p}.cards.{}", key(&card_id));
            let Some(e) = c.engines.get(&s.engine) else {
                v.push(Issue::err(
                    format!("{cp}.engine"),
                    format!("no engine `{}`", s.engine),
                ));
                continue;
            };
            let prof = e.profiles.get(&s.profile);
            if prof.is_none() {
                v.push(Issue::err(
                    format!("{cp}.profile"),
                    format!("engine `{}` has no profile `{}`", s.engine, s.profile),
                ));
            }
            let art = c
                .models
                .get(&s.model)
                .and_then(|m| m.artifacts.get(&s.engine));
            match (c.models.get(&s.model), art) {
                (None, _) => v.push(Issue::err(
                    format!("{cp}.model"),
                    format!("no model `{}`", s.model),
                )),
                (Some(_), None) => v.push(Issue::err(
                    format!("{cp}.model"),
                    format!(
                        "model `{}` has no artifact for engine `{}`",
                        s.model, s.engine
                    ),
                )),
                (Some(_), Some(a)) => {
                    if !a.profiles.is_empty() && !a.profiles.contains(&s.profile) {
                        v.push(Issue::warn(
                            format!("{cp}.profile"),
                            format!(
                                "model `{}` does not list profile `{}` as fitting",
                                s.model, s.profile
                            ),
                        ));
                    }
                }
            }
            match e.port.get(&card_id) {
                None => v.push(Issue::err(
                    format!("{cp}.engine"),
                    format!("engine `{}` has no port for card `{card_id}`", s.engine),
                )),
                Some(port) => {
                    if let Some(other) = ports.insert((card.server.clone(), *port), card_id.clone())
                    {
                        v.push(Issue::err(
                            &cp,
                            format!(
                                "port {port} on server `{}` is also used by card `{other}`",
                                card.server
                            ),
                        ));
                    }
                }
            }
            // Capacity: artifact + profile <= card VRAM - reserve.
            if let (Some(vram), Some(prof), Some(a)) = (card.vram_gib, prof, art) {
                let need = a.vram_gib + prof.vram_gib;
                let have = vram - card.reserve_gib;
                if need > have + 1e-9 {
                    let msg = format!(
                        "needs {need:.1} GiB (model {:.1} + profile {:.1}) but card `{card_id}` has {have:.1} GiB ({vram:.1} - {:.1} reserve)",
                        a.vram_gib, prof.vram_gib, card.reserve_gib
                    );
                    v.push(if l.force {
                        Issue::warn(&cp, format!("{msg}; allowed by force = true"))
                    } else {
                        Issue::err(&cp, msg)
                    });
                }
            }
        }
        for a in &l.aliases {
            if let Some(o) = alias_owner.insert(a, lid) {
                v.push(Issue::err(
                    format!("{p}.aliases"),
                    format!("alias `{a}` is also used by layout `{o}`"),
                ));
            }
            if c.layouts.contains_key(a) {
                v.push(Issue::err(
                    format!("{p}.aliases"),
                    format!("alias `{a}` is the name of a layout"),
                ));
            }
        }
    }
    // Container names unique per server across engines (a name template that does not include
    // the card would make two cards fight over one container).
    for (eid, e) in &c.engines {
        if let Some(t) = &e.container
            && !t.contains("{card")
            && e.driver == DriverKind::Docker
        {
            v.push(Issue::err(
                format!("engines.{}.container", key(eid)),
                "the container name must include {card} or {card_lc} (one container per card)",
            ));
        }
    }
}

fn principal_access_ok(a: &str) -> bool {
    matches!(a, "users" | "admins" | "anonymous")
        || a.strip_prefix("keys:").is_some_and(|k| !k.is_empty())
}

fn routes(c: &ControlConfig, v: &mut Vec<Issue>) {
    if !principal_access_ok(&c.route_defaults.access) {
        v.push(Issue::err(
            "route_defaults.access",
            "users | admins | anonymous | keys:<list>",
        ));
    }
    let mut lower: HashMap<String, &str> = HashMap::new();
    let mut reachable = false;
    for (name, r) in &c.routes {
        let p = format!("routes.{}", key(name));
        if let Some(o) = lower.insert(name.to_lowercase(), name) {
            v.push(Issue::err(
                &p,
                format!("route names are case-insensitive: clashes with `{o}`"),
            ));
        }
        if let Some(a) = &r.access
            && !principal_access_ok(a)
        {
            v.push(Issue::err(
                format!("{p}.access"),
                "users | admins | anonymous | keys:<list>",
            ));
        }
        let access = r.access.as_deref().unwrap_or(&c.route_defaults.access);
        if !r.hidden && matches!(access, "users" | "anonymous") {
            reachable = true;
        }
        match (&r.same_as, r.targets.is_empty()) {
            (Some(s), _) => {
                if !r.targets.is_empty() {
                    v.push(Issue::err(&p, "`same_as` and `targets` exclude each other"));
                }
                // Follow the chain; refuse cycles and dangling names.
                let mut seen = BTreeSet::from([name.as_str()]);
                let mut cur = s.as_str();
                loop {
                    let Some(next) = c.routes.get(cur) else {
                        v.push(Issue::err(
                            format!("{p}.same_as"),
                            format!("no route `{cur}`"),
                        ));
                        break;
                    };
                    if !seen.insert(cur) {
                        v.push(Issue::err(
                            format!("{p}.same_as"),
                            format!("`same_as` cycle through `{cur}`"),
                        ));
                        break;
                    }
                    match &next.same_as {
                        Some(n) => cur = n,
                        None => break,
                    }
                }
            }
            (None, true) => v.push(Issue::err(&p, "a route needs `targets` or `same_as`")),
            (None, false) => {}
        }
        let mut only_models = !r.targets.is_empty();
        for (i, t) in r.targets.iter().enumerate() {
            let tp = format!("{p}.targets.{i}");
            let kinds = [
                t.pool.is_some(),
                t.card.is_some(),
                t.upstream.is_some(),
                t.model.is_some() && t.upstream.is_none(),
            ];
            if kinds.iter().filter(|x| **x).count() != 1 {
                v.push(Issue::err(&tp, "a target is exactly one of { pool }, { card }, { model } or { upstream, model }"));
                continue;
            }
            if t.model.is_none() || t.upstream.is_some() {
                only_models = false;
            }
            if let Some(pool) = &t.pool
                && pool != "live"
            {
                v.push(Issue::err(
                    format!("{tp}.pool"),
                    "the only pool is \"live\"",
                ));
            }
            if let Some(e) = &t.engine
                && !c.engines.contains_key(e)
            {
                v.push(Issue::err(
                    format!("{tp}.engine"),
                    format!("no engine `{e}`"),
                ));
            }
            if let Some(card) = &t.card
                && !c.cards.contains_key(card)
            {
                v.push(Issue::err(
                    format!("{tp}.card"),
                    format!("no card `{card}`"),
                ));
            }
            match (&t.upstream, &t.model) {
                (Some(u), m) => {
                    if !c.upstreams.contains_key(u) {
                        v.push(Issue::err(
                            format!("{tp}.upstream"),
                            format!("no upstream `{u}`"),
                        ));
                    }
                    if m.is_none() {
                        v.push(Issue::err(
                            &tp,
                            "an upstream target needs the `model` id the upstream expects",
                        ));
                    }
                }
                (None, Some(m)) if !c.models.contains_key(m) => v.push(Issue::err(
                    format!("{tp}.model"),
                    format!("no catalog model `{m}`"),
                )),
                _ => {}
            }
        }
        if r.targets.first().is_some_and(|t| t.fallback) {
            v.push(Issue::warn(
                format!("{p}.targets.0.fallback"),
                "the first target is never a fallback",
            ));
        }
        let cold = r.cold.unwrap_or(c.route_defaults.cold);
        for f in &r.fallback {
            if !c.routes.contains_key(f) {
                v.push(Issue::err(
                    format!("{p}.fallback"),
                    format!("no route `{f}`"),
                ));
            } else if f == name {
                v.push(Issue::err(
                    format!("{p}.fallback"),
                    "a route cannot fall back to itself",
                ));
            }
        }
        if cold == ColdPolicy::Fallback && r.fallback.is_empty() {
            v.push(Issue::err(
                format!("{p}.cold"),
                "cold = \"fallback\" needs `fallback = [routes…]`",
            ));
        }
        // Safety: generic and pinned names never switch (7.2, 7.4).
        if cold == ColdPolicy::OnDemand {
            if !only_models {
                v.push(Issue::err(
                    format!("{p}.cold"),
                    "on_demand is only allowed on routes whose targets are all catalog models (never pool, card or upstream targets)",
                ));
            }
            let od = r.on_demand.clone().unwrap_or_default();
            if od.cards.is_empty() {
                v.push(Issue::err(
                    format!("{p}.on_demand.cards"),
                    "list the cards an on-demand switch may use",
                ));
            }
            for card in &od.cards {
                match c.cards.get(card) {
                    None => v.push(Issue::err(
                        format!("{p}.on_demand.cards"),
                        format!("no card `{card}`"),
                    )),
                    Some(cd) if !cd.managed || !cd.on_demand => v.push(Issue::err(
                        format!("{p}.on_demand.cards"),
                        format!("card `{card}` must be managed with on_demand = true"),
                    )),
                    _ => {}
                }
            }
            match &od.pick {
                None => v.push(Issue::err(
                    format!("{p}.on_demand.pick"),
                    "on-demand needs pick = { engine, profile }",
                )),
                Some(pk) => {
                    let e = c.engines.get(&pk.engine);
                    if e.is_none() {
                        v.push(Issue::err(
                            format!("{p}.on_demand.pick.engine"),
                            format!("no engine `{}`", pk.engine),
                        ));
                    }
                    if let (Some(e), Some(pr)) = (e, &pk.profile)
                        && !e.profiles.contains_key(pr)
                    {
                        v.push(Issue::err(
                            format!("{p}.on_demand.pick.profile"),
                            format!("engine `{}` has no profile `{pr}`", pk.engine),
                        ));
                    }
                }
            }
            let sw = od.access.as_deref().unwrap_or("admins");
            if matches!(sw, "users" | "anonymous") && !r.allow_user_switch {
                v.push(Issue::err(
                    format!("{p}.on_demand.access"),
                    "users may only cause switches when the route sets allow_user_switch = true; anonymous callers never switch",
                ));
            }
            if sw == "anonymous" {
                v.push(Issue::err(
                    format!("{p}.on_demand.access"),
                    "anonymous callers never switch",
                ));
            }
        } else if r.on_demand.is_some() {
            v.push(Issue::warn(
                format!("{p}.on_demand"),
                "ignored unless cold = \"on_demand\"",
            ));
        }
    }
    // Route names may not collide with other models' served ids unless same_as says so.
    let routes_lc: HashMap<String, &String> =
        c.routes.keys().map(|k| (k.to_lowercase(), k)).collect();
    for (mid, m) in &c.models {
        for a in m.artifacts.values() {
            if let Some(sid) = &a.served_id
                && let Some(rn) = routes_lc.get(&sid.to_lowercase())
            {
                let r = &c.routes[*rn];
                let points_here = r
                    .targets
                    .iter()
                    .any(|t| t.model.as_deref() == Some(mid.as_str()) && t.upstream.is_none())
                    || r.same_as.is_some()
                    || r.targets
                        .iter()
                        .any(|t| t.pool.is_some() || t.card.is_some());
                if !points_here {
                    v.push(Issue::err(
                        format!("routes.{}", key(rn)),
                        format!(
                            "route name is model `{mid}`'s served id but targets something else"
                        ),
                    ));
                }
            }
        }
    }
    if !c.routes.is_empty() && !reachable {
        v.push(Issue::err(
            "routes",
            "no route is reachable by users (access = \"users\" and not hidden)",
        ));
    }
}

fn upstreams(c: &ControlConfig, v: &mut Vec<Issue>) {
    for (id, u) in &c.upstreams {
        let p = format!("upstreams.{}", key(id));
        if u.kind == UpstreamKind::Bridge {
            if u.enabled {
                v.push(Issue::err(
                    format!("{p}.enabled"),
                    "bridge upstreams are reserved (BRIDGE.md 10.3): set enabled = false",
                ));
            }
            continue;
        }
        if !(u.base_url.starts_with("http://") || u.base_url.starts_with("https://")) {
            v.push(Issue::err(
                format!("{p}.base_url"),
                "base_url must be http(s)://…",
            ));
        }
        for (i, k) in u.key.iter().chain(u.keys.iter()).enumerate() {
            secret_ref(
                c,
                &format!(
                    "{p}.key{}",
                    if i == 0 {
                        String::new()
                    } else {
                        format!("s.{}", i - 1)
                    }
                ),
                k,
                v,
            );
        }
        if u.kind == UpstreamKind::Router {
            let fl = u.flavour.as_deref().unwrap_or("abhoth");
            if !matches!(fl, "abhoth" | "litellm") {
                v.push(Issue::err(format!("{p}.flavour"), "abhoth | litellm"));
            }
            if fl == "litellm"
                && u.health_path
                    .as_deref()
                    .is_some_and(|h| h.trim_end_matches('/') == "/health")
            {
                v.push(Issue::err(
                    format!("{p}.health_path"),
                    "never probe LiteLLM's /health: it sends real chats to every deployment (use /health/liveliness)",
                ));
            }
        }
    }
    for (i, b) in c.budgets.iter().enumerate() {
        let p = format!("budgets.{i}");
        let ok = b.scope == "global"
            || ["user:", "key:", "route:"]
                .iter()
                .any(|pre| b.scope.strip_prefix(pre).is_some_and(|r| !r.is_empty()));
        if !ok {
            v.push(Issue::err(
                format!("{p}.scope"),
                "user:<name> | user:* | key:<id> | route:<name> | global",
            ));
        }
        if b.usd.is_some() && !matches!(b.period.as_deref(), Some("day" | "week" | "month")) {
            v.push(Issue::err(
                format!("{p}.period"),
                "a usd budget needs period = day | week | month",
            ));
        }
    }
}

fn hhmm(s: &str) -> bool {
    if s.len() != 5 {
        return false;
    }
    let Some((h, m)) = s.split_once(':') else {
        return false;
    };
    matches!((h.parse::<u32>(), m.parse::<u32>()), (Ok(h), Ok(m)) if h < 24 && m < 60)
}

fn guards(c: &ControlConfig, v: &mut Vec<Issue>) {
    for (id, g) in &c.guards {
        let p = format!("guards.{}", key(id));
        if g.blocks.is_empty() {
            v.push(Issue::warn(format!("{p}.blocks"), "blocks nothing"));
        }
        if let Some(s) = &g.server
            && !c.servers.contains_key(s)
        {
            v.push(Issue::err(
                format!("{p}.server"),
                format!("no server `{s}`"),
            ));
        }
        let host_kind = matches!(
            g.kind,
            GuardKind::Process | GuardKind::JsonFile | GuardKind::FileExists
        );
        if host_kind && g.server.is_none() {
            v.push(Issue::err(
                format!("{p}.server"),
                "this guard kind reads a host: name its server",
            ));
        }
        match g.kind {
            GuardKind::Schedule => {
                if g.windows.is_empty() {
                    v.push(Issue::err(
                        format!("{p}.windows"),
                        "give windows = [\"HH:MM-HH:MM\"]",
                    ));
                }
                for w in &g.windows {
                    let ok = w.split_once('-').is_some_and(|(a, b)| hhmm(a) && hhmm(b));
                    if !ok {
                        v.push(Issue::err(
                            format!("{p}.windows"),
                            format!("{w:?} is not HH:MM-HH:MM"),
                        ));
                    }
                }
            }
            GuardKind::Process if g.matcher.is_none() => v.push(Issue::err(
                format!("{p}.match"),
                "a process guard needs match = { argv_any, … }",
            )),
            GuardKind::JsonFile | GuardKind::FileExists if g.path.is_none() => {
                v.push(Issue::err(format!("{p}.path"), "give the path"))
            }
            GuardKind::Http if g.url.is_none() => {
                v.push(Issue::err(format!("{p}.url"), "give the url"))
            }
            GuardKind::Lease if g.reason.is_none() => v.push(Issue::err(
                format!("{p}.reason"),
                "a lease guard needs the lease reason (bench, video, …)",
            )),
            _ => {}
        }
    }
}

/// Does a string look like a credential rather than a reference? (4.7)
pub fn looks_like_secret(s: &str) -> bool {
    let t = s.trim();
    if t.starts_with("secret:") {
        return false;
    }
    if t.starts_with("sk-") && t.len() >= 20
        || t.starts_with("abk_")
        || t.starts_with("Bearer ")
        || t.starts_with("sk-ant-")
    {
        return true;
    }
    if t.contains("-----BEGIN") {
        return true;
    }
    // A long unbroken base64/hex run (40+), not a path, URL, UUID or hash-pinned host key.
    let long_run = t
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '+' || c == '=' || c == '_'))
        .any(|w| {
            w.len() >= 40
                && w.chars().any(|c| c.is_ascii_digit())
                && w.chars().any(|c| c.is_ascii_alphabetic())
        });
    long_run
        && !t.starts_with("SHA256:")
        && !t.starts_with("ssh-")
        && !t.contains("://")
        && !t.starts_with('/')
}

fn inline_secrets(c: &ControlConfig, v: &mut Vec<Issue>) {
    fn walk(path: &str, val: &Value, v: &mut Vec<Issue>) {
        match val {
            Value::String(s) if s.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\t')) => {
                v.push(Issue::err(path, "control characters are not allowed in config values"))
            }
            Value::String(s) if looks_like_secret(s) => v.push(Issue::err(
                path,
                "this looks like a key or token; put it in a file or env var and reference it as \"secret:<id>\"",
            )),
            Value::Object(m) => {
                for (k, x) in m {
                    walk(&if path.is_empty() { key(k) } else { format!("{path}.{}", key(k)) }, x, v);
                }
            }
            Value::Array(a) => {
                for (i, x) in a.iter().enumerate() {
                    walk(&format!("{path}.{i}"), x, v);
                }
            }
            _ => {}
        }
    }
    if let Ok(val) = serde_json::to_value(c) {
        walk("", &val, v);
    }
}

/// Render every start/stop/status command for every (layout, card): unknown variables,
/// unbalanced braces and bad option values fail here, not at run time.
fn templates(c: &ControlConfig, v: &mut Vec<Issue>) {
    for (sid, s) in &c.servers {
        for (tid, t) in &s.templates {
            if let Err(e) = super::template::variables(t) {
                v.push(Issue::err(
                    format!("servers.{}.templates.{}", key(sid), key(tid)),
                    e.to_string(),
                ));
            }
        }
    }
    if v.iter().any(|i| i.level == Level::Error) {
        return; // references are broken; rendering would only repeat them
    }
    for (lid, l) in &c.layouts {
        for (card, spec) in resolve_cards(l) {
            let Some(spec) = spec else { continue };
            let p = format!("layouts.{}.cards.{}", key(lid), key(&card));
            if let Err(e) = super::resolve::start_commands(c, &card, &spec) {
                v.push(Issue::err(&p, format!("does not render: {e}")));
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
