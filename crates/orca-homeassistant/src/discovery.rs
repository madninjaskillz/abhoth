//! HA MQTT discovery payloads (SUPERVISOR.md 11.2, 11.3).
//!
//! Devices: the server, one per card, one per fan, one per engine (`via_device` = server).
//! With `legacy_ids`, the entities that replace ha-bridge's keep its `deepthought_*`
//! unique_ids and object ids, and the server device keeps ha-bridge's `deepthought`
//! identifier, so HA history, entity ids and automations carry on (Q7).

use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::config::MqttConfig;
use crate::model::HaSnapshot;
use crate::topics::{Topics, slug};

/// Everything discovery needs besides the snapshot.
#[derive(Debug, Clone)]
pub struct DiscoveryOptions {
    pub legacy_ids: bool,
    pub legacy_card: String,
    pub controls: bool,
    pub scopes: Vec<String>,
    pub clock_cap_options: Vec<u32>,
    pub clock_cap_ttl_seconds: u64,
    /// Supervisor version, for `origin` and the device's `sw_version`.
    pub sw_version: String,
}

impl DiscoveryOptions {
    pub fn from_config(c: &MqttConfig, sw_version: &str) -> Self {
        Self {
            legacy_ids: c.legacy_ids,
            legacy_card: c.legacy_card.clone(),
            controls: c.controls,
            scopes: c.scopes.clone(),
            clock_cap_options: c.clock_cap_options.clone(),
            clock_cap_ttl_seconds: c.clock_cap_ttl_seconds,
            sw_version: sw_version.to_string(),
        }
    }

    pub fn control(&self, scope: &str) -> bool {
        self.controls && self.scopes.iter().any(|s| s == scope)
    }
}

/// One retained discovery config.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveryMessage {
    pub topic: String,
    pub component: &'static str,
    pub unique_id: String,
    pub payload: Value,
}

/// ha-bridge's unique_ids, and what replaces each (11.3). `card:` entries follow
/// `legacy_card` except the per-card "LLM Card X" sensors, which follow their own card.
pub const LEGACY_MAP: &[(&str, &str)] = &[
    ("deepthought_gpu_temp", "card:core_temp"),
    ("deepthought_hbm_temp", "card:hbm_temp"),
    ("deepthought_power", "card:power"),
    ("deepthought_sm_clock", "card:sm_clock"),
    ("deepthought_gpu_util", "card:util"),
    ("deepthought_vram_gb", "card:vram_used"),
    ("deepthought_tps", "server:tok_s_total"),
    ("deepthought_model", "server:models"),
    ("deepthought_llm_profile", "server:layout"),
    ("deepthought_llm_card_a", "card A:engine"),
    ("deepthought_llm_card_b", "card B:engine"),
    ("deepthought_llm_switch", "server:switch_state"),
    ("deepthought_bench_queue_running", "server:bench_running"),
    ("deepthought_llm_target_profile", "server:layout_target"),
    ("deepthought_llm_apply_profile", "server:layout_apply"),
    ("deepthought_maxclock", "retired: per-card clock_cap (Q6)"),
];

fn legacy_for(opts: &DiscoveryOptions, scope: Scope<'_>, key: &str) -> Option<&'static str> {
    if !opts.legacy_ids {
        return None;
    }
    let want = match scope {
        Scope::Server => format!("server:{key}"),
        Scope::Card(c) => {
            if key == "engine" {
                format!("card {}:engine", c.to_ascii_uppercase())
            } else if c.eq_ignore_ascii_case(&opts.legacy_card) {
                format!("card:{key}")
            } else {
                return None;
            }
        }
        _ => return None,
    };
    LEGACY_MAP.iter().find(|(_, v)| *v == want).map(|(k, _)| *k)
}

#[derive(Clone, Copy)]
enum Scope<'a> {
    Server,
    Card(&'a str),
    Fan(&'a str),
    Engine(&'a str),
}

struct Builder<'a> {
    t: &'a Topics,
    opts: &'a DiscoveryOptions,
    server_ident: Value,
    out: Vec<DiscoveryMessage>,
}

/// A field-driven entity: `value_json.<field>` from the scope's state topic.
struct E<'a> {
    component: &'static str,
    key: String,
    name: &'a str,
    field: String,
    extra: Value,
}

fn e<'a>(component: &'static str, key: &str, name: &'a str, extra: Value) -> E<'a> {
    E {
        component,
        key: key.to_string(),
        name,
        field: key.to_string(),
        extra,
    }
}

fn sensor<'a>(
    key: &str,
    name: &'a str,
    unit: Option<&str>,
    dc: Option<&str>,
    icon: Option<&str>,
) -> E<'a> {
    let mut m = Map::new();
    if let Some(u) = unit {
        m.insert("unit_of_measurement".into(), json!(u));
        m.insert("state_class".into(), json!("measurement"));
    }
    if let Some(d) = dc {
        m.insert("device_class".into(), json!(d));
    }
    if let Some(i) = icon {
        m.insert("icon".into(), json!(i));
    }
    e("sensor", key, name, Value::Object(m))
}

