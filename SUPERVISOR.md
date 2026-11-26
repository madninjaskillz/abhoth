# The Abhoth Supervisor (design)

Status (2026-11-26): **design; building started.** The open questions of section 15 are resolved (15). This document grows the router control
plane of [ROUTER-CONFIG.md](ROUTER-CONFIG.md) into **`abhoth supervisor`**, a single control
plane for the GPU host. The router, which handles layouts, engines, routes, leases and
adapters, becomes one subsystem of it. The new subsystems are:

- live telemetry and the **Glance** dashboard inside the Abhoth web UI;
- **fan control** with configurable sensors and pluggable backends;
- **GPU clocks and power**;
- **media engines**: ComfyUI, image-api and SA3;
- **alerts**;
- **Home Assistant over MQTT discovery**;
- a generic **calendar**, with per-event API keys.

The supervisor retires these:

| Retired | What it is today |
|---|---|
| deepthought-dash | `:8077`, with its root cron collectors `hoststat.py` and `bmcstat.py` |
| `gpu-fans-push.service` | `push-gpu-fans.py` |
| clockctl | `:9877` |
| gpu-power | `:9880`, with its failsafe cron |
| the PREINIT `boot.sh` | the nvidia boot script |
| ha-bridge | the MQTT discovery bridge |
| the special-case guards | the video-pipeline and quiet-window guards |

It does **not** change what ROUTER-CONFIG.md's three builders are doing on
`feature/router-control`:

- **rc-core**: config, importer, SSH, drivers and lifecycle;
- **rc-switch**: switching, guards, on-demand and leases;
- **rc-ui**: accounts, the admin UI and metrics.

Every addition here is a new crate, a new top-level config table or a new web UI page. The
places where it touches their code are named in section 14, and each one waits for their
foundation.

The survey was read-only, done on 2026-11-26 on the GPU host and on the workstation that holds
the ESPHome project. Host-specific values are placeholders:

- `gpu-host`, `$AI_ROOT` and `$APPS` (the apps pool);
- `esp-gpu-fans.lan` (the fan controller);
- `ha.lan` (Home Assistant and its MQTT broker);
- `GPU-aaaa…` (card UUIDs).

No secret was read or printed. The fan controller's environment file was read only for the
key names and the fan map.

## Contents

