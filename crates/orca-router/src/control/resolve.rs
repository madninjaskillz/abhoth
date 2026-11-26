//! Resolving a (card, spec) into the template variables of ROUTER-CONFIG.md 4.5 and the rendered
//! pieces a driver needs: the engine's options (layoutctl `ninfer_cli` / `llama_cli`), the
//! `layouts.spec` label (layoutctl `norm_card`, so rollback and `detect()` recognise our
//! containers), mounts, env, the key delivery.

use std::collections::BTreeMap;

use indexmap::IndexMap;
use serde_json::Value;

use super::driver::{Cmd, InstanceRef, Resolved};
use super::drivers;
use super::schema::*;
use super::template::{self, TplValue, Vars, raw};

/// Variables every command for `card` (with `engine`) may use: server, host, card, card_lc,
/// gpu_uuid, cpus, engine, port, container/name, `var:*`, `secret_file:*`, state_dir.
pub fn base_vars(c: &ControlConfig, card_id: &str, engine_id: &str) -> Result<Vars, String> {
    let card = c
        .cards
        .get(card_id)
        .ok_or_else(|| format!("no card `{card_id}`"))?;
    let server = c
        .servers
        .get(&card.server)
        .ok_or_else(|| format!("no server `{}`", card.server))?;
    let engine = c
        .engines
        .get(engine_id)
        .ok_or_else(|| format!("no engine `{engine_id}`"))?;
    let mut v = Vars::new();
    let w = |s: &str| TplValue::Word(s.to_string());
    v.insert("server".into(), w(&card.server));
    v.insert("host".into(), w(&server.host));
    v.insert("card".into(), w(card_id));
    v.insert("card_lc".into(), w(&card_id.to_lowercase()));
    v.insert("gpu_uuid".into(), w(&card.gpu_uuid));
    v.insert("cpus".into(), w(card.cpus.as_deref().unwrap_or("")));
    v.insert("engine".into(), w(engine_id));
    v.insert("state_dir".into(), w(&c.router.state_dir));
    if let Some(p) = engine.port.get(card_id) {
        v.insert("port".into(), w(&p.to_string()));
    }
    for (k, val) in &server.vars {
        v.insert(format!("var:{k}"), w(val));
    }
    for id in c.secrets.keys() {
        v.insert(
            format!("secret_file:{id}"),
            w(&secret_host_path(engine, id)),
        );
    }
    // Names may use the vars above (`ninfer-{card_lc}`).
    let name = instance_name(c, engine, card_id, &v)?;
    v.insert("container".into(), w(&name));
    v.insert("name".into(), w(&name));
    Ok(v)
}

/// Where hostctl `secret-put` writes a secret on the host.
pub fn secret_host_path(engine: &Engine, id: &str) -> String {
    format!(
        "{}/{}",
        engine.key_delivery.host_dir.trim_end_matches('/'),
        id
    )
}

fn instance_name(c: &ControlConfig, e: &Engine, card: &str, v: &Vars) -> Result<String, String> {
    let _ = c;
    let t = match e.driver {
        DriverKind::Docker => e
            .container
            .clone()
            .unwrap_or_else(|| "{engine}-{card_lc}".into()),
        DriverKind::Compose => e
            .compose
            .as_ref()
            .and_then(|cs| cs.container.get(card).cloned())
            .ok_or_else(|| format!("compose engine has no container for card `{card}`"))?,
        DriverKind::Systemd => e
            .systemd
            .as_ref()
            .map(|s| s.unit.clone())
            .unwrap_or_default(),
        DriverKind::Process | DriverKind::Template => e
            .container
            .clone()
            .unwrap_or_else(|| "{engine}-{card_lc}".into()),
    };
    template::render_word(&t, v).map_err(|x| x.to_string())
}

/// The reference used to stop, observe and read logs of whatever `engine` runs on `card`.
pub fn instance_ref(
    c: &ControlConfig,
    card_id: &str,
    engine_id: &str,
) -> Result<InstanceRef, String> {
    let vars = base_vars(c, card_id, engine_id)?;
    let name = match vars.get("name") {
        Some(TplValue::Word(n)) => n.clone(),
        _ => String::new(),
    };
    Ok(InstanceRef {
        server: c.cards[card_id].server.clone(),
        card: card_id.into(),
        engine: engine_id.into(),
        name,
        vars,
    })
}