fn diag(mut x: E<'_>) -> E<'_> {
    if let Value::Object(m) = &mut x.extra {
        m.insert("entity_category".into(), json!("diagnostic"));
    }
    x
}

fn with(mut x: E<'_>, more: Value) -> E<'_> {
    if let (Value::Object(m), Value::Object(more)) = (&mut x.extra, more) {
        m.extend(more);
    }
    x
}

impl<'a> Builder<'a> {
    fn device(&self, scope: Scope<'_>, snap: &HaSnapshot) -> Value {
        let s = &snap.server;
        match scope {
            Scope::Server => {
                let mut d = json!({
                    "identifiers": self.server_ident.clone(),
                    "name": s.name,
                    "manufacturer": s.manufacturer.clone().unwrap_or_else(|| "Abhoth".into()),
                    "model": s.model.clone().unwrap_or_else(|| "Abhoth Supervisor".into()),
                });
                d["sw_version"] = json!(
                    s.version
                        .clone()
                        .unwrap_or_else(|| self.opts.sw_version.clone())
                );
                d
            }
            Scope::Card(c) => {
                let card = snap.card(c);
                json!({
                    "identifiers": [self.t.unique_id(&format!("card_{}", slug(c)))],
                    "name": format!("{} card {}", s.name, c.to_ascii_uppercase()),
                    "manufacturer": "NVIDIA",
                    "model": card.and_then(|c| c.gpu.clone()).unwrap_or_else(|| "GPU".into()),
                    "via_device": self.server_ident[0].clone(),
                })
            }
            Scope::Fan(f) => {
                let fan = snap.fans.iter().find(|x| x.id == f);
                let mut name = format!(
                    "{} fan {}",
                    s.name,
                    fan.and_then(|x| x.name.clone())
                        .unwrap_or_else(|| f.to_string())
                );
                if let Some(fan) = fan
                    && !fan.cools.is_empty()
                {
                    name.push_str(&format!(" (card {})", fan.cools.join("+")));
                }
                json!({
                    "identifiers": [self.t.unique_id(&format!("fan_{}", slug(f)))],
                    "name": name,
                    "model": "Fan (supervisor-derived)",
                    "via_device": self.server_ident[0].clone(),
                })
            }
            Scope::Engine(en) => {
                let eng = snap.engines.iter().find(|x| x.id == en);
                json!({
                    "identifiers": [self.t.unique_id(&format!("engine_{}", slug(en)))],
                    "name": format!("{} engine {}", s.name, eng.and_then(|x| x.title.clone()).unwrap_or_else(|| en.to_string())),
                    "model": eng.and_then(|x| x.kind.clone()).map(|k| format!("{k} engine")).unwrap_or_else(|| "engine".into()),
                    "via_device": self.server_ident[0].clone(),
                })
            }
        }
    }

    fn availability(&self, scope: Scope<'_>) -> Value {
        let mut a = vec![json!({"topic": self.t.availability()})];
        if let Scope::Card(c) = scope {
            a.push(json!({"topic": self.t.card_availability(c)}));
        }
        json!(a)
    }

    fn state_topic(&self, scope: Scope<'_>) -> String {
        match scope {
            Scope::Server => self.t.state(),
            Scope::Card(c) => self.t.card_state(c),
            Scope::Fan(f) => self.t.fan_state(f),
            Scope::Engine(en) => self.t.engine_state(en),
        }
    }

    fn scoped_key(scope: Scope<'_>, key: &str) -> String {
        match scope {
            Scope::Server => key.to_string(),
            Scope::Card(c) => format!("card_{}_{key}", slug(c)),
            Scope::Fan(f) => format!("fan_{}_{key}", slug(f)),
            Scope::Engine(en) => format!("engine_{}_{key}", slug(en)),
        }
    }

    /// Adds one entity. Command entities pass their own topics in `extra`.
    fn add(&mut self, snap: &HaSnapshot, scope: Scope<'_>, x: E<'_>, stateful: bool) {
        let legacy = legacy_for(self.opts, scope, &x.key);
        let (uid, object_id) = match legacy {
            Some(l) => (l.to_string(), l.to_string()),
            None => {
                let k = Self::scoped_key(scope, &x.key);
                (self.t.unique_id(&k), self.t.unique_id(&k))
            }
        };
        let mut cfg = Map::new();
        cfg.insert("name".into(), json!(x.name));
        cfg.insert("unique_id".into(), json!(uid));
        if legacy.is_some() {
            // ha-bridge published both; keeping them keeps the entity ids it created.
            cfg.insert("object_id".into(), json!(object_id));
        }
        cfg.insert(
            "default_entity_id".into(),
            json!(format!("{}.{object_id}", x.component)),
        );
        if stateful {
            cfg.insert("state_topic".into(), json!(self.state_topic(scope)));
            cfg.insert(
                "value_template".into(),
                json!(format!("{{{{ value_json.{} }}}}", x.field)),
            );
        }
        if let Value::Object(m) = x.extra {
            for (k, v) in m {
                cfg.insert(k, v);
            }
        }
        cfg.insert("availability".into(), self.availability(scope));
        cfg.insert("availability_mode".into(), json!("all"));
        cfg.insert("device".into(), self.device(scope, snap));
        cfg.insert(
            "origin".into(),
            json!({"name": "Abhoth Supervisor", "sw_version": self.opts.sw_version}),
        );
        self.out.push(DiscoveryMessage {
            topic: self.t.discovery(x.component, &uid),
            component: x.component,
            unique_id: uid,
            payload: Value::Object(cfg),
        });
    }
}

fn attrs(fields: &[&str]) -> String {
    let inner = fields
        .iter()
        .map(|f| format!("'{f}': value_json.{f}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{{{{ {{{inner}}} | tojson }}}}")
}

/// All discovery configs for this snapshot, in a stable order.
pub fn build(t: &Topics, opts: &DiscoveryOptions, snap: &HaSnapshot) -> Vec<DiscoveryMessage> {
    let mut ident = vec![json!(format!("abhoth_{}", t.server))];
    if opts.legacy_ids {
        // ha-bridge's device: HA merges devices on any shared identifier.
        ident.insert(0, json!("deepthought"));
    }
    let mut b = Builder {
        t,
        opts,
        server_ident: Value::Array(ident),
        out: Vec::new(),
    };
    let s = Scope::Server;

    // ── server ──
    for x in [
        sensor(
            "tok_s_total",
            "Tokens per second",
            Some("t/s"),
            None,
            Some("mdi:speedometer"),
        ),
        diag(sensor("models", "Model", None, None, Some("mdi:brain"))),
        sensor(
            "power_total",
            "GPU power total",
            Some("W"),
            Some("power"),
            None,
        ),
        sensor("cpu", "Host CPU", Some("%"), None, Some("mdi:cpu-64-bit")),
        sensor("ram_used", "RAM used", Some("GB"), Some("data_size"), None),
        sensor("ram_arc", "RAM ARC", Some("GB"), Some("data_size"), None),
        sensor(
            "ram_consumed",
            "RAM consumed",
            Some("GB"),
            Some("data_size"),
            None,
        ),
        sensor("ram_free", "RAM free", Some("GB"), Some("data_size"), None),
        sensor(
            "net_in",
            "Network in",
            Some("Mbit/s"),
            Some("data_rate"),
            None,
        ),
        sensor(
            "net_out",
            "Network out",
            Some("Mbit/s"),
            Some("data_rate"),
            None,
        ),
        with(
            sensor("layout", "Layout", None, None, Some("mdi:layers-triple")),
            json!({"json_attributes_topic": t.state(), "json_attributes_template": attrs(&["layout_title", "layouts"])}),
        ),
        with(
            sensor(
                "switch_state",
                "Layout switch",
                None,
                None,
                Some("mdi:swap-horizontal"),
            ),
            json!({"json_attributes_topic": t.state(), "json_attributes_template": attrs(&["last_switch", "last_command", "layout_target"])}),
        ),
        sensor(
            "last_switch_at",
            "Last switch",
            None,
            Some("timestamp"),
            None,
        ),
        sensor(
            "on_demand",
            "On-demand",
            None,
            None,
            Some("mdi:power-sleep"),
        ),
        sensor("leases", "Leases held", None, None, Some("mdi:lock")),
        sensor(
            "slots_busy",
            "Slots busy",
            None,
            None,
            Some("mdi:tray-full"),
        ),
        sensor("slots_total", "Slots total", None, None, Some("mdi:tray")),
        e(
            "binary_sensor",
            "bench_running",
            "Bench running",
            json!({"icon": "mdi:timer-sand"}),
        ),
        e(
            "binary_sensor",
            "calendar_block",
            "Calendar block",
            json!({"icon": "mdi:calendar-lock"}),
        ),
        with(
            e(
                "binary_sensor",
                "event_active",
                "Event active",
                json!({"icon": "mdi:calendar-star"}),
            ),
            json!({"json_attributes_topic": t.state(), "json_attributes_template": attrs(&["event"])}),
        ),
        with(
            e(
                "binary_sensor",
                "problem",
                "Problem",
                json!({"device_class": "problem"}),
            ),
            json!({"json_attributes_topic": t.state(), "json_attributes_template": attrs(&["open_alerts"])}),
        ),
        diag(sensor(
            "version",
            "Supervisor version",
            None,
            None,
            Some("mdi:tag"),
        )),
        diag(sensor(
            "config_rev",
            "Config revision",
            None,
            None,
            Some("mdi:file-cog"),
        )),
        diag(sensor(
            "hostd",
            "hostd status",
            None,
            None,
            Some("mdi:heart-pulse"),
        )),
    ] {
        b.add(snap, s, x, true);
    }
    // Event entity for alerts (10.3): no value_template, the payload is the event.
    b.add(
        snap,
        s,
        e(
            "event",
            "alert",
            "Abhoth alert",
            json!({"state_topic": t.alert_event(), "event_types": ["info", "warning", "critical", "resolved"], "icon": "mdi:alert"}),
        ),
        false,
    );

    // ── server controls ──
    if opts.control("switch") {
        let options = if snap.control.layouts.is_empty() {
            vec!["unknown".to_string()]
        } else {
            snap.control.layouts.clone()
        };
        b.add(
            snap,
            s,
            e(
                "select",
                "layout_target",
                "Layout to apply",
                json!({"command_topic": t.set("layout_target"), "state_topic": t.layout_target(), "options": options, "icon": "mdi:layers-search"}),
            ),
            false,
        );
        b.add(
            snap,
            s,
            e(
                "button",
                "layout_apply",
                "Apply layout",
                json!({"command_topic": t.set("layout_apply"), "payload_press": "apply", "icon": "mdi:swap-horizontal-bold"}),
            ),
            false,
        );
    }
    if opts.control("fan_profile") {
        b.add(
            snap,
            s,
            with(
                e("select", "fan_mode", "Fan mode", json!({"command_topic": t.set("fan_mode"), "options": ["auto", "quiet", "full"], "icon": "mdi:fan"})),
                json!({"entity_category": "config"}),
            ),
            true,
        );
    }

    // ── cards ──
    for card in &snap.cards {
        let c = Scope::Card(&card.id);
        for x in [
            sensor(
                "core_temp",
                "Core temp",
                Some("°C"),
                Some("temperature"),
                None,
            ),
            sensor(
                "hbm_temp",
                "HBM temp",
                Some("°C"),
                Some("temperature"),
                None,
            ),
            sensor("power", "Power", Some("W"), Some("power"), None),
            sensor(
                "power_pct",
                "Power of limit",
                Some("%"),
                None,
                Some("mdi:flash"),
            ),
            sensor("sm_clock", "SM clock", Some("MHz"), Some("frequency"), None),
            sensor("util", "Utilization", Some("%"), None, Some("mdi:chip")),
            sensor(
                "vram_used",
                "VRAM used",
                Some("GB"),
                None,
                Some("mdi:memory"),
            ),
            with(
                sensor("engine", "Engine", None, None, Some("mdi:expansion-card")),
                json!({"json_attributes_topic": t.card_state(&card.id), "json_attributes_template": attrs(&["engine_name", "engine_state", "model", "gpu"])}),
            ),
            sensor(
                "engine_state",
                "Engine state",
                None,
                None,
                Some("mdi:state-machine"),
            ),
            sensor("model", "Model", None, None, Some("mdi:brain")),
            sensor(
                "tok_s",
                "Tokens per second",
                Some("t/s"),
                None,
                Some("mdi:speedometer"),
            ),
            sensor("running", "Running", None, None, Some("mdi:play")),
            sensor("queued", "Queued", None, None, Some("mdi:tray-full")),
            e(
                "binary_sensor",
                "clock_locked",
                "Clock locked",
                json!({"icon": "mdi:lock-clock"}),
            ),
            with(
                e(
                    "binary_sensor",
                    "throttling",
                    "Throttling",
                    json!({"icon": "mdi:speedometer-slow"}),
                ),
                json!({"json_attributes_topic": t.card_state(&card.id), "json_attributes_template": attrs(&["throttle_reasons"])}),
            ),
        ] {
            // Fields that ha-bridge exposed under the same template names keep them.
            b.add(snap, c, x, true);
        }
        if opts.control("card_state") {
            b.add(
                snap,
                c,
                e("switch", "online", "Online", json!({"command_topic": t.set(&format!("card_{}_online", slug(&card.id))), "icon": "mdi:power"})),
                true,
            );
        }
        if opts.control("clock_cap") && card.cap_offered {
            let mut options = vec!["none".to_string()];
            options.extend(opts.clock_cap_options.iter().map(|m| m.to_string()));
            b.add(
                snap,
                c,
                e(
                    "select",
                    "clock_cap",
                    "Clock cap",
                    json!({
                        "command_topic": t.set(&format!("card_{}_clock_cap", slug(&card.id))),
                        "options": options,
                        "entity_category": "config",
                        "icon": "mdi:speedometer-medium",
                        "json_attributes_topic": t.card_state(&card.id),
                        "json_attributes_template": attrs(&["clock_cap_ttl_seconds"]),
                    }),
                ),
                true,
            );
        }
    }

    // ── fans ──
    for fan in &snap.fans {
        let f = Scope::Fan(&fan.id);
        for x in [
            sensor("duty", "Duty", Some("%"), None, Some("mdi:fan")),
            sensor("rpm", "Speed", Some("rpm"), None, Some("mdi:fan")),
            sensor(
                "control_temp",
                "Control temp",
                Some("°C"),
                Some("temperature"),
                None,
            ),
            sensor(
                "source",
                "Control source",
                None,
                None,
                Some("mdi:thermometer-lines"),
            ),
            diag(sensor(
                "push_age",
                "Push age",
                Some("s"),
                Some("duration"),
                None,
            )),
            e(
                "binary_sensor",
                "failsafe",
                "Failsafe",
                json!({"device_class": "problem"}),
            ),
        ] {
            b.add(snap, f, x, true);
        }
    }

    // ── engines ──
    for en in &snap.engines {
        let g = Scope::Engine(&en.id);
        for x in [
            sensor("state", "State", None, None, Some("mdi:state-machine")),
            sensor("model", "Model", None, None, Some("mdi:brain")),
            sensor(
                "tok_s",
                "Tokens per second",
                Some("t/s"),
                None,
                Some("mdi:speedometer"),
            ),
            e(
                "binary_sensor",
                "healthy",
                "Healthy",
                json!({"device_class": "running"}),
            ),
        ] {
            b.add(snap, g, x, true);
        }
    }
    b.out
}

/// The discovery topics to clear (retained empty) at startup: configured extras, plus with
/// `legacy_ids` the ha-bridge configs that have no same-id replacement.
pub fn retired_topics(t: &Topics, cfg: &MqttConfig) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if cfg.legacy_ids {
        out.extend(
            crate::topics::LEGACY_RETIRED_DISCOVERY
                .iter()
                .map(|x| format!("{}/{x}/config", t.discovery_prefix)),
        );
    }
    for r in &cfg.retired_discovery {
        if !out.contains(r) {
            out.push(r.clone());
        }
    }
    out
}

/// topic → serialized payload, for change detection.
pub fn index(msgs: &[DiscoveryMessage]) -> IndexMap<String, String> {
    msgs.iter()
        .map(|m| (m.topic.clone(), m.payload.to_string()))
        .collect()
}