0. [Scope and decisions](#0-scope-and-decisions)
1. [Naming and command line](#1-naming-and-command-line)
2. [Architecture](#2-architecture)
3. [What exists today](#3-what-exists-today)
4. [Sensors and expressions](#4-sensors-and-expressions)
5. [Fan control](#5-fan-control)
6. [GPU clocks and power](#6-gpu-clocks-and-power)
7. [Media engines](#7-media-engines)
8. [The calendar](#8-the-calendar)
9. [Event API keys](#9-event-api-keys)
10. [Alerts](#10-alerts)
11. [Home Assistant over MQTT](#11-home-assistant-over-mqtt)
12. [Glance in the Abhoth web UI](#12-glance-in-the-abhoth-web-ui)
13. [Abhoth as a supervisor client](#13-abhoth-as-a-supervisor-client)
14. [Build plan](#14-build-plan)
15. [Open questions (resolved)](#15-open-questions-resolved)
16. [Decisions in this document](#16-decisions-in-this-document)
- [Appendix A: the ESP32 fan controller (pinout, wiring, BOM, firmware)](#appendix-a-the-esp32-fan-controller)

The config schema additions and the annotated DeepThought example are in
[ROUTER-CONFIG.md 4.11](ROUTER-CONFIG.md#411-supervisor-additions).

---

## 0. Scope and decisions

User decisions, 2026-11-26. These are fixed inputs.

| # | Decision |
|---|---|
| S1 | **Name:** the Abhoth Supervisor (`abhoth supervisor`). The router becomes one of its subsystems; `abhoth router` stays as an alias. |
| S2 | **Glance is rebuilt inside the Abhoth web UI** as a dashboard page, fed live by the supervisor. It has an optional public read-only view. The Python dash container is retired once the new page reaches parity, which needs a parity table. |
| S3 | **Fan control:** a configurable temperature source per fan or zone: any sensor, or an expression such as max or avg, with fallbacks. The power/load feed-forward is configurable. Backends are pluggable: ESPHome, hwmon, IPMI, liquidctl, NVIDIA and Windows. **Fail-safe to full speed on stale data, preferably on the device.** The fan pusher is reproduced in config and then retired. The ESPHome YAML, the pinout and a BOM are included. |
| S4 | **Home Assistant over MQTT discovery:** sensors, controls and availability. Credentials come only from env or file references. |
| S5 | **Also absorbed:** GPU clocks and power (clockctl, gpu-power, boot script; card A at its full 1597 MHz, re-asserted after engine starts; **revised 2026-11-26: "unlocked" boost, not pinned**, see D-SV6); ComfyUI, image-api and SA3 as engines; alerts; and a generic calendar that replaces the special-case guards, with live detection, a UI, iCal import and today's cron pipelines imported. |
| S6 | **Event API keys** (added 2026-11-26): an event can have its own key. Inside the event window, that key bypasses the event's own reservations and blocks. It can carry admin-chosen control scopes, defaulting to least privilege. It is time-bound, uses D140 key storage, is audited, has explicit priority rules, and can never bypass the probe rule, the excluded cards or thermal safety. |
| S7 | **Abhoth as a supervisor client** (added 2026-11-26): the app (CLI, web UI, desktop) points at a supervisor with one URL and key, and learns models, capabilities, warm/cold state, layouts, leases, calendar and status from it. Manual providers still work alongside it; existing installs migrate without loss. |

## 1. Naming and command line

| Thing | Today (ROUTER-CONFIG.md) | Supervisor |
|---|---|---|
| Binary | `abhoth` | `abhoth` (unchanged, one binary) |
| Serve command | `abhoth router serve` | **`abhoth supervisor serve`**. `abhoth router serve` is an alias that runs the same thing. A config without supervisor tables behaves exactly like the router. |
| Admin subcommands | `abhoth router config check`, `ssh keygen`, `hostctl export`, `import` | `abhoth supervisor …` with the same subcommands. `abhoth router …` maps to them one to one. New: `calendar check`, `calendar import-cron`, `calendar import-ics`, `fans test`, `hostd policy show` |
| Host side | `abhoth hostctl serve/exec/install` (forced-command SSH) | unchanged, plus **`abhoth hostd`**, the long-running host daemon (2.3), and `abhoth hostd boot` / `abhoth hostd failsafe` |
| Config file | `router.toml` | **`supervisor.toml`**. If it is missing, `router.toml` is loaded. Supervisor tables can live in includes (`include = ["fans.toml", "calendar.toml"]`). |
| Crate | `orca-router` | `orca-router` keeps its name. The new crates are in 2.2. |
| Container | `abhoth-router` | `abhoth-supervisor` from the cutover onwards. The old name is kept until then, so nothing on the host has to change twice. |
| Metrics | `abhoth_router_*` | kept. New: `abhoth_fan_*`, `abhoth_gpu_*`, `abhoth_calendar_*`, `abhoth_alert_*` |
| Web UI | admin pages (ROUTER-CONFIG.md 11) | under **Supervisor**: Glance, Router, Fans, Power, Calendar, Alerts, Home Assistant |

## 2. Architecture

### 2.1 Picture

```
                      browsers (LAN, signed in)           public (NPM, https)
                                │                                  │
                     Abhoth web UI (abhoth-web)          /public/glance (read-only feed,
                     Glance · Router · Fans · Power      separate listener, redacted)
                     Calendar · Alerts · HA pages                  │
                                │ admin API + SSE                  │
 ┌──────────────────────────────▼──────────────────────────────────▼───────────────────┐
 │ abhoth supervisor  (container, unprivileged, no docker socket, no /dev/nvidia*)      │
 │                                                                                      │
 │  router (data plane, ROUTER.md)      control (ROUTER-CONFIG.md: engines, layouts,    │
 │  adapters · upstreams · budgets      switches, guards, leases, on-demand)            │
 │                                                                                      │
 │  telemetry hub ─► feed (snapshot + SSE)   calendar   alerts   mqtt/ha   policies     │
 │      ▲                                     │  ▲        │         │      (fans,       │
 │      │                                     ▼  │        ▼         ▼       clocks,     │
 │      │                       guards / placement / on-demand   HA broker  idle)       │
 └──────┼─────────────────────────────────────────────────────────────────────┬─────────┘
        │ one SSH connection per server (russh, pinned key, forced command)   │
        │ channels: hostctl exec (allowlist)  ·  hostd (telemetry, policy)     │
 ┌──────▼──────────────────────────────────────────────────────────────────────▼────────┐
 │ gpu-host                                                                             │
 │  abhoth hostctl serve ─(relay)─► /run/abhoth/hostd.sock                              │
 │                                                                                      │
 │  abhoth hostd  (root, systemd unit, watchdog)                                        │
 │    NVML sampler (500 ms) · host facts (containers, zpool, OOM, procs) · BMC reader   │
 │    fan loop ──► backends: ESPHome REST/SSE · hwmon · ipmitool · liquidctl · NVML fan │
 │    clocks/power enforcer · idle clock-down + wake (router RPC, packet sniff, util)   │
 │    last-known-good policy on disk · limits file (root-owned, can't be widened)       │
 └──────────────────────────────────────┬───────────────────────────────────────────────┘
                                        │ HTTP (digest auth): temps + load in, SSE out
                                  ESP32 fan controller (device-side curve; 100 % if no
                                  fresh data for 30 s, at boot, and on any crash)
```

### 2.2 Crates and modules

New crates only, so nothing collides with the files the builders are editing.

| Crate | Content | IO? | Depends on |
|---|---|---|---|
| `orca-sensors` | Sensor ids, readings with timestamps, the expression language (4.2): parser, evaluator, EMA, gate, fallbacks, staleness | no (pure) | — |
| `orca-fans` | Fan policy (curves, feed-forward, ramp, caps), the `FanBackend` trait, and the ESPHome, hwmon, IPMI, liquidctl, NVML and Windows backends, plus a fake backend for tests | backends only | `orca-sensors` |
| `orca-gpuctl` | Clock and power policy (pin, cap, app clocks, limits), idle clock-down state machine, wake sources, the NVML adapter (`nvml-wrapper`) with an `nvidia-smi` fallback | NVML | `orca-sensors` |
| `orca-hostd` | The host daemon: sampler, host facts (ports `hoststat.py`/`bmcstat.py`), socket protocol, policy validation against limits, last-known-good store, watchdog | host | `orca-fans`, `orca-gpuctl` |
| `orca-calendar` | Entries, RRULE (`rrule` crate), cron (`croner`), iCal import (`icalendar`), occurrence expansion in a time zone (`chrono-tz`), effects, live-detection merge, conflict resolution, event-key scopes (9) | no (pure) | — |
| `orca-alerts` | Rules, conditions over sensors and events, dedupe, cooldown, grouping, and sinks (HA notify, MQTT, ntfy, webhook) | sinks | `orca-sensors` |
| `orca-homeassistant` | MQTT client (`rumqttc`), discovery model, legacy id map, command handling, HA REST client (state reads such as EcoFlow, and notify) | network | — |
| `orca-supervisor` | The umbrella: subsystem registry, config tables for the new parts, telemetry hub, the event bus, the Glance feed (snapshot, SSE, public redaction), and wiring between calendar, guards and alerts | — | all of the above + `orca-router` |

The CLI gets `crates/orca-cli/src/supervisor.rs` and `hostd.rs`, each a new file plus one
registration line. The web UI gets new pages under `client/src/pages/supervisor/` plus one
route entry each.

### 2.3 Process model and privileges: the decision

**Two processes from the same binary:**

1. `abhoth supervisor` runs in a container, unprivileged.
2. `abhoth hostd` runs on the host as root, under systemd.

hostd is reached only through the existing forced-command SSH channel, and only with policies
that a root-owned limits file bounds.

| Needs | Where it runs | Why |
|---|---|---|
| Web listeners, accounts, config editing, MQTT, HA REST, calendar, alerts, the data plane | **supervisor container** | Parses untrusted input and has a large dependency surface. Unprivileged, read-only rootfs, no docker socket (as D-RC4) |
| NVML writes (`-lgc`, `-pl`, `-ac`, `-pm`, fan speed), `/dev/ipmi0`, hwmon `pwm*` sysfs, AF_PACKET sniffing for wake, `journalctl -k` OOMs, `/proc/*/cgroup` of all processes, `zpool list` | **hostd on the host** | All of them need root or near-root. Giving them to a container means `privileged` plus devices plus `CAP_NET_RAW`, which is root anyway, but web-facing. |
| Docker, compose and `midclt` for engines | **hostctl exec** (allowlisted templates, D-RC4) | unchanged |

Why not everything in one privileged container:

- The **cooling and clock loops must outlive the web process.** TrueNAS app redeploys, image
  pulls, OOM kills and config mistakes restart containers.
- With hostd separate, a supervisor restart changes nothing on the hardware: hostd keeps its
  last-known-good policy and goes on pushing temperatures.
- If hostd itself dies, the ESP fails safe to 100 % after 30 s.
- If hostd is gone for more than 2 minutes, the `hostd failsafe` cron unlocks every managed
  card's clocks, as gpu-power's dead-man cron does today.

Why not the whole supervisor on the host: it would be a root process serving HTTP to the LAN
and the internet (the public Glance). No.

**hostd's protection against a compromised supervisor:**

- **The limits file** (`$APPS/abhoth/etc/hostd-limits.toml`, root-owned 0644) defines:
  - allowed GPU UUIDs per action;
  - clock ranges per card;
  - power-limit ranges;
  - **minimum fan duty**;
  - **maximum control temperature** (the cap, 5.4);
  - the minimum stale timeout;
  - which backends may write (for example IPMI is read-only unless set here).

  A policy from the supervisor that falls outside these limits is refused in full, and the old
  policy stays.
- **No command text.** The protocol has typed messages only (2.4).
- **Overrides expire.** Every override (fan duty, hold, cap) has a TTL of at most 1 hour.
- **Hard floor.** hostd never pushes a temperature lower than the measured one; feed-forward
  only adds. The device curve and the critical temperature (85 °C on the ESP) stay in charge.

**Installing on TrueNAS:**

- The root filesystem and `/etc` are replaced on updates, so the binary and the files live on
  the apps pool:
  - binary: `$APPS/abhoth/bin/abhoth`;
  - limits and allowlist: `$APPS/abhoth/etc/`;
  - state: `$APPS/abhoth/state/`.
- A POSTINIT init script starts hostd, the same way `gpu-fans-esphome/start.sh` starts today's
  pusher:

  ```
  systemd-run --unit=abhoth-hostd -p Restart=always -p RestartSec=2 -p WatchdogSec=10 \
    $APPS/abhoth/bin/abhoth hostd --limits $APPS/abhoth/etc/hostd-limits.toml
  ```

- A PREINIT init script runs `abhoth hostd boot`, which replaces `boot.sh` (6.2).
- A TrueNAS cron job runs `abhoth hostd failsafe` every minute.
- The hostctl allowlist (D-RC4) moves to the same `etc/` directory for the same reason. This
  is a note for rc-core: `/etc/abhoth` does not survive TrueNAS updates.

**Remote or extra servers** may run hostctl without hostd. Then they get telemetry by SSH
polling (ROUTER-CONFIG.md 6.5) but no fan, clock or idle control. Validation says so.

### 2.4 The hostd protocol

The protocol is framed JSON over the hostd SSH channel. `hostctl serve` relays it to
`/run/abhoth/hostd.sock` (root:abhoth-exec 0660). Typed messages only:

| Message | Direction | Content |
|---|---|---|
| `hello` | both | Versions, limits hash, policy rev on disk |
| `telemetry.subscribe` | sup → hostd | Rates. Then a stream of NVML samples per UUID (every 500 ms), fan backend readings, BMC (every 10 s), host facts (every 5 s: containers with safe hints only, GPU processes → containers, ports, zpool, OOM kills, `proc-scan` results for guards and calendar detection) |
| `policy.put` | sup → hostd | `{rev, fans, clocks, idle, sensors}`, compiled from config. hostd validates it against the limits, then persists and applies it atomically. The reply is `applied`, or `refused` with the reasons. |
| `wake` | sup → hostd | `{card}`. Sent by the router on admission, before it proxies a request (6.3) |
| `hold` / `release` | sup → hostd | `{card or all, ttl ≤ 1 h, reason}`: no idle clock-down |
| `override` | sup → hostd | `{fan, duty, ttl ≤ 1 h}` or `{card, clock_cap, ttl}`. Floors and caps still apply. |
| `assert` | sup → hostd | `{card}`: re-check and fix clocks, power limit and persistence now. Sent after engine `Ready` (6.2). |
| `status` | sup → hostd | Policy in force, per-loop health, the last error per backend |

hostd also writes a heartbeat file every 5 s. The failsafe cron reads it.

### 2.5 Telemetry hub and feed

The telemetry hub in the supervisor merges five sources:

- hostd samples;
- engine metrics: Strata `/metrics`, llama.cpp `/slots`, ninfer logs, polled as the dash does
  today (README "Polling");
- leases and slots;
- the calendar;
- external readings: HA states and MQTT topics.

It keeps the ring buffers for sparklines (10 minutes at 2.5 s, as today, and 24 hours at 1
minute). It publishes three things:

- **`GET /api/v1/supervisor/snapshot`**: the full state, the replacement for `/api/state`.
- **`GET /api/v1/supervisor/stream`** (SSE):
  - a `snapshot` event on connect;
  - `patch` events (JSON merge patches) at most once a second for fast metrics;
  - immediate `event` messages (switch steps, alerts, calendar transitions, fan failsafe).

  It uses SSE rather than websocket because the feed is one-way, survives NPM and other
  proxies without upgrade handling, reconnects on its own with `Last-Event-ID`, and matches
  the admin SSE events of rc-ui (C8).
- **Prometheus `/metrics`**, extended. Grafana and Prometheus stay. The nvidia_gpu_exporter can
  go later, because hostd's samples are exported.

## 3. What exists today

The read-only survey, 2026-11-26.

### 3.1 GPUs (index · name · bus · role)

| Card | GPU | Bus | Role |
|---|---|---|---|
| A | V100-SXM3-32GB | 21:00.0 | Layout card. 350 W limit, max SM clock 1597, app clock 1380. |
| B | V100-PCIe-32GB | 01:00.0 | Layout card. 250 W, app clocks 877,1380 (boot.sh). |
| C | V100-SXM2-16GB | E1:00.0 | ComfyUI, exempt from quiet mode, idle clock-down |
| D | Quadro M4000 | 44:00.0 | 120 W, **has its own fan**, **no HBM sensor** |

The three V100s are passive. Each has a **duct fan** on the ESP32: fan0 cools card B, fan1
cools card A, fan2 cools card C (`FAN_MAP="01:00.0 21:00.0 E1:00.0"`, load-tested 2026-11-21).
The BMC runs only the CPU fans, in "Optimal" mode.

### 3.2 Fans

**ESPHome firmware** (`gpu-fans.yaml`, now in
[`docs/hardware/esp32-gpu-fans/`](docs/hardware/esp32-gpu-fans/gpu-fans.yaml)):

- **The device runs the curve itself.** Per fan, the target duty is the larger of:
  - the temperature curve: Min Duty 30 % below 45 °C, a linear ramp to 100 % at 78 °C;
  - the load feed-forward: Min Duty at or below 20 % load, a linear ramp to FF Max Duty 80 %
    at 90 % load.
- Critical Temp 85 °C sets 100 %, even in manual mode.
- Ramp-up is immediate; ramp-down is 2 % per 2 s tick.
- Each fan goes to **100 % if it has received no temperature for `stale_seconds` = 30 s**,
  and at boot.
- The PWM pins float to 100 % while the ESP is unpowered or crashed.
- Manual mode holds only while HA is connected; 60 s after HA goes away it returns to auto.
- Telemetry: RPM (tach, 2 pulses per revolution), duty, FF duty, failsafe and stalled binary
  sensors, WiFi signal.
- Two interfaces:
  - the encrypted native API for HA;
  - `web_server` v3 with digest auth: REST `POST /number/<name>/set` and SSE `/events`.

**Pusher** (`push-gpu-fans.py`, a host systemd unit started by a POSTINIT script):

- **Input:** one `nvidia-smi … -lms 500` stream.
- **Control temperature per GPU:** `TEMP_SRC` = `hbm` (falls back to core when N/A), `core`
  or `max`, smoothed by an EMA with τ = 4 s.
- **Load:** `power.draw / enforced.power.limit × 100`. It is gated by temperature:
  `load × clamp((T − 52)/(64 − 52))`, so a cold card under load does not spin up its fan.
- **Fan map:** each fan follows one bus id, `bus+bus` (the max of several; all must be
  present) or `max`.
- **Sending:** it sends on a change of 1 °C or more, or 10 % load or more (at most once a
  second), and as a heartbeat every 10 s. It sends `Power` first, then `Temp`; the temperature
  is what marks a fan fresh on the ESP. Load is clamped to 0-150.
- **Missing or stale data:** a missing or stale GPU (sample older than 3 s) means nothing is
  sent, so the ESP fails safe 30 s later.
- **Hung stream:** after 10 s it kills the stream and restarts it, with a 5 s retry.

**Retired already:**

- `gpu-fan-control` (`:9878`, quiet mode, tiers), 2026-11-21;
- the serial `gpu-fan-esp32` bootstrap (disabled).

### 3.3 Clocks and power

See ROUTER-CONFIG.md 2.5 (`boot.sh`, clockctl `:9877`, gpu-power). gpu-power's internals:

- `IDLE_SECS` 60, then `LOW_MHZ` 135 (`-lgc 135,135`);
- wake sources: an AF_PACKET socket with a BPF filter on GPU container ports, NVML polling,
  and `GET /wake?gpu=`;
- `EXTRA_PORTS` maps ports to cards (`8188` → card C);
- holds through `/run/gpu-power-hold[-<idx|uuid>]` files and `GET /hold?secs=`.

HA's "GPU Max Clock" select calls clockctl `/maxclock` (1200-1500 MHz or no limit). It can
leave card A capped below 1597.

### 3.4 Home Assistant

- **Broker:** Mosquitto on the HA host, plain TCP on 1883, with a user and password.
- **ha-bridge container:**
  - publishes discovery for one device, "DeepThought";
  - publishes state JSON every 10 s on `deepthought/state`, with availability on
    `deepthought/bridge/availability`;
  - entities: GPU Temp, HBM Temp, GPU Power, SM Clock, GPU Utilization, VRAM Used, Model,
    tokens/s, LLM Profile, LLM Card A/B, LLM Profile Switch, Bench Queue Running, LLM Profile
    To Apply (select), Switch To Selected Profile (button) and GPU Max Clock (select);
  - switches go through `POST :8098/switch`.

  Findings:
  - the bridge's container is given **only card B**, so HA's "GPU Temp" and related sensors
    show card B alone;
  - **the MQTT credentials are written in the script**.
- **The ESP32** is in HA natively, through the ESPHome integration.
- **Glance** reads one HA entity (the EcoFlow AC output) over the HA REST API, with a
  long-lived token from `dash.env`.
- **Jarvis** switches layouts through `:8098` as well.

### 3.5 Glance (deepthought-dash)

- **Backend:** `server.py`, a stdlib-only read-only aggregator on `:8077`.
- **Frontend:** React 18 with htm.
- **Root cron collectors:**
  - `hoststat.py` every 15 s: containers, GPU processes → containers, ports, zpool and OOM
    kills, with safe command-line hints only;
  - `bmcstat.py` every 10 s: `ipmitool sdr` and the fan-mode GET.
- **Polling:** engines every 2.5 s, Prometheus every 5 s, services every 20 s, HA every 20 s,
  and SSE from the ESP.
- **Public access:** a public NPM host serves it, with the controls hidden on https.
- **Unauthenticated control:** the card online/offline buttons POST from the browser to the
  unauthenticated `:8098`.

The full panel inventory is in 12.2.

### 3.6 Schedules (TrueNAS cron, read-only)

| Job | Schedule | Notes |
|---|---|---|
| AI News (`run_daily.sh`) | daily 07:00 | Runs observed: 07:00 → 07:28/07:35 on normal days, up to 12:15 and 13:10 on bad days |
| Video Game News | daily 07:00 | `--wait-lock`: starts when AI News ends |
| Car News | Sundays 07:00 | `--after` AI News and Game News |
| Upload retry | every 10 min | Skips 06:30-07:30 and any running `run_daily.sh` (hard-coded) |
| Layout runner, qfn-llm watchdog | every 1 / 2 min | Replaced by the control plane (ROUTER-CONFIG.md) |
| Dash collectors, gpu-power failsafe | every minute | Replaced by hostd and its failsafe |

Guards today (ROUTER-CONFIG.md 4.2):

- the quiet window 06:45-07:30;
- the video process;
- the bench queue file;
- music mode;
- `DENSE_LEAD`.

## 4. Sensors and expressions

### 4.1 Sources

Every reading is `(value, unit, timestamp, source)`. A reading is **fresh** while it is
younger than its source's `stale` setting. Sources are named by path:

| Namespace | Examples | Comes from |
|---|---|---|
| `gpu.<card>.<metric>` | `gpu.A.core`, `gpu.A.hbm`, `gpu.A.hotspot`, `gpu.A.power_w`, `gpu.A.power_pct` (of the enforced limit), `gpu.A.util`, `gpu.A.mem_util`, `gpu.A.sm_mhz`, `gpu.A.mem_used`, `gpu.D.fan_pct` | hostd NVML, 500 ms. A metric the card does not report is **absent**, not 0. The M4000 has no `hbm`; Volta and Maxwell have no `hotspot`. |
| `bmc.<sdr name>` | `bmc."CPU1 Temp"`, `bmc.MB_10G`, `bmc.FAN1` | hostd `ipmitool sdr`, 10 s |
| `hwmon.<chip>.<label>` | `hwmon.k10temp.Tctl`, `hwmon.nvme0.Composite` | hostd sysfs, 2 s |
| `esp.<device>.<entity>` | `esp.gpu-fans."Fan 0 RPM"`, `esp.gpu-fans."Failsafe Active"` | ESPHome SSE |
| `ha.<entity_id>` | `ha.sensor.ecoflow_ac_out_power` | HA REST (or the HA websocket later) |
| `mqtt.<alias>` | `mqtt.room_temp` | topic and JSON path in config |
| `host.<metric>` | `host.cpu`, `host.load1`, `host.mem_avail`, `host.arc`, `host.net_rx` | hostd `/proc` |
| `engine.<id>.<metric>` | `engine.A.tok_s`, `engine.A.running` | telemetry hub |
| `sensor.<name>` | `sensor.duct0_temp` | virtual: a named expression (4.2) |

### 4.2 Expressions

There is a small, total language. It has no loops and no general scripting, and it is
evaluated every 500 ms in hostd (for fan policy) and in the supervisor (for alerts and the
dashboard).

```
expr   := number | source | expr (+|-|*|/) expr | fn "(" args ")"
fn     := max | min | avg            # strict: missing if any argument is missing
        | max_avail | avg_avail      # ignore missing arguments; missing only if all are
        | first                      # the first fresh argument: fallbacks
        | ema(x, 4s)                 # time-constant smoothing (pusher EMA_TAU)
        | clamp(x, lo, hi) | scale(x, in_lo, in_hi, out_lo, out_hi)
        | gate(x, by, lo, hi)        # x * clamp((by - lo)/(hi - lo), 0, 1)
        | age(source)                # seconds since the last reading
```

Examples:

- `first(gpu.D.hbm, gpu.D.core)` is the M4000 rule.
- `max(gpu.A.hbm, gpu.B.hbm)` is one fan for two cards. Strict, like the pusher's `bus+bus`.
- `max_avail(gpu.A.hbm, gpu.A.core - 5)` falls back when one value is missing.
- `gate(gpu.B.power_pct, sensor.duct0_temp, 52, 64)` is today's load gate.

**A missing result is never treated as 0.** For a fan, a missing control temperature means
nothing is pushed (device backends) or 100 % duty (host backends). Validation rejects
expressions whose sources do not exist on that server. It warns about strict functions over
sources that are often absent.

## 5. Fan control

### 5.1 Model

```
[sensors.*]   named expressions (control temps, loads)
[fans.*]      one fan or zone: backend + channel + temp + load (+ gate) + policy
[fan_backends.*]   one device or bus: kind, address, credentials, fail-safe settings
[fan_profiles.*]   named policy overrides (quiet, burst), chosen by the calendar, HA or the UI
```

A fan's `mode` is set by what the backend can do:

- **`device_curve`** (ESPHome, liquidctl devices with profiles, FanControl on Windows): hostd
  pushes the **inputs**, temperature and load, and the device runs the curve. The device is
  the safety authority. hostd can keep the device's curve settings in sync with config
  (`sync_device_settings`; 15 Q3).
- **`host_curve`** (hwmon, IPMI zones, NVML): hostd computes the duty from the same policy
  fields:
  - `curve`;
  - `min_duty`;
  - `feed_forward`;
  - `ramp_down`;
  - `max_temp`.

  The backend's own fail-safe (5.3) covers hostd dying.

Both modes share one policy evaluation, so the dashboard can show "what the curve wants" for
both.

### 5.2 Policy fields

| Field | Meaning | Today |
|---|---|---|
| `temp` | expression → control temperature | `TEMP_SRC=hbm` with fallback to core, EMA 4 s |
| `load` | expression → load % | `power.draw / enforced limit × 100` |
| `load_gate` | `{ by = "temp", lo, hi }` scales load to 0 below `lo` | 52 → 64 °C |
| `curve` | points `[[45, 30], [78, 100]]` (linear between, flat outside) | Curve Low/High, Min Duty |
| `feed_forward` | `{ lo = 20, hi = 90, max_duty = 80 }`: load → duty floor | FF Low/High Load, FF Max Duty |
| `min_duty` | floor in every mode. It is checked against the limits file, which cannot go below it. | 30 % (ESP Min Duty) |
| `max_temp` | the **cap**: at or above it the duty is 100 %, whatever the profile or override | 85 °C (ESP Critical Temp) |
| `ramp_down` | `"2%/2s"`. Ramp-up is always immediate. | ESP: 2 % per 2 s |
| `stale` | input age after which the fan fails safe | 30 s on the ESP, 3 s per sample in the pusher |
| `send` | device backends: `{ temp_delta = 1.0, load_delta = 10, min_gap = "1s", heartbeat = "10s" }` | pusher |
| `profile` | the default profile, overridable by the calendar, HA or the UI | — |
| `alerts` | `{ temp_warn = 75, stalled = true, failsafe = true }` | the dash colours |

A **profile** may lower the curve, for example a `quiet` profile with `curve = [[50, 25],
[80, 100]]` and `feed_forward.max_duty = 60`. It can never change `max_temp` or go below
`min_duty` from the limits file. The `full` profile is 100 % with a TTL.

### 5.3 Backends

| Kind | Talks to | Inputs / outputs | Fail-safe (who, how) | Notes |
|---|---|---|---|---|
| `esphome` | ESPHome `web_server` v3 (REST + SSE, digest auth); `api = "native"` later | push `"<prefix> N Temp"` / `"N Power"` numbers; read RPM, duty, FF duty, failsafe and stalled over SSE | **Device:** no temperature for `stale_seconds` → 100 %. Boot → 100 %. Unpowered/crashed → PWM floats → 100 %. Critical temp → 100 % even in manual. hostd only has to keep sending honest data. | The default. Entity names are configurable (`names = { temp = "GPU {n} Temp", load = "GPU {n} Power" }`). |
| `hwmon` | `/sys/class/hwmon/*/pwmN` (+ `pwmN_enable`, `fanN_input`) | duty 0-255; RPM | **Host:** on start hostd records `pwmN_enable`. On exit, panic or SIGTERM it writes `pwmN_enable = 2` (chip auto) or, with `on_exit = "full"`, 255. A watchdog thread writes full if the loop stalls for more than 5 s. **A SIGKILL or hard hang leaves the last duty**, so a `min_duty` of 40 % or more is recommended. The `hostd failsafe` cron sets full once the heartbeat is stale. | Chip-dependent. Some chips (it87, nct67xx) have a hardware "smart fan" mode as a better fallback (`on_exit = "auto"`). |
| `ipmi` | `ipmitool` on `/dev/ipmi0` | read: `sdr`, fan mode GET (`raw 0x30 0x45 0x00`); write (only if the limits allow it): mode SET (`0x30 0x45 0x01 <mode>`: Standard 0, Full 1, Optimal 2, HeavyIO 4), zone duty (`0x30 0x70 0x66 0x01 <zone> <duty>`) | **BMC:** on exit and on stale data hostd sets the mode back to `restore_mode` (Optimal), so the BMC's own curve takes over. If hostd dies without doing that, the BMC holds the last duty in Full mode, but the BMC's own sensor thresholds still trigger full speed at critical. | **Read-only by default.** Supermicro-style raw commands; other vendors' commands go in a per-vendor table. Today it is read-only (bmcstat). |
| `liquidctl` | `liquidctl` CLI (JSON output) | `set <ch> speed <duty>` or a device-side profile `speed 30 40 50 100` against liquid temp | **Device** for devices that store profiles: hostd uploads a profile, and only "boosts" use fixed duties with a TTL. Otherwise as hwmon. | Commander, Kraken, Smart Device … Probe with `liquidctl list --json`. |
| `nvml` | NVML `nvmlDeviceSetFanSpeed_v2` / `SetDefaultFanSpeed_v2` (driver ≥ 520) | duty per fan of a GPU that has fans (the M4000 here) | **Driver:** on exit hostd calls `SetDefaultFanSpeed_v2`, which gives the GPU back its own curve. The GPU's own thermal slowdown and shutdown stay active. A hard kill leaves the manual speed, so the failsafe cron resets it. | `nvidia-settings` needs X and is not used. Card D's fan is left on auto unless configured. |
| `windows` | LibreHardwareMonitor web JSON (`:8085/data.json`) for reading; the FanControl (Rem0o) "file sensor" plugin for writing | write a control temperature into a file that FanControl's curve uses | **Device-like:** FanControl owns the curve and falls back to its own sensors if the file goes stale (its "sensor lost" handling). | For a Windows box with GPUs: a small `abhoth hostd` build for Windows that only does `windows` + `nvml` read. Low priority. |
| `fake` | memory | — | — | tests and `abhoth supervisor fans test` |

**The safety rules for every backend:**

1. **Fail-safe means full speed, or the hardware's own auto mode,** on stale input, on a hostd
   exit and on a backend error that lasts more than `stale`.
2. **Ramp-up is immediate.** Only ramp-down is slowed.
3. **`max_temp` gives 100 %, always.** It is checked in hostd for host curves and on the device
   for device curves. The limits file has a global `max_control_temp`, and no fan's
   `max_temp` may exceed it.
4. **`min_duty`** is enforced in hostd and, for device curves, also kept on the device.
5. **Alerts:** stale input, device failsafe active, stall (RPM under 300 at more than 25 %
   duty for 15 s, as the ESP does), control temperature at or above `temp_warn`, and backend
   errors (10).

### 5.4 Fan pusher parity

| `push-gpu-fans.py` behaviour | Supervisor |
|---|---|
| One `nvidia-smi -lms 500` stream | The hostd NVML sampler at 500 ms. The same stream feeds telemetry, so there is one reader instead of three (pusher, ha-bridge, exporter). |
| Per-GPU control temp `TEMP_SRC=hbm` → core fallback | `[sensors.cardX_ctrl] expr = "ema(first(gpu.X.hbm, gpu.X.core), 4s)"` |
| `TEMP_SRC=core` / `max` | `ema(gpu.X.core, 4s)` / `ema(max_avail(gpu.X.hbm, gpu.X.core), 4s)` |
| EMA τ 4 s | `ema(…, 4s)` |
| Load = draw / enforced limit × 100, clamped 0-150 | `load = "gpu.X.power_pct"`, clamped by the backend's number range |
| Gate `clamp((T−52)/(64−52))` | `load_gate = { lo = 52, hi = 64 }` (on the fan's temp) |
| FAN_MAP: bus, `bus+bus` (all present), `max` | `temp = "…"` per fan: a card sensor, `max(…)` (strict) or `max(gpu.*.hbm)` |
| Send on ΔT ≥ 1, Δload ≥ 10, min gap 1 s, heartbeat 10 s | `send = { temp_delta = 1.0, load_delta = 10, min_gap = "1s", heartbeat = "10s" }` |
| Power first, then Temp (the temp marks fresh) | the same order in the `esphome` backend (tested) |
| Stale sample 3 s → send nothing | `stale_input = "3s"` |
| Stream hang 10 s → restart, retry 5 s | NVML watchdog: re-init NVML after 10 s without a sample, alert after 30 s |
| Env file with host, auth and map | `[fan_backends.esp-gpu-fans]` with `password = "secret:esp_fans"` (a file) |
| systemd unit with `Restart=always` | the hostd unit |
| Rollback: `push-gpu-temps.sh` | rollback: start `gpu-fans-push` again (kept on disk for 30 days) |

**How it is retired:**

1. hostd runs the fan policy in **shadow** for 3 days. It computes what it would send and logs
   the differences against the pusher, which it reads from the ESP's SSE. It sends nothing.
2. The pusher is stopped and hostd switches to `act`. Thanks to the device's fail-safe,
   nothing can go wrong in between: at worst the fans run at 100 % for 30 s.
3. After 30 days the POSTINIT script is removed.

## 6. GPU clocks and power

### 6.1 Per-card config

The settings go on `[cards.X.power]`, which is new. ROUTER-CONFIG.md's `[cards.X].clocks`
verification becomes part of it.

| Field | Meaning |
|---|---|
| `persistence` | `nvidia-smi -pm 1` at boot and on assert. Without it the driver drops `-lgc` locks. |
| `power_limit_w` | `-pl`. Within the card's min/max and the limits file. |
| `app_clocks` | `[mem, sm]` (`-ac`), e.g. B `[877, 1380]` |
| `serving` | Clocks while an engine on the card is `Ready` or busy. `"unlocked"` (`-rgc`); `{ pin = 1597 }` (`-lgc 1597,1597`); `{ range = [lo, hi] }`; `{ cap = 1400 }` (`-lgc 0,1400`). Card A: `"unlocked"` (it boosts up to 1597 MHz when cool; 15 Q4). **Hardware slowdowns (thermal, power brake) still apply under a lock.** |
| `idle` | `{ after = "60s", lock_mhz = 135, util_at_most = 0, wake = ["router", "sniff", "util"], sniff_ports = [...] }`, or `false` |
| `reassert` | `["boot", "engine_ready", "every:5m", "driver_reload"]` |
| `on_mismatch` | `"fix"` (default for `pin`), `"warn"` or `"fail"` (ROUTER-CONFIG.md 6.6) |

### 6.2 What happens when

- **Boot (PREINIT `abhoth hostd boot`):** persistence, application clocks, power limits and
  `-rgc` on every managed card. Then it exits. This replaces `boot.sh`.
- **hostd start:** reads the last-known-good policy and applies `serving` or `idle` per card
  from the live state.
- **Engine `Ready`** (switch step 10 and reconcile): the supervisor sends `assert {card}`.
  - hostd applies `serving`, checks `clocks.max.sm`, the locked range, the app clocks and the
    power limit, and fixes any mismatch.
  - The result goes into the job step as "A unlocked (max 1597), PL 350 W ✓".
  - This is the **re-assert after engine starts** that nothing does today.
- **Every 5 min:** the same check, cheap. Drift raises a `clock_mismatch` alert and is fixed
  per `on_mismatch`.
- **Switches:** the orchestrator holds idle clock-down for the cards involved (ROUTER-CONFIG.md
  5.4), now through `hold` messages instead of gpu-power's hold files.

### 6.3 Idle clock-down and wake

- **Idle:** utilisation is at or below `util_at_most` for `after`, there is no hold, no
  in-flight request in the router, and no calendar `hold_clocks`. Then hostd locks the card at
  `lock_mhz`.
- **Wake** (whichever comes first, then back to `serving`):
  1. **`router`:** at admission, the router sends `wake {card}` before it proxies to that
     card's engine. It is sub-millisecond on the open channel. This is new and is the main
     path, because the router sees every request first.
  2. **`sniff`:** AF_PACKET with a classic BPF filter on `sniff_ports`, for traffic that
     bypasses the router, such as ComfyUI `:8188` direct and the bench hitting engine ports.
     This is today's mechanism, about 7 ms.
  3. **`util`:** NVML utilisation above 0 on the next sample.
- **Dead-man:** `abhoth hostd failsafe` (cron, every minute). If the heartbeat is older than
  2 min, it runs `-rgc` on every managed card. A dead daemon must never leave a card at
  135 MHz.

### 6.4 Clock-cap and quiet controls

The old "GPU Max Clock" HA select becomes a **per-card cap with a TTL** (`override {card,
clock_cap, ttl}`), exposed in the UI and in HA (11.3). It cannot apply to a card whose
`serving` is `pin` unless `allow_cap = true`. Card A sets `allow_cap = false` explicitly, so
nothing caps its boost below 1597.
The calendar can set a `clock_profile` (for example `quiet = { cap = 1200 }` at night) with
the same rule.

### 6.5 Parity

| Today | Supervisor |
|---|---|
| `boot.sh`: `-pm 1`, `-ac 877,1380` on B | `persistence`, `app_clocks` (PREINIT `hostd boot`) |
| clockctl `/pl` | `power_limit_w` (config) |
| clockctl `/lock` | `serving = { pin / range }` or an override with TTL |
| clockctl `/maxclock` (the HA select) | per-card `cap` override with TTL; refused on pinned cards |
| clockctl `/unlock` (`-rgc` re-applies the cap) | `serving = "unlocked"`; caps are explicit overrides, not hidden state |
| clockctl `/status` | `GET /api/v1/supervisor/cards` (clocks, limits, locks, why) |
| clockctl `exclude` per call | `managed = false` cards are never touched; the limits file lists the allowed UUIDs |
| gpu-power idle 60 s → 135 MHz | `idle = { after = "60s", lock_mhz = 135 }` |
| gpu-power sniff / NVML / `/wake` | wake `["router", "sniff", "util"]`; `POST /admin/api/cards/X/wake` |
| gpu-power `EXTRA_PORTS` (8188 → C) | `sniff_ports` per card, from engine ports automatically, plus extras |
| holds (`/run/gpu-power-hold*`, `/hold?secs=`) | `hold` messages with TTL; the calendar `hold_clocks`; `POST /admin/api/cards/X/hold` |
| gpu-power failsafe cron | `abhoth hostd failsafe` |
| Nothing re-checks A after a switch | `assert` on engine `Ready` and every 5 min |

## 7. Media engines

ComfyUI, the image-api shim and SA3 become `[engines.*]` with drivers from ROUTER-CONFIG.md
4.6, usually `compose` against their TrueNAS apps. They gain three fields:

- **`api`**: `comfyui`, `openai-images` or `sa3`. It decides health, metrics and routes.
- **`lifecycle`**:
  - `standing`: always ensured on its card, outside layouts;
  - `on_request`: started by the first routed request and stopped after `idle_stop`;
  - `layout`: part of a layout like an LLM engine.
- **`depends_on`**: the image-api shim needs ComfyUI.

| Engine | Card | Health | Lifecycle | Routes |
|---|---|---|---|---|
| `comfyui` | C | `GET :8188/system_stats` | `standing` | none directly; behind the shim |
| `image-api` | none (CPU shim) | `GET :8189/health` (it reports backends: enabled, healthy, in flight, queue, served/failed, unet) | `standing`, `depends_on = ["comfyui"]` | `kind = "images"`: `/v1/images/generations` and `/v1/images/edits` → `:8189` |
| `sa3` | configurable (**not pinned today**: the container has the nvidia runtime without a device list) | `GET :8095/health` | `standing`: always running (15 Q9) | `kind = "music"`: `/v1/audio/music` → passthrough to the SA3 API |

Routes get a `kind` (`chat` is the default, then `images`, `audio` and `music`). Access,
budgets, leases and the calendar apply to them as to chat.

Two safety rules from the LLM side carry over:

- **A probe can never start a media engine.** Only authenticated admissions can, as with
  switches.
- **Today's music mode** (a guard file) becomes a calendar effect or an engine conflict:
  `conflicts = ["layout:*@B"]`, so SA3 on card B blocks LLM switches on B. Because SA3 is
  always running (15 Q9), the conflict holds only while it is generating (busy), not merely
  while it is up.

Glance shows them in the Image generation and Services panels (12.2).

## 8. The calendar

### 8.1 Entries

An entry is an event that **reserves** or **blocks** resources while it is active. Entries
live in `calendar.toml` (an include, edited by the UI with `toml_edit`, with backups and
audit like the rest of the config). Live state goes in `router.db`: occurrences, live
extensions and key use.

| Field | Meaning |
|---|---|
| `title`, `owner` | owner = the admin who created it (D140 user) |
| `when` | one of: `at = "2026-12-01T20:00"` + `duration`; `rrule = "FREQ=DAILY;BYHOUR=7;BYMINUTE=0"` + `duration`; `cron = "0 7 * * *"` + `duration`; `ics = "<uid>"` (imported) |
| `tz` | default `router.timezone`. Expansion is in local time, so DST is handled. |
| `lead`, `tail` | blocks start `lead` before and end `tail` after the planned window (e.g. lead 15 m replaces the 06:45 quiet window) |
| `detect` | live detection: `{ process = { argv_any = "run_daily.sh", label_arg = "--channel", label = "ai-news" } }`, `{ lock_file = "…" }`, `{ http = "…", json = "running" }`; `mode = "extend" \| "start" \| "both"`; `grace = "10m"` after the signal ends |
| `after` | sequencing: becomes active when the named entry's occurrence ends (Game News after AI News) |
| `effects` | list (8.2) |
| `priority` | `low` / `normal` (default) / `high` / `critical` (8.4) |
| `key` | the event key (9), optional |
| `enabled`, `until`, `exdates` | the usual |

**Occurrence states:** `scheduled → lead → active → (extended) → tail → done`, or
`skipped`/`cancelled` from the UI for one occurrence. With `detect.mode = "both"`:

- a detected run with no planned occurrence starts an **unplanned occurrence**, so a manual
  pipeline run still gets its protection;
- a planned occurrence whose signal never appears ends at its planned end;
- an occurrence whose signal is still present at the planned end is **extended** while it
  lasts, plus `grace`. This covers the 12:15 and 13:10 runs.

### 8.2 Effects

| Effect | Scope | Consulted by |
|---|---|---|
| `block_switch` | server or card | switch orchestrator (admin, on-demand, HA, Jarvis): refused with `Retry-After` = the remaining occurrence time |
| `block_on_demand` | server, card or route | on-demand `decide()` |
| `block_ensure` | card | reconcile (rare, e.g. maintenance) |
| `reserve_card` | card | placement and leases: requests without the event's key are not placed on the card, and new leases on it go only to the key's holder (9). In-flight work: 9.4. |
| `reserve_slots` | route or card, `n` | slots: `n` lanes withheld for the key's holder (like the Lead reserve) |
| `hold_clocks` | card | idle clock-down suspended (benches) |
| `clock_profile` / `fan_profile` | card / fan | 6.4 / 5.2 profiles (`quiet`); safety caps still apply |
| `mute_alerts` | rule selector | alert sinks (not the dashboard). Never mutes `critical`. |
| `info` | — | shown only (cron jobs that only need to be visible) |

The guards of ROUTER-CONFIG.md 7.1 gain **kind `calendar`** (`{ kind = "calendar", effect =
"block_switch" }`), and the special cases become entries:

| Today's guard | Becomes |
|---|---|
| `quiet-window` (`schedule` 06:45-07:30) | `lead = "15m"` on the AI News entry |
| `video` (`process`) | `detect.process` on each pipeline entry |
| `bench` (`json_file`) | the bench queue creates a one-off entry with its event key when a run starts (or keeps its guard lease; 15 Q10) |
| `music` (`file_exists`) | the SA3 conflict while it is generating (7), or a `music` calendar entry |
| `dense-lead` (`file_exists`) | stays a guard; it is a layout fact, not a schedule |

`schedule` guards remain as sugar: the importer turns them into calendar entries.

**Consulted by:**

- guards, on-demand, the switch orchestrator, placement, leases and slots;
- hostd policies (through `policy.put` on every transition: `hold_clocks`, the profiles);
- alerts;
- the dashboard: a "Now / next" strip, and a block badge on every affected card.

**Scripts** can ask too: `abhoth supervisor calendar check --effect block_switch --card B`
exits with 0 (free) or 1 (blocked, with the reason). Through hostctl it is also available on
the host, so the upload-retry job's hard-coded 06:30-07:30 can become a calendar question
later.

### 8.3 UI and import

**The Calendar page** (admin):

- a week grid and a list;
- a timeline per card of the blocks;
- create and edit with a preview of the next 10 occurrences and their effects;
- skip or cancel one occurrence;
- live state (detected, extended until, by which signal);
- the event key panel (9).

Users see a read-only "busy" view for the cards and routes they can use.

**iCal:**

- **import once** (`calendar import-ics file.ics`): VEVENTs become entries; RRULE, EXDATE and
  time zones are kept;
- **subscribe** (`ics_url`, read-only, refreshed every 15 min): imported entries get default
  effects from the subscription's settings;
- **export:** a read-only `.ics` feed per user, with a token URL, so the blocks show up in a
  phone calendar.

**Cron import** (`calendar import-cron`): it reads `midclt call cronjob.query` through a
hostctl builtin, read-only.

- Known commands (`run_daily.sh` with its channel, the bench) become entries with effects.
- Everything else becomes an `info` entry, so the calendar shows the whole machine's schedule.
- **Cron stays the thing that starts the pipelines.** The calendar describes and protects;
  it does not execute.

**Initial entries:**

| Entry | When | Effects | Detect | Key |
|---|---|---|---|---|
| `ai-news` | daily 07:00, 90 m, lead 15 m | `block_switch` (server), `block_on_demand`, `reserve_card B` | process `run_daily.sh` label `ai-news`, both, grace 10 m | yes: `use_reserved` |
| `game-news` | daily, `after = "ai-news"`, 60 m | same | label `game-news` | yes |
| `car-news` | Sundays, `after = "game-news"`, 60 m | same | label `car-news` | yes |
| `upload-retry` | every 10 min | `info` | — | no |
| `bench` | one-off, created by the bench queue | `block_on_demand`, `hold_clocks` (the cards in use), `reserve_slots` | json_file `queue.json` | yes: `use_reserved`, `switch` limited to bench layouts |

### 8.4 Overlaps and priority

When two active occurrences touch the same resource:

1. **Blocks combine** (union). If either blocks a switch, the switch is blocked. An event key
   bypasses only its own event's blocks, plus the blocks of lower-priority events if its event
   is marked `overrides_lower = true` (9.3).
2. **Reservations of one card by two events:**
   - the higher `priority` holds the card;
   - at **equal** priority, the occurrence that started first keeps it. The other event's key
     gets 409 "card reserved by <event> until ~T" for placement there (its requests fall back
     to the route's other targets as usual);
   - the UI flags the overlap when the entry is saved ("ai-news and bench both reserve B on
     Sundays 07:00-08:30") and asks for a priority. Saving is allowed; it is a warning.
3. `critical` entries (maintenance) beat everything except safety (9.5).
4. A fan or clock profile from two events: **the more conservative one wins.** That means
   the higher fan duty and the higher clock cap, since cooling beats quiet.

## 9. Event API keys

### 9.1 What they are

An event can have **its own API key**. Inside the event's window (plus grace), requests with
that key get through the event's blocks and use its reservations; everyone else is kept off
them. The AI News pipeline, for example, uses its key and gets card B while other users'
requests are placed on A or refused.

Event keys are **D140 API keys of a special kind**:

- They live in the web server's `abhoth.db`, in the same table as `abk_` keys, with
  `kind = "event"` and `event_id`. Only the SHA-256 is stored; the key is shown once.
- They are **owned by the admin who created the event** and can be revoked and rotated.
- The prefix is **`abe_`**, so they are recognisable in logs and by secret scanners.
- **Introspection** (ROUTER-CONFIG.md 11.1) returns `{kind: "event", key_id, event_id, owner,
  scopes, layouts, grace_before, grace_after, priority_bypass}`. The supervisor holds the
  calendar, so it decides validity in time itself. The web server does not need the
  occurrence logic.
- **Recurring events reuse one key** across occurrences. The key works only while an
  occurrence of its event is in `lead`, `active`, `extended` or `tail`, or within
  `grace_before`/`grace_after` of one. Outside those windows it answers **403 "event key
  outside its window (next: 2026-11-27 06:45)"**. It does not fall back to user permissions,
  so a leaked key is useless most of the day.

### 9.2 Scopes

The admin picks the scopes per event, with checkboxes on the event's key panel. **Defaults
are least privilege**: only `chat` and `use_reserved`.

| Scope | Allows (only inside the window) |
|---|---|
| `chat` (default) | chat/images/music on the routes listed in `routes` (default: the routes whose targets include the reserved cards) |
| `use_reserved` (default) | placement on the event's `reserve_card` cards and `reserve_slots` lanes; bypasses the event's own `block_on_demand` for its requests |
| `lease` | take slot leases on reserved cards (the pipeline pins a lane for a run) |
| `switch` | layout switches although `block_switch` is active, **limited to `layouts = [...]`** (empty list = no switch; there is no "any" without the admin ticking "all layouts", which shows a warning) |
| `card_state` | offline/online for the reserved cards only |
| `hold_clocks` | holds on the reserved cards (TTL ≤ the occurrence's end) |
| `fan_profile` | choose a fan profile within the configured profiles (never below safety) |
| `on_demand` | may trigger on-demand switches for routes in `routes`, within the event's cards, bypassing the event's own `block_on_demand` (still subject to 9.5) |

Scopes never grant admin pages, config edits, other events' keys or cards outside the event.

### 9.3 Priority between events

- By default an event key bypasses **only its own event's** blocks and reservations.
- `overrides_lower = true` (admin-set, shown with a warning) lets it also bypass blocks and
  reservations of **lower-priority** events that overlap. It never bypasses equal or higher
  ones.
- **Two keys contend for one card:** the reservation holder (8.4) wins. The other key is
  treated as an ordinary principal for that card.

### 9.4 In-flight work on a reserved card

When a reservation starts, other work may be running on the card. The event chooses one
policy (`on_reserve`); the default is `wait`.

| `on_reserve` | Behaviour |
|---|---|
| **`wait`** (default) | No new placements for others. In-flight requests finish. Other users' leases on the card are not renewed past their current TTL. Event-key requests queue for the reserved lanes first (they jump the queue for that card) and fall back to free lanes. The UI shows "draining for ai-news: 2 requests, ~40 s". |
| `drain` | As `wait`, plus other users' leases are released at once (their holders get `lease revoked: reserved by <event>` on the next renew; ROUTER-CONFIG.md 8). Requests already streaming finish. |
| `preempt` | As `drain`, plus in-flight streams from others on that card are cancelled after `preempt_after` (min 30 s) with a clean error (`503` / SSE error event `preempted_by_reservation`) so clients can retry elsewhere. Admin-only, `priority ≥ high`, shows a warning, audited per cancelled request. |

Bench runs are never preempted. A bench lease counts as `critical` unless the bench's own
event is lower. A switch requested by an event key also waits for the card's drain as in
ROUTER-CONFIG.md 5.4. A key cannot make a switch skip the drain.

### 9.5 What an event key can never do

- **Trigger a switch, an engine start or on-demand from a probe.** The probe rule
  (ROUTER-CONFIG.md D-RC7) is structural: a switch can only come from a chat admission with a
  real body, and `x-abhoth-no-switch` requests and `GET`s never switch, whoever the key
  belongs to.
- **Touch excluded cards or models.** `managed = false` cards, `on_demand = false` cards and
  excluded models (`dense-pi` today) are outside every event key. Validation refuses an
  event whose reservation or switch list names them.
- **Change thermal or fan safety.** Limits-file floors and caps, `max_temp`, `min_duty` and
  device fail-safes are below the policy layer. A key's `fan_profile` and `hold_clocks` go
  through the same hostd validation as anything else.
- **Bypass budgets** for cloud upstreams. Event-key usage is charged to the owner's budget,
  tagged with the event.
- **Act outside its window**, or after the owner is disabled (introspection says
  `disabled`).

### 9.6 Audit

- Every request with an event key is logged in the request log with `event_id`, the
  occurrence and `key_id`.
- Every control action (switch, offline, hold, lease, preempt) goes into the audit table with
  the event, the key and the owner.
- The event page shows uses per occurrence.
- Alerts: `event_key_outside_window` (more than 3 uses in 10 min, which suggests a leak or a
  clock problem) and `event_key_preempt`.

### 9.7 How the pipelines get their key

The keys go in files, never in argv or crontab:

1. The admin opens the `ai-news` event → Key → **Issue**. The key is shown once. The UI offers
   "write to host" using a hostctl builtin (`secret-put`), which writes
   `$AI_ROOT/vp-tools/keys/ai-news.key` with mode 0600, owned by the pipeline user. The UI can
   also show the key for copying.
2. The pipeline env (`vp-tools/env.sh`, already sourced by `run_daily.sh` as `VP_ENV`) gets:

   ```sh
   export ABHOTH_API_KEY_FILE="$AI_ROOT/vp-tools/keys/${VP_CHANNEL:-ai-news}.key"
   ```

   The pipeline's OpenAI client reads `ABHOTH_API_KEY_FILE` (a 3-line change in
   `vp-tools`) and sends `Authorization: Bearer abe_…`.
3. **Rotation:** Rotate issues a new key. The old one keeps working until the end of the next
   occurrence or 24 h, whichever is sooner, so a running pipeline is not cut off. Then it is
   revoked.

**Migration:**

| Step | When | Effect |
|---|---|---|
| 1 | Cutover | Entries `ai-news`, `game-news`, `car-news` created by the importer from cron + the old guards; effects as today (block switch/on-demand, no reservation yet); keys not issued; pipelines still anonymous LAN (ROUTER-CONFIG.md 16, question 6) |
| 2 | +1 week | Keys issued with `chat` + `use_reserved`; key files written; `env.sh` updated; requests show `event_id` in logs |
| 3 | +2 weeks | `reserve_card B` turned on for the pipeline entries (`on_reserve = "wait"`); other users' requests go to A during the window |
| 4 | +30 days | Anonymous LAN access for the pipelines removed |

### 9.8 Config

```toml
[calendar.entries.ai-news]
title    = "AI News daily pipeline"
owner    = "admin"                       # D140 user name (an admin)
rrule    = "FREQ=DAILY;BYHOUR=7;BYMINUTE=0"
duration = "90m"
lead     = "15m"                         # was the 06:45 quiet window
detect   = { process = { argv_any = "run_daily.sh", label_arg = "--channel", label = "ai-news" },
             mode = "both", grace = "10m" }
effects  = [ { block_switch = "server:deepthought" }, { block_on_demand = "server:deepthought" },
             { reserve_card = "B", on_reserve = "wait" } ]
priority = "normal"
[calendar.entries.ai-news.key]           # the key itself is in abhoth.db; this is its policy
scopes        = ["chat", "use_reserved"] # least privilege
routes        = ["qwen3.8-27b-coder"]
layouts       = []                       # `switch` not granted
grace_before  = "5m"
grace_after   = "15m"
overrides_lower = false
```

## 10. Alerts

### 10.1 Rules

```toml
[alerts.rules.hot-card]
when     = "max_avail(gpu.A.core, gpu.B.core, gpu.C.core) >= 85"   # expression (4.2)
for      = "60s"
severity = "warning"                     # info | warning | critical
message  = "GPU {card} at {value}°C"
dedupe   = "card"                        # key part: one open alert per card
cooldown = "30m"                         # don't re-send while open within this
repeat   = "2h"                          # re-send if still open
resolve  = true                          # send "resolved"
sinks    = ["ha", "mqtt"]
```

Built-in **event** rules (no expression needed; on by default, editable):

| Rule | Trigger | Severity |
|---|---|---|
| `oom_kill` | kernel OOM kill (hostd journal follower; process + container) | warning (critical if an engine) |
| `engine_crash` | engine `Failed`, container exit/restart count up, health down > 60 s while it should be `Ready` | critical |
| `switch_failed` | switch failed or rolled back; reconcile gave up | critical |
| `hot_card` | core ≥ 85 °C or HBM ≥ 90 °C for 60 s | warning; critical at throttle reasons `hw_thermal` |
| `fan_stale` | hostd not pushing for > 20 s; ESP `Failsafe Active`; backend errors > `stale` | critical |
| `fan_stalled` | RPM < 300 at > 25 % for 15 s | critical |
| `nic_hot` | `bmc.MB_10G` ≥ 75 (warning) / 85 (critical) | as given |
| `clock_mismatch` | assert found drift (6.2) | warning |
| `hostd_down` | no hostd heartbeat over SSH for 30 s | critical |
| `calendar_overlap` | two reservations collide at start | warning |
| `event_key_*` | 9.6 | warning |
| ROUTER-CONFIG.md 12.4 | guard stale, SSH down, budgets 80/100 %, config reload failing, allowlist out of date | as there |

### 10.2 Dedupe and grouping

- **Identity:** an alert's identity is `rule + dedupe parts` (for example `hot_card/A`).
- **Opening:** one open alert per identity, stored in `router.db` with first, last and count.
- **Re-sending:** only after `repeat`. A flapping alert (resolve then reopen within
  `cooldown`) does not re-send; its count goes up.
- **Grouping:** alerts opened within 30 s that share a cause, such as a switch failure
  followed by `engine_crash` on the same card, are sent as one notification with the
  children listed.
- **Calendar mutes:** `mute_alerts` suppresses sinks but never the UI, and never `critical`.

### 10.3 Sinks

| Sink | How | Secret |
|---|---|---|
| `ha` | HA REST `POST /api/services/notify/<service>` (mobile app notify), with a tag for replacement and `clear_notification` on resolve | HA long-lived token (file) |
| `mqtt` | `abhoth/<server>/alert/<rule>` retained JSON; HA `event` entity "Abhoth alert" and a `binary_sensor` "Abhoth problem" via discovery (11.2) | MQTT credentials |
| `ntfy` | `POST <ntfy-url>/<topic>` with priority mapping | optional token (file) |
| `webhook` | JSON POST | optional header secret (file) |

## 11. Home Assistant over MQTT

### 11.1 Connection

```toml
[mqtt]
url        = "mqtt://ha.lan:1883"         # mqtts:// supported
client_id  = "abhoth-supervisor-deepthought"
username   = { env = "ABHOTH_MQTT_USER" }
password   = { file = "/run/secrets/mqtt-password" }
base_topic = "abhoth"
discovery_prefix = "homeassistant"
legacy_ids = true                         # keep ha-bridge's deepthought_* unique_ids (11.4)
[homeassistant]
url   = "http://ha.lan:8123"
token = { file = "/run/secrets/ha-token" }
reads = { ecoflow_ac = "sensor.<ecoflow ac out entity>" }   # available as ha.<entity> sensors
```

**Availability:**

- `abhoth/<server>/availability` carries `online`/`offline`, with an MQTT **last will** of
  `offline`.
- Card entities also list `abhoth/<server>/card/<X>/availability`, which goes offline when the
  card is `missing`.
- Entities use `availability_mode = "all"`.

**State:** one retained JSON per device, `abhoth/<server>/state` and
`abhoth/<server>/card/<X>/state`, published every 10 s and on change for discrete values.

### 11.2 Devices and entities

- **Devices:** one per server ("DeepThought") and one per card ("DeepThought card A",
  `via_device` = the server). The fan controller is already in HA natively through ESPHome,
  so the supervisor publishes only what it derives: control temperature, source used, profile
  and push freshness.

| Entity | Kind | Device |
|---|---|---|
| Core temp, HBM temp, power W, power %, SM clock, util, VRAM used, engine state, engine, model, tok/s, running, queued | sensor | card |
| Clock pinned/locked, throttling (reasons as attribute) | binary_sensor | card |
| Card online | **switch** (offline/online through the switch orchestrator; refused with an error while blocked) | card |
| Clock cap | select (`none`, 1200, 1300, 1400, 1500), TTL 1 h; unavailable on pinned cards | card |
| Total tok/s, GPU power total, host CPU, RAM used / ARC / free, network in/out | sensor | server |
| Layout (live), switch state (`idle/queued/switching/failed/refused`), last switch, on-demand (`off/on/idle/running`), leases held, slots busy/total | sensor | server |
| **Layout** | **select** (target) + **button** "Apply layout" (kept two-step, as today) | server |
| Bench running, calendar block active, event active (attribute: which), problem (any open alert) | binary_sensor | server |
| Fan mode | select `auto / quiet / full (1 h)` | server (or fan device) |
| Fan N control temp, source, push age | sensor | server |
| Abhoth alert | event | server |
| Supervisor version, config rev, hostd status | sensor (diagnostic) | server |

**Commands:**

- Commands arrive on `abhoth/<server>/set/<obj>`.
- They act as the MQTT **service principal** (a D140 service user with exactly `switch`,
  `card_state`, `clock_cap` and `fan_profile`), so they are audited like any other caller
  and refused by the same guards and calendar.
- HA never gets an event key.

### 11.3 HA parity

| ha-bridge today | Supervisor |
|---|---|
| GPU Temp / HBM Temp / GPU Power / SM Clock / GPU Utilization / VRAM Used (**card B only**) | the same per card (A-D). With `legacy_ids` the old unique_ids map to **card B**, so history continues, and the names say "card B". |
| Model, tokens/sec | `model` per card, total tok/s on the server |
| LLM Profile, LLM Card A/B, LLM Profile Switch | layout, card engine/model sensors, switch state |
| Bench Queue Running | binary_sensor "Bench running" (from the calendar/bench) |
| LLM Profile To Apply (select) + Switch To Selected Profile (button) | Layout select + Apply button (same ids with `legacy_ids`) |
| GPU Max Clock (select → clockctl `/maxclock`, all cards) | per-card Clock cap with TTL; not on pinned A |
| State JSON `deepthought/state`, availability `deepthought/bridge/availability` | `abhoth/<server>/state` + LWT; the old topics are cleared at cutover (retained-empty) |
| Retired entities cleared at startup | same mechanism: the supervisor clears the discovery topics it no longer publishes (list kept in config) |
| Credentials in the script | env/file refs only |
| SSH key to the host as admin | not needed (hostd channel) |

**Read from HA:** the EcoFlow AC out entity (Glance tile) through `[homeassistant].reads`,
polled every 20 s, as today.

## 12. Glance in the Abhoth web UI

### 12.1 Page and access

- **`/supervisor/glance`** in the Abhoth web UI. Signed-in users see it.
- **Controls** (card online/offline, layout, fan mode, holds) render **only for admins** and
  call the authenticated admin API. This fixes today's unauthenticated browser → `:8098`
  POSTs.
- **Data:** the snapshot plus the SSE stream (2.5). The page works with no polling; on
  reconnect it re-syncs from the snapshot.
- **Public read-only view** (`[glance.public] enabled = true`, off by default):
  - served on its **own listener** (`public_listen = "0.0.0.0:8077"`), so NPM forwards only
    that, and the old URL keeps working;
  - feed `/public/glance/stream` with a **redaction profile**;
  - no controls, rate limited, no cookies.

  The default redaction:
  - drops the recent-requests table, OOM process names, collector error text and container
    names;
  - keeps everything else;
  - is configurable per field (15 Q5).

### 12.2 Parity table

Every panel and metric of today's dash. "Source" is where the supervisor gets it.

| Area | Item | Today's source | Supervisor source | Notes |
|---|---|---|---|---|
| Header | Brand, connection age, server time | page | page | |
| Header | Layout pill (detected from running servers: name, servers, slots) | engine probes | control plane state (configured + verified) | better: no guessing from container names |
| Header | Profile pill (switcher's layout, healthy/job) | `:8098 /live` | control plane | |
| Header | On-demand pill (off / on · idle / last switch ≤ 15 min, pending) | `:8098` | control plane | |
| Header | *(new)* calendar "Now / next" strip with block badges | — | calendar | |
| Tiles | Total generation tok/s + sparkline, per-copy split | engine metrics | telemetry hub | |
| Tiles | Requests running / slots, queued, leases held (or "pig-slots down") | engines + `:8099` | router + leases | |
| Tiles | GPU power total, hottest core | Prometheus exporter | hostd NVML | |
| Tiles | Host CPU %, load | `/proc` | hostd | |
| Tiles | Host RAM: used, ARC, consumed, free (+ segmented bar) | `/proc/meminfo` + `host.json` ARC | hostd (`arcstats`) | same formulas (dash 2026-11-25 notes) |
| Tiles | Network in/out, uptime | `/proc` | hostd | |
| Tiles | EcoFlow AC out (HA, age) | HA REST | `[homeassistant].reads` | |
| GPU card | Title, role (LLM roles / ComfyUI / STT / TTS / SA3 / idle), PCI, P-state | exporter + procs | hostd + engines (`roles` from config, not regexes) | |
| GPU card | tok/s, running, queued | engines | hub | |
| GPU card | Per instance: model, engine + version, port, container, ctx, KV type, down + error, tok sparkline | engines + host.json | control plane + hub | |
| GPU card | Slot rows: state chip (idle · cached, prefill %, generating/phase), sparkline, tok/s (* estimated), ctx tokens, lease (kind, label, host, idle) | engines + pig | hub + leases | |
| GPU card | Non-LLM GPU processes (name, process, GiB) / "No GPU processes" | host.json | hostd | |
| GPU card | Temp core / HBM with levels | exporter | hostd | |
| GPU card | Fan: duct duty · rpm · STALLED, or card fan % | ESP SSE / exporter | fan subsystem | adds control source and profile |
| GPU card | Power / limit + bar | exporter | hostd | |
| GPU card | VRAM used / total + bar | exporter | hostd | |
| GPU card | Util compute / memory BW | exporter | hostd | |
| GPU card | SM / max / memory clock | exporter | hostd | adds pin/cap/idle state and "why" |
| GPU card | PCIe gen × width ("lanes") | exporter | hostd | |
| GPU card | Throttle reasons (HW/SW thermal, power cap, HW slowdown, power brake) | exporter | hostd | |
| GPU card | Sparklines: temp (core + HBM), fan duty, power, util | ring buffers | hub ring buffers | |
| Panel | 32 GB cards · online/offline: state, engine/model, slots, detail, VRAM/util, two-click button, job progress | `:8098` | control plane; admin-only buttons | |
| Panel | Services (grouped up/down): model servers; pig-slots, qfn-lb, LiteLLM (+ "no upstream"), abhoth router; ComfyUI, Image API, SA3; Whisper STT, TTS; GPU fan ESP32, Clockctl, Noise meter, Prometheus, Grafana | HTTP/TCP probes | `[glance.services]` + engine health | retired services drop out at cutover |
| Panel | GPU fans · ESP32: live/stale/offline, failsafe, manual; table per fan (card, temp, load, duty, FF, RPM, stalled); duty sparklines; curve/FF/critical settings; WiFi dBm; ESP uptime | ESP SSE | fan subsystem (ESPHome backend SSE) | adds control expression and source used per fan |
| Panel | Noise (loudest mic) value + sparkline | Prometheus `noise_dbfs` | `mqtt.`/Prometheus reading (`[glance.extra]`) | noise meter app unchanged |
| Panel | CPU & board · BMC: live/stale, fan mode, CPU1/CPU2/10G NIC with levels + sparklines, CPU fan RPMs, VRM/DIMM max, system/peripheral | `bmc.json` | hostd BMC reader | |
| Panel | Image generation: image-api status, ComfyUI up, backends (state, model, queue, served/failed) | `:8189/health` | media engines | |
| Panel | Storage pools | host.json `zpool` | hostd | |
| Panel | AI News job (last log: date, status, YouTube Studio links) | log glob | calendar occurrence + log reader | ties to the entry |
| Panel | OOM kills · 7 days (when, process, container, anon MB) | host.json | hostd journal follower | |
| Panel | Model copies · since restart (reqs, out tok, decode, draft acc., cache, up) | engines / ninfer logs | hub | |
| Panel | Recent requests (ago, copy, prompt, cached, out, tok/s, TTFT, time, end) | engines | router request log | richer: route, principal kind, event |
| Footer | Collector warnings, host collector staleness | backend | supervisor health (hostd, SSH, backends) | |
| Footer | ninfer estimate footnote | page | page | |

**Retiring the Python dash:**

1. Run both side by side for one week. The new page is reachable at the old public URL only
   after sign-off.
2. A checklist built from this table is ticked off in the UI.
3. Stop the `deepthought-dash` app and remove the two root cron collectors.
4. Keep the files for 30 days.

## 13. Abhoth as a supervisor client

The Abhoth app (CLI, web UI, desktop) can **point at a supervisor** instead of configuring
each model and endpoint by hand. The supervisor already knows every route, engine, layout,
card, lease and calendar entry, so the app asks it.

### 13.1 One setting

**Settings → Models** gets a "Supervisor" card at the top, above the providers list:

```
┌ Supervisor ─────────────────────────────────────────────────────────────┐
│ URL      [ http://gpu-host:8010                        ]  ● connected   │
│ API key  [ abk_••••••••••••••  ]  (Sign in with the web UI…)  [Test]    │
│ Use for  [x] chat  [x] images  [x] music  [x] slot leases  [x] layouts  │
│ As       alice (admin) · scopes: chat, switch, card_state · rev 412     │
└─────────────────────────────────────────────────────────────────────────┘
```

In `~/.abhoth/config.json` this is a new top-level `supervisor` object, next to `providers`:

```json
"supervisor": {
  "url": "http://gpu-host:8010",
  "apiKeyEnv": "ABHOTH_SUPERVISOR_KEY",
  "apiKeyFile": null,
  "use": { "chat": true, "images": true, "music": true, "slots": true, "layouts": true }
}
```

- **The key** follows the existing provider pattern: `apiKey`, `apiKeyEnv` or `apiKeyFile`.
  The settings dialog stores it like provider keys and never shows it again.
- **"Sign in with the web UI"** gets a key without copy and paste. The app opens
  `<abhoth-web>/connect?client=<name>` in the browser. There the user signs in (D140,
  with 2FA) and approves. The web server mints a per-device `abk_` key, labelled with the
  device name and revocable on the Account page. The app receives it through a localhost
  callback, or a short code for the CLI (`abhoth supervisor login`).
- **The web container** sets this once, server-wide, through env: `ABHOTH_SUPERVISOR_URL`
  and `ABHOTH_SUPERVISOR_TOKEN_FILE`. That token is a service token with an `act_as_user`
  permission. The web server forwards each signed-in user's identity
  (`x-abhoth-act-as: <user id>`, accepted only with that token). So every web user sees what
  **their own** account allows, with no per-user key setup. The supervisor audits both the
  service and the user.

### 13.2 Discovery: the catalog

**Endpoints:**

- `GET /supervisor/v1/catalog` returns one document, filtered to the caller's permissions,
  with an `ETag` (the config rev plus the state rev).
- **`GET /v1/models`** stays OpenAI-shaped for plain clients. Each entry gains an `abhoth`
  object with the same per-model fields, so a generic client can use them too.

```json
{
  "supervisor": { "name": "DeepThought", "version": "0.20.0", "rev": 412 },
  "principal": { "kind": "user", "name": "alice", "role": "admin",
                 "scopes": ["chat", "switch", "card_state", "leases", "calendar_read", "status"],
                 "event": null },
  "models": [
    { "id": "qwen3.8-27b-coder", "kind": "chat", "title": "Qwen 3.8 27B coder",
      "wire": ["openai-chat", "anthropic-messages", "responses"],
      "capabilities": { "context": 131072, "max_output": 32768, "tools": true,
                        "parallel_tools": true, "vision": false,
                        "thinking": { "supported": true, "efforts": ["low", "medium", "high"],
                                      "default": "medium" }, "json_mode": true },
      "state": { "status": "warm", "card": "A", "engine": "ninfer",
                 "lanes": 4, "lanes_free": 3, "tok_s": 41.2 },
      "cold": null,
      "agent": { "mode": "pi", "tool_profile": "pi", "sampling": { "temperature": 0.6 } },
      "source": "local" },
    { "id": "flash-next", "kind": "chat",
      "state": { "status": "cold" },
      "cold": { "policy": "on_demand", "allowed": true, "eta_s": 190,
                "needs_restore": "≈24 GB, ~6 min", "blocked_by": null } },
    { "id": "dense-stock", "kind": "chat",
      "state": { "status": "cold" },
      "cold": { "policy": "fallback", "fallback_to": "qwen3.8-27b-coder" } },
    { "id": "qwen-image", "kind": "images", "sizes": ["1024x1024", "1328x1328"],
      "state": { "status": "warm", "card": "C", "queue": 0 } },
    { "id": "sa3", "kind": "music", "max_seconds": 190,
      "state": { "status": "cold" }, "cold": { "policy": "on_demand", "eta_s": 40 } },
    { "id": "claude-sonnet", "kind": "chat", "source": "cloud",
      "budget": { "period": "month", "left_usd": 61.4 } }
  ],
  "layouts": [ { "id": "merged-inkling-x2", "live": true },
               { "id": "flash-x2", "eta_s": 190 } ],
  "cards": [ { "id": "A", "label": "V100-SXM3 32 GB", "online": true, "engine": "ninfer" } ],
  "calendar": [ { "id": "ai-news", "title": "AI News daily pipeline",
                  "next": "2026-11-27T06:45:00Z", "until": "2026-11-27T08:30:00Z",
                  "effects": ["block_switch", "reserve_card:B"] } ],
  "links": { "stream": "/supervisor/v1/catalog/stream", "leases": "/supervisor/v1/leases",
             "plans": "/admin/api/plans", "jobs": "/admin/api/jobs" }
}
```

(The trimmed entries leave out their capabilities.)

**Kinds:** `chat`, `images`, `music`, `audio` (STT/TTS, later) and `embeddings` (if any route
of that kind exists; none today). `wire` lists the API formats the route accepts through the
adapters (ROUTER-CONFIG.md 10). The app uses the native one for the model and needs no
per-model wire setting.

**Live updates:** `GET /supervisor/v1/catalog/stream` (SSE) sends these events:

| Event | When |
|---|---|
| `catalog` | the full document, on connect and after a config reload |
| `model` | a model's `state` or `cold` changed: warm, cold, loading with `progress` and `eta_s`, unavailable |
| `job` | switch or on-demand job steps, with progress and ETA |
| `calendar` | an occurrence started, was extended or ended |
| `status` | 1 Hz GPU temperatures, power and tok/s, only with `?status=1` and the `status` scope |

The app keeps the last catalog in `~/.abhoth/catalog-cache.json`, replacing
`profiles-cache.json`, so model pickers work offline and while starting up.

### 13.3 What it replaces in the app

| Today (`orca-core` config) | With a supervisor |
|---|---|
| `lmStudio` / `providers.*` entries pointing at the GPU host, each with `models`, `contextWindow`, `maxTokens` | the catalog's chat models and capabilities |
| `profiles.serverUrl` (`:8098` profiles API) + `profiles-cache.json`; the setups' agent `mode` from `profiles.json` v3 (OpenOrca tuning per layout) | catalog `layouts` + `models[].agent` (`mode`, `tool_profile`, sampling), configured in `supervisor.toml` as `[models.X.client]`. A per-project agent mode still wins. |
| `slots` (`registryUrl` = pig-slots `:8099`, `copies`) and the slot client (`orca-core/src/slots`) | the native lease API `/supervisor/v1/leases` (ROUTER-CONFIG.md C7), with the same lease semantics; copies come from the catalog's `state.card`/`lanes` |
| `imageGen.baseUrl` / `model` | the catalog's `images` models (the route goes through the supervisor) |
| hand-made model lists for STT/TTS/music | the catalog's `music`/`audio` models |

The old fields stay readable (13.8). They are **ignored** for anything the supervisor
provides, whenever `supervisor.use.<x>` is on.

### 13.4 What it opens up (by scope)

| Feature in the app | Scope | Uses |
|---|---|---|
| Layout switching (the existing Profiles feature, repointed): pick a layout, **plan preview** (steps, restores, drains, ETA), start, live **progress and ETA**, cancel | `switch` (optionally limited to `layouts`) | `POST /admin/api/plans` (dry run), `POST /admin/api/switch`, `job` events |
| Card online/offline | `card_state` | admin API |
| Calendar: reservations and blocks, "next free" for a card | `calendar_read` (all users get the entries that affect their routes) | catalog `calendar`, `calendar` events |
| Status strip (GPU temps, tok/s, busy lanes) in the status bar and the Models tab | `status` | `status` events |
| Leases for sub-agents and benches | `leases` | lease API |

- **Non-admin keys** get a catalog with their routes only. `layouts` and `cards` are empty
  unless they have the scopes, and the matching buttons are hidden, not disabled.
- **Event keys** (9) work the same way. The principal block carries `event: {id, until}`; the
  app shows "AI News key · valid until 08:30" and disables the model picker outside the window.

### 13.5 Cold models in the UI

The model picker shows each chat model's state from the catalog and the stream:

| State | Shown | On send |
|---|---|---|
| warm | `●` (lanes free) | sent |
| loading | `◐ loading 42 % · ≈1 m 10 s` | waits for the `model` event, then sends; the composer shows progress |
| cold, `on_demand` allowed for this key | `○ will load (≈3 min)` (plus "needs restore ≈24 GB" if so) | asks once ("Load flash-next? ≈3 min, replaces X on card B"), then sends. The router queues the request and starts the switch (ROUTER-CONFIG.md 7.2); the app follows the `job` events. |
| cold, `fallback` | `○ uses qwen3.8-27b-coder instead` | sent; the reply header names the model that answered |
| cold, `reject`, or blocked by the calendar or a guard | `✕ not available now · AI News until ≈08:30` | send disabled; offers the fallback or another model |

**The app never probes a cold model** to learn its state. The catalog replaces probing, and
health checks use `GET` only, which can never switch anything (D-RC7).

### 13.6 Coexistence with manual providers

- **Manual providers still work.** A direct Anthropic, OpenAI, OpenRouter or LM Studio
  provider sits next to the supervisor in `providers`. Pickers group models by source
  ("DeepThought", "Anthropic (direct)").
- **The same cloud model may come both ways**, through the supervisor's cloud upstream (with
  budgets) and directly. The user picks one source; a per-model "prefer" setting remembers
  the choice.
- **Per-session and per-project model choices** store `source` + `id`, so they survive
  either source being removed.
- **Fallbacks:** without a supervisor the app behaves exactly as today. If the supervisor is
  unreachable, cached models are shown as "offline" and manual providers keep working.

### 13.7 The bridge (BRIDGE.md)

- **BRIDGE.md 10.2** (session → machine models through the server's bridge model proxy) is
  unchanged. A bridged session's model list is the **server's** supervisor catalog plus the
  owner's machine models.
- **BRIDGE.md 10.3** (a machine as a per-user router upstream) becomes a supervisor upstream
  of kind `bridge` (ROUTER-CONFIG.md 9.5):
  - it appears in the owner's catalog only, as `@<machine>/<model>`, with `source = "bridge"`;
  - its state is the machine's online/offline;
  - it is never cold-switched or reserved;
  - the catalog's private-name rule matches BRIDGE.md: other principals get "unknown model".
- **A bridged machine** does not need its own supervisor setting. The server's session
  carries it.

### 13.8 Migration

The migration is offered automatically, and can be run by hand with
`abhoth config migrate --to-supervisor`.

1. **Detect.** If `lmStudio.baseUrl`, a provider, `profiles.serverUrl` or `slots.registryUrl`
   point at a host whose `GET /supervisor/v1/catalog` answers (401 with
   `x-abhoth-supervisor: 1` counts), the Models tab shows "This server is an Abhoth
   Supervisor: connect?"
2. **Connect.** The user signs in (13.1). Old engine keys (`api-key.txt`, LiteLLM keys) are
   not reused; they are upstream keys, not user keys.
3. **Back up and rewrite.**
   - `config.json` is copied to `config.json.bak-<date>`.
   - The `supervisor` block is added.
   - Each replaced block gets `"supersededBy": "supervisor"` and is otherwise kept. Turning
     the supervisor off restores the old behaviour in one step.
4. **Keep ids.** The importer kept LiteLLM route names (ROUTER-CONFIG.md 13.1), so per-project
   and per-session model ids stay valid. The importer report's rename map is applied for any
   renamed ones. Saved per-project agent modes are kept and still win.
5. **Web container.** Set `ABHOTH_SUPERVISOR_URL` and `ABHOTH_SUPERVISOR_TOKEN_FILE`
   (deploy/compose). The server-wide LLM settings in the web container's config get the same
   `supersededBy` treatment. Users' own provider settings are left alone.
6. **Nothing is deleted** for 30 days. After that, `abhoth config migrate --cleanup` removes
   superseded blocks on request.

## 14. Build plan

The estimates are agent-days for code plus tests. **"Now"** means the phase can start in
parallel with rc-core (A), rc-switch (B) and rc-ui (C) without touching their files: new
crates and new pages only, tested against fakes and recorded fixtures. **"Needs"** names the
ROUTER-CONFIG.md phase it waits for.

| Phase | Content | Days | Start |
|---|---|---|---|
| SV1 | `orca-sensors`: sources, readings, expression parser and evaluator, EMA/gate/fallbacks, property tests | 1.5 | **Now** |
| SV2 | `orca-fans`: policy, ESPHome backend (REST push, SSE read, digest auth), fake backend, pusher-parity table tests against recorded `nvidia-smi` traces | 3 | **Now** |
| SV3 | `orca-gpuctl`: NVML adapter, pin/cap/app clocks/limits, assert, idle state machine, wake sources incl. AF_PACKET sniff | 3 | **Now** |
| SV4 | `orca-hostd`: daemon, socket protocol, limits file, last-known-good, watchdog, `boot`/`failsafe`, host facts (port of hoststat/bmcstat), journal OOM follower; **shadow mode** for fans | 4 | **Now** (ships standalone; the fan shadow can run on the host before anything else) |
| SV5 | `orca-calendar`: entries, RRULE/cron/iCal, occurrences, lead/tail, detection merge, effects, overlap/priority, event-key scope evaluation (pure) | 3 | **Now** |
| SV6 | `orca-alerts`: rules, dedupe/grouping, sinks with fakes | 2 | **Now** |
| SV7 | `orca-homeassistant`: discovery model, legacy id map, LWT, commands, HA REST client; tests against an embedded broker | 2.5 | **Now** |
| SV8 | Glance page UI against a recorded `/api/state`-shaped fixture (components, sparklines, parity checklist) | 4 | **Now** (new files under `client/src/pages/supervisor/`; route registration later) |
| SV9 | `orca-supervisor` umbrella + `abhoth supervisor` CLI, config tables, telemetry hub, snapshot/SSE feed, public listener + redaction, hostd channel in `hostctl serve` | 3 | **Needs** C1 (config loader), C3 (SSH/hostctl), C8 (SSE) |
| SV10 | Clocks/power wiring: `assert` on engine Ready, holds during switches, router wake on admission | 1.5 | **Needs** C4, C5 |
| SV11 | Media engines: `api` kinds, `lifecycle` standing/on_request, `depends_on`, conflicts, route `kind`s | 2.5 | **Needs** C4, C6 |
| SV12 | Calendar integration: guard kind `calendar`, placement/leases/slots reservations, `on_reserve` wait/drain/preempt, on-demand and switch checks, `calendar check` CLI, cron import via hostctl | 3 | **Needs** C5, C6, C7 |
| SV13 | Event keys: `abe_` kind in D140 keys (web server side), introspection fields, window checks, scopes, audit, key panel, `secret-put` to host | 2.5 | **Needs** C8 (+ a small change in the D140 key table: coordinate with rc-ui) |
| SV14 | Admin pages: Fans, Power, Calendar (week grid, edit, preview, key panel), Alerts, Home Assistant | 4 | **Needs** C9 shell |
| SV15 | Alerts wiring to control-plane events; MQTT commands → orchestrator | 1 | **Needs** C5, C7 |
| SV16 | Cutovers and retirements: fan pusher (shadow → act), clockctl/gpu-power/boot.sh, ha-bridge (legacy ids), dash (parity week), pipeline keys (9.7); runbooks and rollbacks | 3 | **Needs** SV4, SV9-SV15 |
| SV17 | Catalog API: `/supervisor/v1/catalog`, `/v1/models` `abhoth` objects, catalog stream, `[models.X.client]` agent hints, act-as-user for the web server | 2.5 | **Needs** C1, C6, C8 (the schema can be written now) |
| SV18 | App client: `supervisor` config, Models-tab card, sign-in/connect flow, catalog cache, pickers with warm/cold/loading states and cold UX, source grouping | 3 | **Needs** SV17 and the D140 connect endpoint (touches `orca-core` config and `client/src/features/settings`: coordinate with rc-ui) |
| SV19 | Profiles feature repointed to plans/switch/jobs; slot client → lease API; images/music via catalog; status strip | 2.5 | **Needs** SV17, C5, C7 |
| SV20 | Migration (`config migrate --to-supervisor`, detection, `supersededBy`, web container env), tests on copies of real configs | 1.5 | **Needs** SV18 |
| **Total** | | **~54** | of which **~23 can start now** |

**Order and parallelism:**

- SV1 → SV2 → SV4 is the fan path. SV4's shadow mode can run on the host weeks before the
  router cutover and retire the pusher on its own. That is a good first deliverable, because
  it removes a Python service and proves hostd.
- SV3 joins SV4 next.
- SV5-SV8 are independent.
- With two extra agents the "now" work takes about 2 weeks of calendar time. The "needs"
  half follows the builders' C5-C9.

**Conflict notes for the three builders:**

| Builder | Touch point | How it is handled |
|---|---|---|
| rc-core | `hostctl serve` (hostd relay channel) | added in SV9, after C3 merges |
| rc-core | `[cards.X].clocks` | becomes `[cards.X.power]`; the loader accepts the old key as an alias |
| rc-core | the allowlist path | `/etc/abhoth` → `$APPS/abhoth/etc` on TrueNAS (2.3) |
| rc-switch | guards | gain the `calendar` kind |
| rc-switch | placement and leases | gain a reservation filter (SV12, after C6/C7) |
| rc-switch | switch step 10 | calls `assert` |
| rc-ui | the D140 key table | gains `kind`/`event_id` (SV13) |
| rc-ui | the SSE event bus | reused for the feed |
| rc-ui | admin page shell | the supervisor pages slot into it (SV14) |
| all | root `Cargo.toml` | each new crate adds one `members` line: a trivial merge |

## 15. Open questions (resolved)

All resolved on 2026-11-26. Q4 and Q9 were decided by the user against the recommendation;
the rest take the recommendation.

### Resolved

| # | Question | Resolution |
|---|---|---|
| 1 | Host daemon on the host (systemd from the apps pool) vs a privileged container? | **Host** (2.3): hardware loops must outlive container redeploys; a privileged container is root anyway (as recommended) |
| 2 | ESPHome transport: keep `web_server` REST + digest, or move to the encrypted native API? | **REST for parity** now (proven, SSE for reads); native API later (removes the web password, needs a Rust noise-protocol client) (as recommended) |
| 3 | Should config own the ESP's curve numbers (Curve Low/High, Min Duty, FF, Critical) and write them? | **Yes, after a read-only diff phase**; the device keeps them with `restore_value`, so it stays safe without the host (as recommended) |
| 4 | Card A while serving: `pin` 1597 (`-lgc 1597,1597`) or `unlocked` (boost to 1597 when cool)? | **`unlocked`** (user, 2026-11-26): A boosts up to 1597 MHz when cool and is never pinned; `allow_cap = false`; `assert` checks it reaches 1597 when busy and cool (D-SV6) |
| 5 | Public Glance: what to hide? | Drop recent requests, OOM process names, container names and error text; keep the rest; off until enabled (as recommended) |
| 6 | Keep the HA "GPU Max Clock" control at all? | Replace with per-card cap with TTL, not offered on A (as recommended) |
| 7 | MQTT ids: reuse `deepthought_*` unique_ids for the replaced entities? | **Yes** (history and automations survive); new entities use `abhoth_<server>_*` (as recommended) |
| 8 | Main push channel for alerts? | **HA mobile notify**; ntfy optional (as recommended) |
| 9 | SA3: standing or on-request (frees VRAM when idle)? | **Standing, always running** (user, 2026-11-26); no idle stop. Its layout conflict on its card applies only while it is generating |
| 10 | Bench protection: calendar entry with key, or keep the guard lease? | **Calendar entry with key** created by the bench queue (one mechanism, visible in the calendar); keep the lease for slot pinning (as recommended) |
| 11 | Event-key default for `on_reserve`? | **`wait`**; `preempt` only for `high`+ with a warning (as recommended) |
| 12 | Calendar storage: config include or database? | **`calendar.toml` include** (diffable, backups); occurrences and key use in `router.db` (as recommended) |
| 13 | iCal: subscribe to an external calendar? | Support it, **off by default**; import-once covers the known need (as recommended) |
| 14 | BMC fan control | **Read-only** (as today); `ipmi` write only if the limits file allows it (as recommended) |
| 15 | Prometheus nvidia_gpu_exporter after hostd exports GPU metrics | Keep 30 days, then remove (Grafana panels move to `abhoth_gpu_*`) (as recommended) |
| 16 | Windows backend | Design only; build when a Windows GPU box needs it (as recommended) |
| 17 | Client key setup: paste a key, or the browser "connect" flow that mints a per-device key? | **Both**; connect is the default (no copy/paste, revocable per device) (as recommended) |
| 18 | Web container: one service token with act-as-user, or a key per web user? | **Service token with act-as-user** (no per-user setup; both audited); the token can only act as existing, enabled users (as recommended) |
| 19 | Cold `on_demand` from the app: ask the user every time, or remember per model? | **Ask once per model per day**, with "always for this model" in settings (as recommended) |
| 20 | Should `/v1/models` carry the `abhoth` extension, or only the catalog endpoint? | **Both**: the extension costs nothing and helps generic clients; the catalog is the full document (as recommended) |

## 16. Decisions in this document

| # | Decision | Why |
|---|---|---|
| D-SV1 | `abhoth supervisor`, one binary; `abhoth router` alias; `supervisor.toml` with `router.toml` fallback | one product, no break for the builders |
| D-SV2 | Unprivileged supervisor container + root `abhoth hostd` on the host, reached only through the forced-command SSH channel, bounded by a root-owned limits file | least privilege; hardware loops survive web-process restarts |
| D-SV3 | Fans prefer device-side curves with host-pushed inputs; host curves only where the hardware has no curve | the device's fail-safe works when everything else is dead |
| D-SV4 | Missing sensor values are never 0; strict and `_avail` functions are explicit | silent zeros are how fans stop |
| D-SV5 | Safety floors and caps live below policy (limits file, device), not in profiles, keys or calendar | nothing configurable can make cooling unsafe |
| D-SV6 | Card A runs **unlocked** while serving and boosts up to 1597 MHz when cool (user decision 2026-11-26, replacing the pin); `allow_cap = false` keeps caps off it; clocks re-asserted on every engine `Ready` and every 5 min, and `assert` checks it reaches 1597 when busy and cool | the drift nothing catches today; a pin would hold 1597 even when hot |
| D-SV7 | Router admission is the primary wake signal for idle clock-down; sniffing stays as a fallback | the router sees requests first and costs nothing |
| D-SV8 | One calendar replaces the special-case guards; cron keeps starting jobs | describe and protect, don't become a scheduler |
| D-SV9 | Event keys are D140 keys (`abe_`) valid only in their occurrences, least-privilege scopes, owned and audited | a pipeline gets its card without opening anything outside its window |
| D-SV10 | Event keys never bypass the probe rule, excluded cards, budgets or thermal safety | the structural guarantees of ROUTER-CONFIG.md stay structural |
| D-SV11 | Glance feed is snapshot + SSE patches; public view on its own listener with redaction | one-way data, proxy-friendly, no controls in public |
| D-SV12 | HA through MQTT discovery with legacy ids and an MQTT service principal | continuity for HA history; HA actions audited and guarded |
| D-SV13 | The app points at a supervisor with one URL and key; the catalog (plus its SSE stream) replaces per-model config, the profiles API client and the slot client | one source of truth; the app follows layouts instead of being reconfigured for them |
| D-SV14 | The app never probes models; warm/cold comes from the catalog, and cold behaviour follows the route policy | no accidental switches; honest ETAs |
| D-SV15 | Migration marks old settings `supersededBy` and keeps them 30 days | rollback is one toggle, nothing lost |

---

## Appendix A: the ESP32 fan controller

The firmware is in [`docs/hardware/esp32-gpu-fans/gpu-fans.yaml`](docs/hardware/esp32-gpu-fans/gpu-fans.yaml)
(secrets are `!secret`; fill [`secrets.yaml.example`](docs/hardware/esp32-gpu-fans/secrets.yaml.example)).
Build with `esphome run gpu-fans.yaml`.

### A.1 Pinout

The board is an ESP32 DevKit, 30 pins (`esp32dev`, ESP32-WROOM-32). All the fan pins are on
the left (VIN/GND) side, one pair per fan.

| Fan | PWM (header pin 4, blue) | TACH (header pin 3, yellow) |
|---|---|---|
| 0 (card B, V100-PCIe) | GPIO32 (D32) | GPIO33 (D33) |
| 1 (card A, V100-SXM3) | GPIO25 (D25) | GPIO26 (D26) |
| 2 (card C, V100-SXM2) | GPIO27 (D27) | GPIO13 (D13) |

- **PWM:** LEDC at 25 kHz (the Intel 4-pin spec).
- **Tach:** `pulse_counter` with the internal pull-up, 2 pulses per revolution, 13 µs filter.
- **Pins avoided:**
  - D34, D35, VP and VN: input-only, with no pull-ups;
  - D12: a boot strap pin;
  - D14: glitches at boot.

### A.2 Wiring

The screw-terminal shield is printed for an ESP32-S3, so its labels are wrong for this board.
The 30-pin DevKit sits in the bottom 15 of the 22 positions. Count terminals from the top
(1-22) and ignore the printed labels.

```
                                  ┌── 12 V supply ──┐
                                  │  +12V      GND  │
                                  └───┬─────────┬───┘
                     ┌────────────────┼─────────┤   (wired to every fan
                     │      ┌─────────┼─────────┤    header's pin 1 and pin 2)
                     │      │         │         │
   ESP32 shield,     │   FAN 0      FAN 1      FAN 2       each header:
   LEFT block        │  1 2 3 4    1 2 3 4    1 2 3 4      1 GND   2 +12V
   (count from top)  │      │ │        │ │        │ │       3 TACH  4 PWM
                     │      │ │        │ │        │ │
   13  D32 ●─────────┼──────┼─┘        │ │        │ │      PWM 0
   14  D33 ●─────────┼──────┘          │ │        │ │      TACH 0
   15  D25 ●─────────┼─────────────────┼─┘        │ │      PWM 1
   16  D26 ●─────────┼─────────────────┘          │ │      TACH 1
   17  D27 ●─────────┼────────────────────────────┼─┘      PWM 2
   18  D14 ○         │                            │        (leave empty)
   19  D12 ○         │                            │        (leave empty: boot strap)
   20  D13 ●─────────┼────────────────────────────┘        TACH 2
   21  GND ●─────────┘ ◄── common-ground link to 12 V GND (+ the 5 V supply's GND)
   22  VIN ●──── 5 V supply +
```

Terminals 1-12 have no pin (1-7) or are EN, VP, VN, D34 and D35 (8-12), and are left empty.

**Rules:**

1. **Common ground.** The 12 V GND must be connected to the ESP32 GND. Without it the fans
   ignore PWM and run at 100 %. That is safe, but they are not controlled.
2. **12 V never touches an ESP terminal.**
3. Swapping TACH and PWM does no harm, but the fan runs at 100 % and shows 0 RPM.
4. Keep the signal wires under about 50 cm. Twist each one with a ground wire if the RPM
   readings are noisy.

**Why it fails safe with no extra parts:**

- A 4-pin fan's PWM input has an internal pull-up, so a floating PWM wire means 100 %. The
  pins float while the ESP is unpowered, resetting or booting.
- The firmware boots at 100 %.
- The firmware only slows a fan while fresh temperatures (younger than 30 s) arrive.
- The critical temperature forces 100 %, even in manual mode.

### A.3 BOM (inferred from the wiring notes and firmware)

| Qty | Part | Notes |
|---|---|---|
| 1 | ESP32 DevKit V1, 30-pin (ESP32-WROOM-32) | `board: esp32dev` |
| 1 | 22-way screw-terminal breakout shield | printed for the ESP32-S3; see the offset note above |
| 1 | 5 V supply for the ESP32 (into VIN) | separate adapter or from the PSU (Molex/SATA) |
| 1 | 12 V supply for the fans | PSU rail or adapter, sized for 3 fans at full speed |
| 3 | 4-pin PWM fans (duct fans for the passive V100s) | model not recorded; any Intel-spec 4-pin PWM fan |
| 3 | 4-pin fan header pigtails | GND, +12 V, TACH, PWM |
| — | hook-up wire, common-ground link | — |
| 0-3 | *(optional)* 10 kΩ pull-ups TACH → 3.3 V | only if the RPM readings are noisy (the internal pull-up is weak) |

No transistors or level shifters are needed. The PWM input is open-collector-tolerant at
3.3 V, and the tach is an open collector pulled up to 3.3 V.

### A.4 Firmware entities used by the supervisor

| Entity | Use |
|---|---|
| `GPU N Temp` | pushed control temperature (marks the fan fresh) |
| `GPU N Power` | pushed load (%) |
| `Curve Low Temp`, `Curve High Temp`, `Min Duty`, `FF Low Load`, `FF High Load`, `FF Max Duty`, `Critical Temp` | the device curve. Read, and written when `sync_device_settings` is on. |
| `Manual Override`, `Manual Duty` | left to HA (it holds only while HA is connected) |
| `Fan N RPM`, `Fan N Duty`, `Fan N FF Duty`, `Failsafe Active`, `Fan N Stalled`, `WiFi Signal` | telemetry, alerts, Glance |

Proposed firmware v2. None of it is required for parity.

- A `Last Source` text sensor that echoes which host pushed, for when there are several hosts.
- A `Push Age` sensor.
- `stale_seconds` exposed as a config number, bounded to 10-60 s.