/// The options a card runs with, before defaults: artifact options under the profile's, the
/// served id as `alias` when it differs from the engine's, vision dropped for text-only models
/// (profilecat `card_spec`). Sorted by key (layoutctl `norm_card`).
pub fn card_options(c: &ControlConfig, spec: &CardSpec) -> Result<IndexMap<String, Value>, String> {
    let (e, prof, model, art) = parts(c, spec)?;
    let mut opts: IndexMap<String, Value> = art.options.clone();
    for k in &art.unset {
        opts.insert(k.clone(), Value::Null);
    }
    for (k, v) in &prof.options {
        opts.insert(k.clone(), v.clone());
    }
    for k in &prof.unset {
        opts.insert(k.clone(), Value::Null);
    }
    if let Some(sid) = &art.served_id
        && *sid != e.served_id
        && e.driver != DriverKind::Compose
    {
        opts.insert("alias".into(), Value::String(sid.clone()));
    }
    if opts.get("vision").is_some_and(truthy) && !model.vision {
        opts.shift_remove("vision");
    }
    opts.sort_keys();
    Ok(opts)
}

/// Whether `options` turn image input on: a truthy `vision` (ninfer `--vision`) or a projector
/// (`mmproj`, llama.cpp).
fn vision_on(opts: &IndexMap<String, Value>) -> bool {
    opts.get("vision").is_some_and(truthy) || opts.get("mmproj").is_some_and(truthy)
}

/// An engine that needs an option for image input (some profile or artifact sets `vision` or
/// `mmproj`); others (Strata) take images whenever the model does.
fn vision_gated(c: &ControlConfig, engine: &str) -> bool {
    let has = |o: &IndexMap<String, Value>| o.contains_key("vision") || o.contains_key("mmproj");
    c.engines
        .get(engine)
        .is_some_and(|e| e.profiles.values().any(|p| has(&p.options)))
        || c.models
            .values()
            .filter_map(|m| m.artifacts.get(engine))
            .any(|a| has(&a.options))
}

/// Whether catalog model `model` takes images when run by `engine` with `profile` (the
/// artifact's default when `None`), from the config alone: the model's `vision`, and on an
/// engine that needs an option for it, that option in the card's options. `None` when it can't
/// be told (no engine named and the model's engines need an option).
pub fn model_vision(
    c: &ControlConfig,
    model: &str,
    engine: Option<&str>,
    profile: Option<&str>,
) -> Option<bool> {
    let m = c.models.get(model)?;
    if !m.vision {
        return Some(false);
    }
    let Some(engine) = engine else {
        // No engine chosen: known only when none of the model's engines needs an option.
        return (!m.artifacts.keys().any(|e| vision_gated(c, e))).then_some(true);
    };
    if !vision_gated(c, engine) {
        return Some(true);
    }
    let art = m.artifacts.get(engine)?;
    let profile = profile
        .map(str::to_string)
        .or_else(|| art.default_profile.clone())
        .or_else(|| art.profiles.first().cloned())?;
    let spec = CardSpec {
        engine: engine.into(),
        profile,
        model: model.into(),
    };
    card_options(c, &spec).ok().map(|o| vision_on(&o))
}

/// A route's image input from the config: its first catalog-model target, run as its on-demand
/// pick says (cold models: nothing is asked of an engine).
pub fn route_vision(c: &ControlConfig, r: &Route) -> Option<bool> {
    let base = r
        .same_as
        .as_ref()
        .and_then(|s| c.routes.get(s))
        .unwrap_or(r);
    let model = base
        .targets
        .iter()
        .find(|t| t.upstream.is_none() && t.pool.is_none() && t.card.is_none())
        .and_then(|t| t.model.as_deref())?;
    let pick = r
        .on_demand
        .as_ref()
        .or(base.on_demand.as_ref())
        .and_then(|o| o.pick.as_ref());
    model_vision(
        c,
        model,
        pick.map(|p| p.engine.as_str()),
        pick.and_then(|p| p.profile.as_deref()),
    )
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

type Parts<'a> = (&'a Engine, &'a EngineProfile, &'a Model, &'a Artifact);

fn parts<'a>(c: &'a ControlConfig, spec: &CardSpec) -> Result<Parts<'a>, String> {
    let e = c
        .engines
        .get(&spec.engine)
        .ok_or_else(|| format!("no engine `{}`", spec.engine))?;
    let prof = e
        .profiles
        .get(&spec.profile)
        .ok_or_else(|| format!("engine `{}` has no profile `{}`", spec.engine, spec.profile))?;
    let model = c
        .models
        .get(&spec.model)
        .ok_or_else(|| format!("no model `{}`", spec.model))?;
    let art = model.artifacts.get(&spec.engine).ok_or_else(|| {
        format!(
            "model `{}` has no artifact for `{}`",
            spec.model, spec.engine
        )
    })?;
    Ok((e, prof, model, art))
}

