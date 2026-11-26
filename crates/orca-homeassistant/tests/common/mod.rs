#![allow(dead_code)]

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use orca_homeassistant::commands::{ControlOutcome, ControlPort, FanMode, ServicePrincipal};
use orca_homeassistant::model::*;

/// DeepThought as surveyed 2026-11-26 (SUPERVISOR.md 3.1): A has no clock cap (allow_cap = false), D has no HBM sensor.
pub fn deepthought() -> HaSnapshot {
    let card = |id: &str, gpu: &str| CardState {
        id: id.into(),
        gpu: Some(gpu.into()),
        present: true,
        online: true,
        cap_offered: true,
        ..Default::default()
    };
    let mut a = card("A", "V100-SXM3-32GB");
    a.cap_offered = false;
    a.core_c = Some(41.0);
    a.hbm_c = Some(44.0);
    a.power_w = Some(61.23);
    a.power_limit_w = Some(350.0);
    a.sm_clock_mhz = Some(1597.0);
    a.util_pct = Some(0.0);
    a.vram_used_gb = Some(28.4);
    a.vram_total_gb = Some(32.0);
    a.clock_locked = Some(true);
    a.engine = Some("strata".into());
    a.engine_state = Some("ready".into());
    a.model = Some("inkling-2-mini".into());
    a.summary = Some("Inkling 2 Mini (strata, 4 slots)".into());
    a.tok_s = Some(0.0);
    a.running = Some(0);
    a.queued = Some(0);
    let mut b = a.clone();
    b.id = "B".into();
    b.gpu = Some("V100-PCIe-32GB".into());
    b.cap_offered = true;
    b.core_c = Some(26.0);
    b.hbm_c = Some(24.0);
    b.power_w = Some(24.27);
    b.power_limit_w = Some(250.0);
    b.sm_clock_mhz = Some(135.0);
    b.clock_cap_mhz = Some(1300);
    b.tok_s = Some(37.25);
    let mut c = card("C", "V100-SXM2-16GB");
    c.core_c = Some(33.0);
    c.hbm_c = Some(35.0);
    c.engine = Some("comfyui".into());
    c.engine_state = Some("idle".into());
    c.throttle_reasons = vec!["sw_power_cap".into()];
    let mut d = card("D", "Quadro M4000");
    d.core_c = Some(38.0);
    d.hbm_c = None;
    d.online = false;
    let fan = |id: &str, card: &str, duty: f64| FanState {
        id: id.into(),
        name: None,
        cools: vec![card.into()],
        duty_pct: Some(duty),
        rpm: Some(1234.4),
        control_temp_c: Some(44.0),
        source: Some(format!("gpu.{card}.hbm")),
        push_age_s: Some(1.2),
        failsafe: Some(false),
    };
    HaSnapshot {
        server: ServerInfo {
            id: "deepthought".into(),
            name: "DeepThought".into(),
            model: Some("4-GPU server".into()),
            manufacturer: None,
            version: Some("0.16.6".into()),
            config_rev: Some("r42".into()),
            hostd: Some("ok".into()),
        },
        cards: vec![a, b, c, d],
        fans: vec![
            fan("0", "B", 30.0),
            fan("1", "A", 34.0),
            fan("2", "C", 30.0),
        ],
        engines: vec![
            EngineState {
                id: "strata-a".into(),
                title: Some("Strata (card A)".into()),
                kind: Some("llm".into()),
                cards: vec!["A".into()],
                state: Some("ready".into()),
                model: Some("inkling-2-mini".into()),
                tok_s: Some(0.0),
                healthy: Some(true),
            },
            EngineState {
                id: "comfyui".into(),
                title: Some("ComfyUI".into()),
                kind: Some("image".into()),
                cards: vec!["C".into()],
                state: Some("idle".into()),
                model: None,
                tok_s: None,
                healthy: Some(true),
            },
        ],
        host: HostState {
            cpu_pct: Some(3.25),
            ram_total_gb: Some(503.0),
            ram_used_gb: Some(310.0),
            ram_arc_gb: Some(180.0),
            ram_consumed_gb: Some(130.0),
            ram_free_gb: Some(193.0),
            net_in_mbps: Some(1.5),
            net_out_mbps: Some(0.4),
        },
        control: ControlState {
            layout: Some("merged-inkling-x2".into()),
            layout_title: Some("Inkling 2 Mini ('merged') on both cards".into()),
            layouts: vec![
                "flash-x2".into(),
                "dense-stock-x2".into(),
                "merged-inkling-x2".into(),
            ],
            switch_state: Some("idle".into()),
            last_switch: Some(
                serde_json::json!({"at": "2026-11-26T08:00:00Z", "from": "flash-x2", "to": "merged-inkling-x2"}),
            ),
            on_demand: Some("off".into()),
            leases_held: Some(0),
            slots_busy: Some(0),
            slots_total: Some(8),
            bench_running: Some(false),
            calendar_block: Some(false),
            event_active: None,
            problem: Some(false),
            open_alerts: vec![],
            fan_mode: Some("auto".into()),
        },
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    Switch(String, String),
    CardOnline(String, String, bool),
    ClockCap(String, String, Option<u32>, u64),
    Fan(String, FanMode),
}

/// A control port behind fake guards: refuses switches while `bench` is set, refuses taking
/// a card offline while it holds a lease, and records every call with its principal.
#[derive(Default)]
pub struct FakeControl {
    pub calls: Mutex<Vec<Call>>,
    pub bench: Mutex<bool>,
    pub leased_cards: Mutex<Vec<String>>,
}

#[async_trait]
impl ControlPort for FakeControl {
    async fn switch_layout(&self, who: &ServicePrincipal, layout: &str) -> ControlOutcome {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Switch(who.name.clone(), layout.into()));
        if *self.bench.lock().unwrap() {
            return ControlOutcome::Refused {
                reason: "guard bench: the bench queue is running".into(),
            };
        }
        ControlOutcome::Accepted {
            message: "queued, about 95 s".into(),
            job: Some("job-1".into()),
        }
    }
    async fn set_card_online(
        &self,
        who: &ServicePrincipal,
        card: &str,
        online: bool,
    ) -> ControlOutcome {
        self.calls
            .lock()
            .unwrap()
            .push(Call::CardOnline(who.name.clone(), card.into(), online));
        if !online && self.leased_cards.lock().unwrap().iter().any(|c| c == card) {
            return ControlOutcome::Refused {
                reason: format!("card {card} holds a lease"),
            };
        }
        ControlOutcome::Accepted {
            message: "ok".into(),
            job: None,
        }
    }
    async fn set_clock_cap(
        &self,
        who: &ServicePrincipal,
        card: &str,
        mhz: Option<u32>,
        ttl: Duration,
    ) -> ControlOutcome {
        self.calls.lock().unwrap().push(Call::ClockCap(
            who.name.clone(),
            card.into(),
            mhz,
            ttl.as_secs(),
        ));
        ControlOutcome::Accepted {
            message: "capped".into(),
            job: None,
        }
    }
    async fn set_fan_mode(
        &self,
        who: &ServicePrincipal,
        mode: FanMode,
        _ttl: Option<Duration>,
    ) -> ControlOutcome {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Fan(who.name.clone(), mode));
        ControlOutcome::Accepted {
            message: "ok".into(),
            job: None,
        }
    }
}

pub fn mqtt_config(port: u16, legacy: bool) -> orca_homeassistant::MqttConfig {
    toml::from_str(&format!(
        "enabled = true\nurl = \"mqtt://127.0.0.1:{port}\"\nclient_id = \"abhoth-test\"\nlegacy_ids = {legacy}\nstate_interval_seconds = 1\nkeep_alive_seconds = 5"
    ))
    .unwrap()
}