/// The build name a spec runs (profile's, else the engine's default).
pub fn build_name<'a>(e: &'a Engine, prof: &'a EngineProfile) -> Option<&'a str> {
    prof.build.as_deref().or(e.default_build.as_deref())
}

/// layoutctl's flat card spec (`norm_card`), restricted to the engine's `spec_label.keys`.
pub fn flat_spec(c: &ControlConfig, spec: &CardSpec) -> Result<Value, String> {
    let (e, prof, _model, art) = parts(c, spec)?;
    let keys: Vec<&str> = e.spec_label.keys.iter().map(String::as_str).collect();
    let mut m = serde_json::Map::new();
    let has = |k: &str| keys.contains(&k);
    if has("engine") {
        m.insert("engine".into(), Value::String(spec.engine.clone()));
    }
    if has("slots") {
        m.insert("slots".into(), Value::from(prof.lanes));
    }
    if has("model") && !art.file.is_empty() {
        m.insert("model".into(), Value::String(art.file.clone()));
    }
    if has("build")
        && let Some(b) = prof.build.as_deref()
        && Some(b) != e.spec_label.omit_build.as_deref()
    {
        m.insert("build".into(), Value::String(b.into()));
    }
    if has("options") {
        let o = card_options(c, spec)?;
        if !o.is_empty() {
            m.insert("options".into(), Value::Object(o.into_iter().collect()));
        }
    }
    if has("env") && !prof.env.is_empty() {
        let mut env: Vec<(&String, &String)> = prof.env.iter().collect();
        env.sort();
        m.insert(
            "env".into(),
            Value::Object(
                env.into_iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect(),
            ),
        );
    }
    if has("mems")
        && let Some(mems) = prof.mems.as_deref().filter(|s| !s.is_empty())
    {
        m.insert("mems".into(), Value::String(mems.into()));
    }
    Ok(Value::Object(m))
}

/// Python's `json.dumps(v, sort_keys=True)` (default separators, ASCII only): the exact
/// `layouts.spec` label text layoutctl writes and compares.
pub fn py_json(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::Number(n) => py_number(n),
        Value::String(s) => py_str(s),
        Value::Array(a) => format!("[{}]", a.iter().map(py_json).collect::<Vec<_>>().join(", ")),
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}: {}", py_str(k), py_json(&o[k])))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

/// Python's compact `json.dumps(v, separators=(",", ":"))` (option values that are objects).
pub fn py_json_compact(v: &Value) -> String {
    match v {
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(py_json_compact).collect::<Vec<_>>().join(",")
        ),
        Value::Object(o) => {
            let parts: Vec<String> = o
                .iter()
                .map(|(k, x)| format!("{}:{}", py_str(k), py_json_compact(x)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        other => py_json(other),
    }
}

fn py_str(s: &str) -> String {
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{u:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn py_number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    py_float(n.as_f64().unwrap_or(0.0))
}

/// Python `repr(float)`.
pub fn py_float(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e16 {
        return format!("{f:.1}");
    }
    if f != 0.0 && (f.abs() < 1e-4 || f.abs() >= 1e16) {
        let s = format!("{f:e}"); // 1e-5, 1.5e20
        if let Some((m, e)) = s.split_once('e') {
            let (sign, digits) = if let Some(d) = e.strip_prefix('-') {
                ('-', d)
            } else {
                ('+', e)
            };
            return format!("{m}e{sign}{digits:0>2}");
        }
    }
    format!("{f}")
}

/// Python `str(v)` for an option value on the command line.
fn py_arg(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Number(n) => py_number(n),
        Value::Null => "None".into(),
        other => py_json_compact(other),
    }
}

fn is_option_key(k: &str) -> bool {
    let b = k.as_bytes();
    !b.is_empty()
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// Everything about one (card, spec): the variables and the rendered pieces.
pub fn resolve(c: &ControlConfig, card_id: &str, spec: &CardSpec) -> Result<Resolved, String> {
    let (e, prof, _model, art) = parts(c, spec)?;
    let card = c
        .cards
        .get(card_id)
        .ok_or_else(|| format!("no card `{card_id}`"))?;
    let mut v = base_vars(c, card_id, &spec.engine)?;
    let w = |s: &str| TplValue::Word(s.to_string());
    let port = *e
        .port
        .get(card_id)
        .ok_or_else(|| format!("engine `{}` has no port for card `{card_id}`", spec.engine))?;
    let served_id = art.served_id.clone().unwrap_or_else(|| e.served_id.clone());
    v.insert("profile".into(), w(&spec.profile));
    v.insert("model".into(), w(&spec.model));
    v.insert("lanes".into(), w(&prof.lanes.to_string()));
    v.insert(
        "ctx".into(),
        w(&prof
            .ctx
            .or(e.max_context)
            .map(|x| x.to_string())
            .unwrap_or_default()),
    );
    v.insert("served_id".into(), w(&served_id));
    v.insert("mems".into(), w(prof.mems.as_deref().unwrap_or("")));

    // Build: a directory ({build_dir}) or an image ({build_image}); same value, two names.
    if let Some(b) = build_name(e, prof) {
        let raw_b = e
            .builds
            .get(b)
            .ok_or_else(|| format!("engine `{}` has no build `{b}`", spec.engine))?;
        let val = template::render_word(raw_b, &v).map_err(|x| x.to_string())?;
        v.insert("build".into(), w(b));
        v.insert("build_dir".into(), w(&val));
        v.insert("build_image".into(), w(&val));
    }
    // Model file: absolute, or under the engine's model_dir.
    let model_dir = match &e.model_dir {
        Some(d) => template::render_word(d, &v).map_err(|x| x.to_string())?,
        None => String::new(),
    };
    let model_path = if art.file.starts_with('/') || model_dir.is_empty() {
        art.file.clone()
    } else {
        format!("{}/{}", model_dir.trim_end_matches('/'), art.file)
    };
    let (parent, base) = match model_path.rsplit_once('/') {
        Some((p, b)) => (p.to_string(), b.to_string()),
        None => (String::new(), model_path.clone()),
    };
    v.insert("model_file".into(), w(&art.file));
    v.insert("model_dir".into(), w(&model_dir));
    v.insert("model_path".into(), w(&model_path));
    v.insert("model_parent".into(), w(&parent));
    v.insert("model_basename".into(), w(&base));

    // The label.
    let flat = flat_spec(c, spec)?;
    let spec_json = py_json(&flat);
    v.insert("spec_json".into(), w(&spec_json));

    let image = match &e.image {
        Some(i) => Some(template::render_word(i, &v).map_err(|x| x.to_string())?),
        None => None,
    };
    if let Some(i) = &image {
        v.insert("image".into(), w(i));
    }

    // Options: defaults (in their order) overridden by the card's (sorted), minus pops.
    let run = e.run.clone().unwrap_or_default();
    let card_opts = card_options(c, spec)?;
    let mut opts: IndexMap<String, Value> = IndexMap::new();
    for (k, val) in &e.defaults {
        opts.insert(k.clone(), val.clone());
    }
    for (k, val) in &card_opts {
        if run.fixed.iter().any(|f| f == k) || !is_option_key(k) {
            return Err(format!(
                "{} option `{k}` cannot be set from a layout",
                spec.engine
            ));
        }
        opts.insert(k.clone(), val.clone());
    }
    let mut option_words: Vec<String> = Vec::new();
    let mut opt_args: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, val) in &opts {
        let value = match val {
            Value::String(s) => Value::String(
                template::render_word(s, &v).map_err(|x| format!("option `{k}`: {x}"))?,
            ),
            other => other.clone(),
        };
        if let Some(tpl) = run.option_args.get(k) {
            if matches!(value, Value::Null | Value::Bool(false)) {
                continue;
            }
            let mut ov = v.clone();
            let txt = py_arg(&value);
            ov.insert("value".into(), w(&txt));
            if let Ok(n) = txt.parse::<f64>() {
                let prod = n * prof.lanes as f64;
                ov.insert(
                    "value_x_lanes".into(),
                    w(&if prod.fract() == 0.0 {
                        format!("{}", prod as i64)
                    } else {
                        prod.to_string()
                    }),
                );
            }
            opt_args.insert(
                k.clone(),
                template::render_argv(tpl, &ov).map_err(|x| x.to_string())?,
            );
            continue;
        }
        if run.pop.iter().any(|p| p == k) {
            continue;
        }
        match &value {
            Value::Null | Value::Bool(false) => {}
            Value::Bool(true) => option_words.push(format!("--{k}")),
            other => match run.options_style {
                OptionsStyle::Gnu => {
                    option_words.push(format!("--{k}"));
                    option_words.push(py_arg(other));
                }
                OptionsStyle::GnuEquals => option_words.push(format!("--{k}={}", py_arg(other))),
            },
        }
    }
    v.insert("options".into(), TplValue::List(option_words));
    for k in run.option_args.keys() {
        v.insert(
            format!("opt_args:{k}"),
            TplValue::List(opt_args.remove(k).unwrap_or_default()),
        );
    }

    // env, mems, labels, key.
    let mut env: Vec<(String, String)> = prof
        .env
        .iter()
        .map(|(k, x)| (k.clone(), x.clone()))
        .collect();
    env.sort();
    for (k, _) in &env {
        if !k
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
        {
            return Err(format!("environment variable `{k}` must be UPPER_CASE"));
        }
    }
    v.insert(
        "env_args".into(),
        TplValue::List(
            env.iter()
                .flat_map(|(k, x)| ["-e".to_string(), format!("{k}={x}")])
                .collect(),
        ),
    );
    let mems_ok = |m: &str| {
        m.split([',', '-'])
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    };
    let mems_args = match prof.mems.as_deref().filter(|m| !m.is_empty()) {
        Some(m) if !mems_ok(m) => return Err(format!("mems `{m}` is not a NUMA node list")),
        Some(m) => vec![format!("--cpuset-mems={m}")],
        None => vec![],
    };
    v.insert("mems_args".into(), TplValue::List(mems_args));
    let mut labels = Vec::new();
    for (k, t) in &e.labels {
        labels.push((
            k.clone(),
            template::render_word(t, &v).map_err(|x| x.to_string())?,
        ));
    }
    v.insert(
        "label_args".into(),
        TplValue::List(
            labels
                .iter()
                .flat_map(|(k, x)| ["--label".to_string(), format!("{k}={x}")])
                .collect(),
        ),
    );
    let key_mode = e.key_delivery.mode_for(build_name(e, prof));
    let (key_args, key_mount) = key_words(e, build_name(e, prof));
    v.insert("key_args".into(), TplValue::List(key_args));
    v.insert("key_mount_args".into(), TplValue::List(key_mount));

    let _ = card;
    let iref = InstanceRef {
        server: c.cards[card_id].server.clone(),
        card: card_id.into(),
        engine: spec.engine.clone(),
        name: match v.get("name") {
            Some(TplValue::Word(n)) => n.clone(),
            _ => String::new(),
        },
        vars: v.clone(),
    };
    let args = if !run.command.is_empty() {
        template::render_argv(&run.command, &v).map_err(|x| x.to_string())?
    } else {
        Vec::new()
    };
    let docker = if !run.docker.is_empty() {
        template::render_argv(&run.docker, &v).map_err(|x| x.to_string())?
    } else {
        Vec::new()
    };
    Ok(Resolved {
        iref,
        spec: spec.clone(),
        args,
        image,
        port,
        served_id,
        lanes: prof.lanes,
        ctx: prof.ctx,
        env,
        mounts: Vec::new(),
        labels,
        docker,
        entrypoint: None,
        spec_json,
        model_file: if art.file.is_empty() {
            None
        } else {
            Some(model_path)
        },
        key_mode: Some(key_mode),
    })
}

/// `{key_args}` and `{key_mount_args}` for the engine's key delivery.
fn key_words(e: &Engine, build: Option<&str>) -> (Vec<String>, Vec<String>) {
    let Some(id) = e.key.as_deref().and_then(ControlConfig::secret_id) else {
        return (Vec::new(), Vec::new());
    };
    let kd = &e.key_delivery;
    let host = secret_host_path(e, id);
    let flag = kd.flag_for(build).to_string();
    match kd.mode_for(build) {
        KeyMode::None => (Vec::new(), Vec::new()),
        KeyMode::FileFlag => (
            vec![flag, kd.path.clone()],
            vec!["-v".into(), format!("{host}:{}:ro", kd.path)],
        ),
        // The wrapper adds the flag itself inside the container (drivers::docker).
        KeyMode::Wrapper => (
            Vec::new(),
            vec!["-v".into(), format!("{host}:{}:ro", kd.path)],
        ),
        // layoutctl parity: the value is read on the host by the shell, never in our text.
        KeyMode::Argv => (
            vec![
                flag,
                raw(&format!("\"$(cat {})\"", template::shell_quote(&host))),
            ],
            Vec::new(),
        ),
    }
}

/// The start commands of `spec` on `card` (validation renders these for every layout card).
pub fn start_commands(c: &ControlConfig, card: &str, spec: &CardSpec) -> Result<Vec<Cmd>, String> {
    let r = resolve(c, card, spec)?;
    let e = &c.engines[&spec.engine];
    drivers::for_engine(e).start(&r).map_err(|x| x.to_string())
}

/// Every CardSpec the config can produce for a card: each layout's, plus each artifact x
/// fitting profile of the engines (on-demand picks). Used to turn a running container's label
/// back into a spec.
pub fn known_specs(c: &ControlConfig, card: &str) -> Vec<CardSpec> {
    let mut out: Vec<CardSpec> = Vec::new();
    for l in c.layouts.values() {
        for (cid, s) in resolve_cards(l) {
            if cid == card
                && let Some(s) = s
                && !out.contains(&s)
            {
                out.push(s);
            }
        }
    }
    for (mid, m) in &c.models {
        for (eid, a) in &m.artifacts {
            let Some(e) = c.engines.get(eid) else {
                continue;
            };
            let profs: Vec<&String> = if a.profiles.is_empty() {
                e.profiles.keys().collect()
            } else {
                a.profiles.iter().collect()
            };
            for p in profs {
                let s = CardSpec {
                    engine: eid.clone(),
                    profile: p.clone(),
                    model: mid.clone(),
                };
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
    }
    out
}

/// The spec whose label equals `flat` (layoutctl's `norm_card` form), if any. Several specs
/// may share a label (Strata labels carry only engine and slots); the first known one wins,
/// layouts before picks.
pub fn spec_from_label(c: &ControlConfig, card: &str, flat: &Value) -> Option<CardSpec> {
    let want = py_json(flat);
    known_specs(c, card).into_iter().find(|s| {
        flat_spec(c, s)
            .map(|f| py_json(&f) == want)
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::validate::tests::sample;

    fn spec(e: &str, p: &str, m: &str) -> CardSpec {
        CardSpec {
            engine: e.into(),
            profile: p.into(),
            model: m.into(),
        }
    }

    #[test]
    fn vision_from_the_catalog_and_the_engine_profile() {
        let mut c = sample();
        // Text-only model: never.
        assert_eq!(
            model_vision(&c, "small", Some("ninfer"), Some("4lane")),
            Some(false)
        );
        // ninfer needs its `vision` option: the 4-lane profile has it, the 2-lane one not.
        assert_eq!(
            model_vision(&c, "dense", Some("ninfer"), Some("4lane")),
            Some(true)
        );
        assert_eq!(
            model_vision(&c, "dense", Some("ninfer"), Some("2lane")),
            Some(false)
        );
        // No engine chosen and its engine needs an option: unknown.
        assert_eq!(model_vision(&c, "dense", None, None), None);
        // An engine that needs no option (Strata) takes images whenever the model does.
        c.models.get_mut("flash").unwrap().vision = true;
        assert_eq!(model_vision(&c, "flash", Some("strata"), None), Some(true));
        assert_eq!(model_vision(&c, "flash", None, None), Some(true));
        assert_eq!(model_vision(&c, "nope", None, None), None);
        // Routes: a cold catalog model by its on-demand pick; pool routes are not the catalog's.
        assert_eq!(route_vision(&c, &c.routes["small-model"]), Some(false));
        assert_eq!(route_vision(&c, &c.routes["dense-27b"]), None);
        let mut r = c.routes["small-model"].clone();
        r.targets[0].model = Some("dense".into());
        assert_eq!(route_vision(&c, &r), Some(false), "2lane: text only");
        r.on_demand.as_mut().unwrap().pick.as_mut().unwrap().profile = Some("4lane".into());
        assert_eq!(route_vision(&c, &r), Some(true));
        // llama.cpp's projector counts as the option.
        let mut o = IndexMap::new();
        o.insert(
            "mmproj".to_string(),
            Value::String("mmproj-F16.gguf".into()),
        );
        assert!(vision_on(&o));
    }

    #[test]
    fn flat_spec_matches_norm_card() {
        let c = sample();
        let f = flat_spec(&c, &spec("ninfer", "4lane", "dense")).unwrap();
        assert_eq!(
            py_json(&f),
            r#"{"build": "merged", "engine": "ninfer", "env": {"NINFER_SHARED_PREFIX": "structural"}, "mems": "1", "model": "dense.ninfer", "options": {"kv-dtype": false, "vision": true}, "slots": 4}"#
        );
        // Text-only model loses vision; another served id becomes the alias.
        let f = flat_spec(&c, &spec("ninfer", "2lane", "small")).unwrap();
        assert_eq!(
            py_json(&f),
            r#"{"engine": "ninfer", "model": "small.ninfer", "options": {"alias": "small-model"}, "slots": 2}"#
        );
        let f = flat_spec(&c, &spec("strata", "2slot", "flash")).unwrap();
        assert_eq!(py_json(&f), r#"{"engine": "strata", "slots": 2}"#);
    }

    #[test]
    fn resolve_renders_docker_words() {
        let c = sample();
        let r = resolve(&c, "A", &spec("ninfer", "4lane", "dense")).unwrap();
        let words = r.docker.join(" ");
        assert!(words.contains("--cpuset-cpus=0-23"), "{words}");
        assert!(
            words.contains("-e NINFER_SHARED_PREFIX=structural --cpuset-mems=1"),
            "{words}"
        );
        assert!(
            words.contains("-v /srv/ai/ninfer/builds/merged:/app:ro"),
            "{words}"
        );
        assert!(words.contains("/app/ninfer-serve /models/dense.ninfer --port 8094 --api-key-file /run/secrets/engine-key"), "{words}");
        assert!(
            words.contains("-v /run/abhoth/secrets/engine_key:/run/secrets/engine-key:ro"),
            "{words}"
        );
        // defaults order, card options sorted over them; false drops; {lanes} rendered.
        assert!(words.ends_with("--model-id dense-27b --max-context 131072 --max-concurrency 4 --lm-head-draft --vision"), "{words}");
        assert_eq!(r.iref.name, "ninfer-a");
        assert_eq!(
            r.model_file.as_deref(),
            Some("/srv/ai/ninfer/models/dense.ninfer")
        );
    }

    #[test]
    fn argv_key_mode_never_puts_the_key_in_text() {
        let mut c = sample();
        c.engines.get_mut("ninfer").unwrap().key_delivery.mode = KeyMode::Argv;
        let r = resolve(&c, "B", &spec("ninfer", "2lane", "dense")).unwrap();
        let m = template::masked(&r.docker).join(" ");
        assert!(m.contains("--api-key ***"), "{m}");
        assert!(!m.contains("key-file"));
    }

    #[test]
    fn fixed_options_are_refused() {
        let mut c = sample();
        c.engines
            .get_mut("ninfer")
            .unwrap()
            .profiles
            .get_mut("2lane")
            .unwrap()
            .options
            .insert("port".into(), 1.into());
        assert!(
            resolve(&c, "A", &spec("ninfer", "2lane", "dense"))
                .unwrap_err()
                .contains("cannot be set")
        );
    }

    #[test]
    fn label_round_trips_to_spec() {
        let c = sample();
        for s in [
            spec("ninfer", "4lane", "dense"),
            spec("ninfer", "2lane", "small"),
            spec("strata", "2slot", "flash"),
        ] {
            let f = flat_spec(&c, &s).unwrap();
            assert_eq!(spec_from_label(&c, "B", &f), Some(s));
        }
        assert_eq!(
            spec_from_label(
                &c,
                "A",
                &serde_json::json!({"engine": "ninfer", "slots": 9})
            ),
            None
        );
    }

    #[test]
    fn py_formats() {
        assert_eq!(py_float(1.0), "1.0");
        assert_eq!(py_float(0.95), "0.95");
        assert_eq!(py_float(1e-5), "1e-05");
        assert_eq!(
            py_json_compact(&serde_json::json!({"reasoning_effort": "medium"})),
            r#"{"reasoning_effort":"medium"}"#
        );
        assert_eq!(py_json(&serde_json::json!("\u{e9}")), r#""\u00e9""#);
    }
}
