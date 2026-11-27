# Sothoth (`abhoth sothoth`) as the inference control plane (design)

**Names (docs/NAMES.md):** Sothoth is the router. `abhoth router ...` still works, and
`router.toml` and `sothoth.toml` stand in for each other (a missing one reads the other).
This document says `router.toml` throughout.

Status (2026-11-25): **design only, nothing built.** It extends ROUTER.md: the router built in
phases 1-3 there is the data plane (`/v1/chat/completions`, placement, leases, on-demand
dry-run, the decision shadow). This document makes the same binary the **control plane** too:
it owns the GPU cards, starts and stops the engines, switches layouts and holds the whole
configuration. One build replaces four things in one go:

- LiteLLM (`:8010`) and its `effort_hook.py`;
- the qfn-lb nginx (`:8092`);
- pig-slots (`:8099`);
- the Python layout tooling: `layoutctl.py` (with its cron runner, watchdog hooks and boot
  reconcile), `profiles.json`, `profilecat.py`, the profiles API (`:8098`), `ondemand.py` and
  `gateway.json`.

The user's decisions (2026-11-25) are fixed inputs and are listed in section 0. The survey was
read-only, done on 2026-11-25 against the live layout `merged-inkling-x2`. Host-specific values
are placeholders: `gpu-host` (the GPU server), `$AI_ROOT` (its AI tree), `$ARCHIVE` (the Optane
model archive), `GPU-aaaa…` / `GPU-bbbb…` (card UUIDs). Keys are left out on purpose.

## Contents

0. [User decisions](#0-user-decisions)
1. [Goals and non-goals](#1-goals-and-non-goals)
2. [Survey: what the Python tooling really does](#2-survey-what-the-python-tooling-really-does)
3. [Feature parity](#3-feature-parity)
4. [The configuration](#4-the-configuration)
5. [Engine lifecycle and layout switches](#5-engine-lifecycle-and-layout-switches)
6. [Execution: local and SSH](#6-execution-local-and-ssh)
7. [Guards and the cold-model policy](#7-guards-and-the-cold-model-policy)
8. [Leases and slots](#8-leases-and-slots)
9. [Upstreams](#9-upstreams)
10. [Translation adapters](#10-translation-adapters)
11. [Access and the admin web UI](#11-access-and-the-admin-web-ui)
12. [Observability](#12-observability)
13. [Migration](#13-migration)
14. [Test strategy](#14-test-strategy)
15. [Build plan](#15-build-plan)
16. [Open questions](#16-open-questions)
17. [Decisions in this document](#17-decisions-in-this-document)

---

## 0. User decisions

| # | Decision |
|---|---|
| U1 | **Config model:** servers → cards (GPUs pinned by UUID, discovered with `nvidia-smi`) → engines (reusable definitions: API kind, start/stop/health, ports, lanes, context, models) → layouts (which engine on which card) → routes (public names → upstreams, with fallbacks). Lifecycle through **bash command templates** with variables (`{card}`, `{port}`, `{model}`, `{gpu_uuid}`, …); built-in drivers for docker compose, systemd and a plain process may be added. |
| U2 | **A TOML/YAML file is the source of truth**, hot-reloaded. The admin pages edit it with validation, write it back (comments kept where feasible), make a backup and an audit entry. |
| U3 | **Replace the Python tooling in one go:** build it all, then cut over; no long side-by-side period. Required: an importer from today's files, a dry-run/plan mode, and a tested cutover and rollback. |
| U4 | **Remote servers over SSH:** key-based, a user per server, commands from templates, timeouts, captured output. Local servers run commands directly. Telemetry: `nvidia-smi` polling over SSH. No stored passwords. |
| U5 | **Cold models: a policy per route:** on-demand switch (guarded: never during a bench, a video pipeline or a lease; rate limited; never on excluded cards/models), fall back to another model, or reject. **Default is fallback/reject.** A health check must never be able to trigger a switch. |
| U6 | **Upstream kinds:** local engines; **cloud APIs** (Anthropic, OpenAI, OpenRouter, …) with per-user and per-key budgets and limits; **other routers** (another Abhoth router, LiteLLM). Bridged machines are not in scope, but leave the hook (BRIDGE.md 10.3). |
| U7 | **Take over pig-slots:** leases, the Lead reserve and agent lanes live in the router; its HTTP API stays compatible (or a shim does) for Abhoth's slot client and the bench. |
| U8 | **Translation adapters, all four:** Responses API in; OpenAI chat ⇄ Anthropic Messages both ways; tool-call normalisation; reasoning/params mapping per engine. Composable per route or per engine, with a golden-test corpus. |
| U9 | **Access:** only admins see and edit the router config and switch layouts; users only see the model list. Use the D140 accounts (SQLite, roles, per-user `abk_…` keys). |

This reverses ROUTER.md section 3 WON'T "running layout switches itself (docker, GPUs)" and
"cloud provider adapters, budgets, an admin UI". ROUTER.md's data-plane design stays as it is.

## 1. Goals and non-goals

### Goals

1. **One process, one config file, one admin UI** for everything between a client and a GPU:
   names, routing, leases, placement, engine lifecycle, layouts, on-demand, cloud fallbacks.
2. **Parity first.** Every behaviour of the four replaced systems that a client, a script or
   the dash relies on is kept or deliberately dropped (section 3). Wire shapes are unchanged:
   `:8010`, `:8092`, `:8098` and `:8099` keep answering as they do today.
3. **Safe switching by construction.** Switches are planned before they run, are refused by
   the same guards as today, roll back on failure, and can never be started by a probe.
4. **Nothing DeepThought-specific in code.** Card UUIDs, container names, ports, ninfer
   defaults, the Strata arena coupling and the quiet window all become config. A second GPU
   host is a new `[servers.x]` block, not a code change.
5. **Least privilege.** The router container needs no docker socket and no root; it reaches
   hosts through a forced-command SSH key whose remote side only runs an allowlist.
6. **Restart-safe.** A router restart never restarts an engine: it adopts what runs.

### Non-goals

- Replacing `gpu-power` (idle clock-down) or `clockctl`. The router reads clocks and holds
  `gpu-power` during switches; those services keep owning clock changes (section 6.6).
- Replacing the qfn-llm watchdog's generic "restart CRASHED GPU apps" job. Only its
  layout and gateway hooks go.
- A general multi-tenant LLM gateway (teams, invoices, SSO). Budgets are per user and per key,
  in dollars and rates, enough for a household and a few accounts.
- Bridged machines as upstreams (BRIDGE.md 10.3): hook only (section 9.5).
- Embeddings, images, audio endpoints. Passthrough can be added per upstream later.
- Router-side prompt caching or tokenisation (ROUTER.md WON'T stays).
- Kubernetes, etcd or any cluster manager.

## 2. Survey: what the Python tooling really does

Read-only, 2026-11-25. `layoutctl.py` is 2061 lines, `profiles_api.py` 835, `ondemand.py` 287,
`profilecat.py` 335, `profiles.json` 903 (schema v3), `gateway.json` 99. Most of the logic is
the result of incidents; each item below cites the one that caused it where the code says so.

### 2.1 Layout switching (`layoutctl.py switch`, run by the root cron `runner`)

The API (`:8098`) only validates and writes a spool file; a root cron job (`runner --seconds
58`, every minute) carries switches out one at a time. A switch:

1. **Preconditions** (refused, never queued): the quiet window 06:45-07:30 UK (AI News at
   07:00); music mode (`/run/qfn-musicmode.state`); the old `DENSE_LEAD` flag; models dir
   pointing into the archive; for each card: the ninfer build has `ninfer-serve`, the
   llama.cpp image exists locally, the GGUF exists, options render; every ninfer model file is
   present or restorable from the archive manifest. On-demand switches re-read the guards
   right before starting (the API decided on guards up to 5 s old).
2. **Offline overlay:** cards in `state/offline.json` count as `engine: off` in every layout; a
   layout that would leave no card serving is refused.
3. **Model restore first:** model files moved to the Optane archive are copied back, verified
   against the manifest's sha256 and logged (`restores.jsonl`) *before* anything stops. The
   copy rate learned from past restores feeds the switch estimate.
4. **Restart set** (`restart_set`): the cards whose spec changed, plus two couplings:
   - Strata B maps the expert arena Strata A fills, so when A (re)starts on Strata while B
     stays on Strata, **B restarts after it too**;
   - a ninfer server that stays while another card (re)starts Strata **restarts as well**,
     after Strata is up: its ~18 GiB of pinned host memory sits on NUMA node 0, where the arena
     fill needs room (2026-11-18: Strata A OOM-killed 15 times).
5. **Leases, race-free:** if every card moves, pig-slots is *frozen* first (claims answer
   `switching`); otherwise only the moving cards' slots are withdrawn and the other card keeps
   serving and leasing. After 1.5 s, held leases on moving cards (or orphans) refuse the
   switch. A lease on card A no longer blocks a B-only switch (2026-11-24).
6. **Locks:** the layout lock, then the qfn-llm watchdog's lock and its start lock, so the
   watchdog cannot `compose up` a container in the middle.
7. **Foreign holders:** any running container whose `NVIDIA_VISIBLE_DEVICES` names the card
   (by UUID; `utility`-only containers such as clockctl, gpu-power and ha-bridge excluded) and
   that is not a layout container refuses the switch.
8. **Apply:** stop the changed cards (Strata: `docker stop -t 30`; ninfer/llama.cpp: a
   `docker rm -f` loop that survives a zombie exit while tens of GiB of pinned memory unpin);
   **wait until each card uses under 1 GiB** (90 s); set Strata's `parallel` in its config
   file; **Strata first, one card at a time, each waited healthy**, after a NUMA check that
   drops the page cache if node 0 has under 64 GiB free; then ninfer/llama.cpp together.
   Containers run with `--cpuset-cpus` per card, `--cpuset-mems` per profile, the card UUID,
   labels `layouts.managed`, `layouts.card`, `layouts.spec` (the card spec as JSON).
9. **Health:** `/health` with an engine-specific body check (`"loaded": true`, `ok`), 3 s
   polls up to 900 s, failing fast with the container's log tail (keys masked) when it exits.
10. **Verify:** a one-token chat ("Reply with the single word OK.", 16 tokens) **only on the
    cards that restarted**; the others only `/health`. A busy card's chat queued behind long
    generations and timed out, which rolled back a switch that only changed card B
    (2026-11-24).
11. **Rollback:** any failure switches back to the previous named layout with the same
    machinery; a failed rollback ends `failed` with whatever `detect()` finds.
12. **Record:** `state/live.json` (layout, cards, since, slots, offline, last switch),
    `want-services` (read by the watchdog after a reboot), pig-slots `layout.json` (unfrozen),
    the job file with timed steps, `history.jsonl`, and **new OOM kills from `dmesg`**
    during the switch.
13. **Gateway step:** regenerate qfn-lb's `nginx.conf` and LiteLLM's `config.yaml`; LiteLLM is
    hot-reloaded (confirmed by `gateway_rev` in `/v1/model/info` within 30 s, else
    redeployed), unless `gateway --hold` keeps it (pending config kept aside).

Typical times: a switch of both cards ~50 s (history: 46-56 s), estimate 115 s from per-engine
constants (`server.estimates`).

### 2.2 Keeping the machine in the layout

- **`ensure`** (watchdog, every 2 min): start a missing ninfer/llama.cpp container of the live
  layout, **3 tries per boot** (counters in tmpfs), one card's failure not blocking the other.
- **`reconcile [--boot]`** (POSTINIT init script, as a transient systemd unit): waits for
  docker, the `ix-apps` mount, both V100s visible by UUID and the TrueNAS middleware (LiteLLM
  and qfn-llm not mid-deploy), up to 1500 s; starts qfn-lb and pig-slots if missing; if the
  live layout's definition changed in `profiles.json`, uses the new definition; starts missing
  cards (no lease check: nothing can hold a lease on a server that is down) or switches
  (ignoring the quiet window); then the gateway step and a **gateway test** (one short chat
  through LiteLLM with the first alias; an informational one through the router on `:8020`).
- **`detect()`** knows what runs from container labels, falling back to parsing `docker
  inspect` args and the `/app` mount for unlabelled containers (shown as a custom state,
  never matched to a layout by accident).
- **Offline/online cards** persist across reboots; `online` reuses the switch's apply rules and
  puts the card back offline on failure.

### 2.3 On-demand (`ondemand.py` + the profiles API relay)

`decide()` is pure and unit-tested (16 tests). Rules worth keeping exactly:

- generic aliases and pinned `-a`/`-b` names **never** switch; only exact catalog names do;
- blockers with their own retry-after: quiet window (until 07:30), a video pipeline (300 s), the
  bench queue (600 s), music mode, gateway hold, an offline card;
- **stale guards (older than 45 s) mean wait, never switch**; pig-slots not answering means
  wait;
- per card: minimum stay 300 s after any change; leases on that card (leases on the other card
  don't matter); load unknown means skip; busy, or busy within the 10 s idle grace, means skip;
- pick the card **idle longest, ties to B** (keeps the fast card A as it is);
- one pending switch per target is shared by every waiting request; another job means wait;
  failure hold 60 s; queue up to 300 s; switch timeout 900 s; a switch that restarts both cards
  (Strata) refused unless `allowBothCards`; `exclude` lists catalog models never offered
  (`dense-pi` today);
- streaming clients get an early `delta.reasoning_content` note plus SSE keepalives.

The guards come from the host: **video pipeline** = a live `run_daily.sh` process (including
one waiting for the run lock), found by scanning `/proc/*/cmdline`; **bench** = the Abhoth
bench queue file says `running`/`current`; **music** = a flag file. The runner writes them to
`state/guards.json` every 5 s because the API container cannot see the host's processes.
Idleness comes from polling each card's engine `/slots` (`is_processing`) every 3 s.

On-demand is **off** since a LiteLLM `GET /health` started a real switch (ROUTER.md 1.1).

### 2.4 The catalog (`profiles.json` v3, `profilecat.py`)

- `engines` (ninfer, strata, llamacpp: ports per card, default model id, builds, client
  `extraBody`), `engineProfiles` (lanes, context, build, options, env, NUMA `mems`, a rough VRAM
  budget, notes), `models` (artifact per engine: file, served id, VRAM, options, the engine
  profiles it fits), `profiles` (per card `{engine, engineProfile, model}` | `off` | `same`, a
  **mode** auto/hands-on/orchestrator/peer, and **client tuning** keys such as
  `agent.reasoning.lead`, `slots.agentsPerCopy`), bridge profiles with no cards (Claude Code,
  Codex), `aliases` / `layoutAliases` for old names.
- A profile's layout id is its own name. User-saved and composed setups live in
  `spool/user/{saved,composed}.json` (the API can only write the spool).
- **VRAM check:** a pick is refused when model + engine profile exceed the card's VRAM minus
  1.5 GiB (`--force` overrides).
- Every profile and layout gets a generated plain-English `summary` and structured `facts`.
- Built-in defaults that live only in `layoutctl.py`: the card table (UUID, CPU set, label,
  container names and ports per engine), `NINFER_DEFAULTS` / `NINFER_FIXED`,
  `LLAMA_DEFAULTS` / `LLAMA_FIXED`, image names, model directories, context sizes, `HEALTH_OK`.

### 2.5 GPU clocks and power (not in layoutctl)

- Boot (`PREINIT`): persistence mode (`-pm 1`; without it the driver silently drops `-lgc`
  locks) and application clocks `877,1380` on the PCIe card B.
- **clockctl** (`:9877`): whitelisted `nvidia-smi` operations per UUID: power limit, lock,
  max-clock cap (`/maxclock`), unlock (`-rgc`, which re-applies the cap), status; an `exclude`
  list per call.
- **gpu-power**: idle clock-down per card (locked at 135 MHz after 60 s idle; ~16 W saved per
  card), woken in ~7 ms by sniffing HTTP POSTs to any GPU container's port, by NVML polling or
  by `GET /wake`; holds (`/run/gpu-power-hold*`, `GET /hold?secs=`) for benches; a dead-man
  cron resets clocks if its heartbeat stops.
- Card A (V100-SXM3) boosts to **1597 MHz** when unlocked; its application clock is 1380.
  Anything that leaves A capped or locked low while it serves costs throughput. Nothing
  re-checks this after a switch today.

### 2.6 Other findings

- **Keys in argv:** ninfer and llama.cpp are started with `--api-key <key>` on the command
  line, so the key is visible in `ps` and `docker inspect` (layoutctl masks it in its own
  logs only).
- Four GPUs on the host: the two layout V100s (32 GB), a V100-SXM2-16GB (ComfyUI, image API,
  quiet-mode exempt) and a Quadro M4000. Only the two are layout cards; the others must be
  discovered and shown but never touched.
- The dash ("Glance", `:8077`) probes LiteLLM, qfn-lb, pig-slots and the profiles API, and its
  offline/online buttons POST straight from the browser to the unauthenticated `:8098`.
- HA's ha-bridge and Jarvis' `deepthought.py` switch layouts through `:8098` as well.
- `gateway.json` still carries `lbUrl`, LiteLLM router settings, `genericExclude` (empty
  since 2026-11-22) and `modelEffortLevels` for `qwen3.8-27b-pi`.

## 3. Feature parity

Keep = same behaviour (maybe new mechanism); Replace = same goal, different mechanism; Drop =
not carried over (reason given).

### 3.1 LiteLLM (`:8010`)

| Capability | Verdict | Where |
|---|---|---|
| `POST /v1/chat/completions`, SSE passthrough | Keep | built (ROUTER.md) |
| 14 names: generic aliases, pinned `-a`/`-b` with fallback, catalog names, `weight` | Keep | routes (4.6) |
| `drop_params`, `additional_drop_params`, `litellmParams` defaults | Keep | engine `params` + adapter `params` (10.5) |
| `effort_hook.py` clamp + thinking-off + extra_body merge (D-R6) | Keep | adapter `reasoning` (10.5) |
| Hot reload of the model list (`gateway_rev`) | Replace | file watch + `rev` (4.9) |
| `simple-shuffle`, `num_retries: 2`, cooldown 10 s | Replace | placement + one failover before first byte (built) |
| `GET /v1/models`, `/v1/model/info`, `/health/liveliness` shapes | Keep | built; `/v1/models` now filtered per user (11.1) |
| `GET /health` that test-calls every deployment | **Drop** | the hazard of ROUTER.md 1.1; router `/health` calls nothing |
| `x-litellm-model-name` header | Keep | built |
| Unauthenticated access | Replace | user keys (11.1), with a LAN-anonymous policy for the transition |
| Cost DB, UI, virtual keys (unused) | Replace | budgets, admin UI and D140 keys (9.3, 11) |

### 3.2 qfn-lb (`:8092`)

| Capability | Verdict | Where |
|---|---|---|
| `/ninfer/`, `/a/`, `/b/`, `/` routes | Keep | compat listener (built, D-R16) |
| Engine key injected | Keep | built |
| `least_conn` by weight | Replace | placement on in-flight + lanes (built) |
| `proxy_next_upstream` 2 tries | Replace | one failover (built) |
| `/a/slots` passthrough (profiles API idle polling) | Keep | compat listener; also used internally (7.3) |
| Generated nginx.conf | Drop | no generator; the router reads its own config |

### 3.3 pig-slots (`:8099`)

| Capability | Verdict | Where |
|---|---|---|
| `/claim`, `/renew`, `/release`, `/list`, `/health`, same JSON, TTL 90 s | Keep | built (`slots.rs`) + `:8099` shim |
| Idempotent claim per key, `copy`/`avoid`/`slot` | Keep | built |
| Slot list from `layout.json`, freeze during switch | Replace | slot list from the active layout; freeze by the orchestrator (8.2) |
| `leases.json` persistence | Replace | SQLite (`router.db`), plus `leases.json` written for rollback (13.5) |
| Advisory only | Replace | leases feed admission and placement (built, flags) |

### 3.4 Layout tooling (`layoutctl.py`, profiles API, `ondemand.py`, catalog)

| Capability | Verdict | Where |
|---|---|---|
| Catalog v3: engines, engine profiles, models/artifacts, profiles, modes, client tuning, aliases | Keep | config 4.4-4.5 |
| User-saved/composed profiles | Keep | written into the config file (as `source = "user"`) by the admin UI |
| VRAM budget check (card − 1.5 GiB, `--force`) | Keep | validation 4.8 + plan |
| Summaries/facts for pickers | Keep | generated from config by the profiles shim |
| `status`, `plan`, `switch`, jobs with timed steps, `history.jsonl`, estimates | Keep | orchestrator 5.4; estimates learned per (engine, card) |
| Restore models from the archive with sha256 | Keep | engine `prepare` hook via hostctl builtin `restore-file` |
| Restart couplings (Strata arena, NUMA node 0) | Keep | coupling rules 5.5 |
| Strata-first sequential starts; page-cache drop before Strata | Keep | `start_order`, `before_start` hook |
| Freeze/withdraw slots then check leases | Keep | 5.4 steps 4-5 |
| Watchdog lock while starting | Keep | hostctl `flock` builtin around starts (6.3) |
| Foreign GPU holders refuse | Keep | telemetry compute-apps + driver labels (6.5) |
| Wait until the card has under 1 GiB used | Keep | `stopped` check per card |
| Zombie-tolerant container removal | Keep | docker driver |
| Health with body check, fail fast with log tail | Keep | engine `health` |
| Verify chat on restarted cards only | Keep | `verify` step (5.4) |
| Auto rollback to the previous layout | Keep | 5.4 |
| OOM-kill detection | Keep | hostctl builtin `oom-kills` |
| `ensure` (3 tries per boot) | Keep | reconcile loop (5.7) |
| Boot reconcile with readiness waits | Keep | server `ready` checks (5.7) |
| Offline/online cards, persisted | Keep | card `admin_state` in `router.db` (4.3) |
| `gateway` / `gateway --hold` | Drop | no separate gateway: routes follow the active layout atomically |
| `gateway_test` after reconcile | Keep | `post_check` of a server (5.7) |
| `live.json`, `want-services`, pig `layout.json` | Replace | `router.db`; legacy files still written for 30 days (13.5) |
| Spool + root cron runner | Replace | the router executes directly (no spool, no cron) |
| `/run/qfn-musicmode.state`, `DENSE_LEAD` | Keep | file guards (7.1) |
| Video/bench/music guards, quiet window, stale-guard rule | Keep | guards 7.1 |
| `decide()` on-demand rules | Keep | built (`ondemand.rs`), extended with the per-route policy (7.2) |
| On-demand relay on `:8098` | Drop | the router decides and forwards itself |
| Profiles API (`/profiles`, `/live`, `/plan`, `/switch`, `/jobs`, `/cards`, `/compose`, `/profiles/save|delete`, `/ondemand`) | Keep | `:8098` shim (8.4), switching now needs an admin or service key |
| GPU clocks after a switch | New | verify clock state and hold gpu-power during switches (6.6) |
| Keys in argv | **Fix** | key files only (6.4) |

## 4. The configuration

### 4.1 Format and files

- **TOML** (`router.toml`), edited by the UI through `toml_edit`, which keeps comments, key
  order and formatting of everything it does not touch. YAML was the alternative; TOML wins on
  round-trip editing in Rust and on not being whitespace-sensitive (D-RC1).
- `include = ["catalog/*.toml"]` lets the catalog (engines, models) live in its own files;
  every file is watched. The UI writes to the file that defines the item.
- Runtime state (active layout per server, offline cards, leases, jobs, budgets, audit) is
  **not** config: it lives in `router.db` (SQLite, WAL, rusqlite 0.43 as D140). Config says
  what can exist; the database says what was chosen and what happened.
- `router.json` (ROUTER.md 4.3) stays readable: the importer converts it, and a router started
  with a JSON file keeps today's data-plane-only behaviour (`control.enabled = false`).

### 4.2 Annotated example (generated from DeepThought, 2026-11-25)

This is what the importer (13.1) produces from `profiles.json`, `gateway.json`, LiteLLM's
`config.yaml`, qfn-lb's `nginx.conf` and the constants in `layoutctl.py`, shortened where marked.

```toml
# router.toml: Sothoth (router) config. Schema version below; `abhoth sothoth config check` validates.
version = 1

[router]
listen       = "0.0.0.0:8010"
state_dir    = "/state"                 # router.db, backups, request and job logs
# Compatibility listeners (D-R16); each speaks only its old surface.
compat       = { lb = "0.0.0.0:8092", slots = "0.0.0.0:8099", profiles = "0.0.0.0:8098" }
request_log  = "/state/requests.jsonl"
timezone     = "Europe/London"           # schedules, quiet windows, daily summaries

[router.limits]
timeout_seconds         = 3600
connect_timeout_seconds = 3
max_body_bytes          = 67108864
failovers               = 1               # before the first byte only (D-R10)
cooldown_seconds        = 10
strip_fields            = ["metadata", "user"]   # D-R5

[control]
enabled  = true          # false = data plane only (ROUTER.md behaviour; the build's feature flag)
mode     = "act"         # "observe" (decide and log, touch nothing) | "act"
legacy_state_mirror = { dir = "$AI_ROOT/layouts/state", pig_slots = "$AI_ROOT/pig-slots/data", until = "2027-01-01" }

# ── secrets: references only, never values (4.7) ───────────────────────────────────────────
[secrets]
engine_key      = { file = "/run/secrets/engine-key" }        # today's api-key.txt
ssh_gpu_host    = { file = "/run/secrets/ssh-gpu-host" }      # private key, 0600
anthropic       = { env = "ABHOTH_ANTHROPIC_KEY" }
openrouter      = { file = "/run/secrets/openrouter-key" }
accounts_token  = { file = "/run/secrets/abhoth-introspect" }  # router <-> web server (11.1)

# ── servers ────────────────────────────────────────────────────────────────────────────────
[servers.deepthought]
title   = "DeepThought"
host    = "gpu-host"
exec    = "ssh"                      # "local" | "ssh" (6.1); loopback SSH from the container
ssh     = { user = "abhoth-exec", port = 22, key = "secret:ssh_gpu_host",
            host_key = "SHA256:xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", connect_timeout = 5 }
hostctl = "strict"                   # the remote side runs only its installed allowlist (6.2)
# Variables any template on this server can use as {var:name}.
vars = { ai_root = "$AI_ROOT", qfn = "$AI_ROOT/qwen-flash-next", ninfer = "$AI_ROOT/ninfer",
         llama_models = "/mnt/apps-models", archive_manifest = "$ARCHIVE/MANIFEST.tsv" }
# A switch takes these host-wide locks (the qfn-llm watchdog uses the same files).
locks   = ["/run/qfn-watchdog.lock", "/run/qfn-start.lock"]
# Boot readiness: reconcile waits until all pass (layoutctl wait_ready).
ready   = [ { builtin = "docker-up" }, { builtin = "mounted", path = "/mnt/.ix-apps" },
            { builtin = "gpus-visible" },                       # every [cards] UUID of this server
            { command = "midclt-apps-settled", timeout = 60 } ] # named template, below
ready_timeout_seconds = 1500
post_check = { route = "qwen3.8-27b-coder", max_tokens = 64 }   # gateway_test, after reconcile and switches

[servers.deepthought.templates]          # server-level named templates
midclt-apps-settled = '''midclt call app.query '[]' '{"select":["name","state"]}' | grep -vq DEPLOYING'''

# ── cards: pinned by UUID, never by index (discovered with nvidia-smi; 6.5) ───────────────
[cards.A]
server   = "deepthought"
gpu_uuid = "GPU-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
label    = "V100-SXM3 32 GB (fast, 21:00.0)"
vram_gib = 32.0
reserve_gib = 1.5                        # VRAM check: model + profile <= vram - reserve
cpus     = "0-23,96-119"
clocks   = { expect = "unlocked", max_sm_mhz = 1597 }     # verified after starts (6.6)
on_demand = true                         # may be switched by an on-demand request

[cards.B]
server   = "deepthought"
gpu_uuid = "GPU-bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"
label    = "V100-PCIe 32 GB (01:00.0)"
vram_gib = 32.0
reserve_gib = 1.5
cpus     = "24-47,120-143"
clocks   = { expect = "unlocked", app_clocks = [877, 1380] }
on_demand = true

# The other two GPUs are discovered and shown, never managed (ComfyUI, image API).
[cards.C]
server   = "deepthought"
gpu_uuid = "GPU-cccccccc-cccc-cccc-cccc-cccccccccccc"
managed  = false
[cards.D]
server   = "deepthought"
gpu_uuid = "GPU-dddddddd-dddd-dddd-dddd-dddddddddddd"
managed  = false

# ── engines: reusable definitions (4.4) ────────────────────────────────────────────────────
[engines.ninfer]
title        = "ninfer"
api          = "openai-chat"             # what it speaks: openai-chat | openai-responses | anthropic-messages
also_speaks  = ["openai-responses"]      # passthrough when the client speaks it too (10.1)
driver       = "docker"                  # docker | compose | systemd | process | template (5.2)
image        = "local/ninfer-build:cuda12.8.1"
container    = "ninfer-{card_lc}"        # same names as layoutctl: rollback finds them (13.5)
port         = { A = 8094, B = 8096 }
key          = "secret:engine_key"       # mounted as a file, never put in argv (6.4)
served_id    = "qwen3.8-27b"             # default; a model artifact's id wins
max_context  = 131072
health       = { path = "/health", expect_body = "ok", every_ms = 3000, timeout_seconds = 900 }
verify       = { max_tokens = 16, prompt = "Reply with the single word OK.", timeout_seconds = 180 }
stop         = { timeout_seconds = 120, gpu_free_below_mib = 1024, gpu_free_timeout_seconds = 90 }
start_estimate_seconds = { A = 75, B = 75 }   # learned from history afterwards
idle_probe   = { path = "/slots", busy_when = "any(.is_processing)" }   # on-demand idleness
request_log  = "{var:ninfer}/logs/req-layout-{card_lc}.jsonl"           # observe.rs reads it
tool_parser  = "none"                    # ninfer returns native tool_calls (10.4)
labels       = { "layouts.managed" = "1", "layouts.card" = "{card}", "layouts.spec" = "{spec_json}" }

[engines.ninfer.run]                     # docker driver inputs
args_prefix = ["/app/ninfer-serve", "/models/{model_file}", "--host", "0.0.0.0", "--port", "{port}",
               "--api-key-file", "/run/secrets/engine-key", "--model-id", "{served_id}",
               "--request-log-jsonl", "/logs/req-layout-{card_lc}.jsonl"]
options_style = "gnu"                    # {k = v} -> --k v; true -> --k; false/null -> left out
fixed   = ["host", "port", "api-key", "api-key-file", "model-id", "request-log-jsonl", "device", "max-concurrency"]
mounts  = ["{build_dir}:/app:ro", "{var:ai_root}/ninfer/models/v3:/models:ro", "{var:ninfer}/logs:/logs"]
docker  = ["--runtime=nvidia", "--restart", "no", "--shm-size", "2g", "--user", "950:950",
           "--cpuset-cpus={cpus}", "-e", "NVIDIA_VISIBLE_DEVICES={gpu_uuid}",
           "-e", "NVIDIA_DRIVER_CAPABILITIES=compute,utility"]

[engines.ninfer.defaults]                # NINFER_DEFAULTS; "lanes" = the profile's lane count
max-context = 131072
kv-capacity = "auto"
max-concurrency = "{lanes}"
kv-dtype = "int8"
device-state-slots = "{lanes}"
host-state-slots = 8
host-kv-mib = 16384
prefill-chunk = 2048
spec = "mtp"
draft-tokens = 3
lm-head-draft = true
preserve-thinking = true
pending-timeout-ms = 600000
default-max-tokens = 32768

[engines.ninfer.builds]                  # {build_dir}
merged = "{var:ninfer}/builds/merged-20261123"
lanes4 = "{var:ninfer}/builds/lanes4-20261120"
gaps   = "{var:ninfer}/builds/gaps-20261121"
stock  = "{var:ninfer}/src-v3/build-v100/apps"

[engines.ninfer.params]                  # request rules for every route that lands here (10.5)
effort_levels      = ["none", "low", "medium", "xhigh"]
thinking_off       = "drop"              # ninfer 400s reasoning_effort + enable_thinking=false
drop               = ["response_format"] # json modes are refused (400)
strict_fields      = true                # unknown top-level fields 400: only pass the allowlist
allow_fields       = ["model", "messages", "stream", "stream_options", "max_tokens", "max_completion_tokens",
                      "temperature", "top_p", "top_k", "min_p", "stop", "seed", "tools", "tool_choice",
                      "parallel_tool_calls", "reasoning_effort", "chat_template_kwargs",
                      "prompt_cache_key", "prompt_cache_options", "uncensored"]   # verify list in shadow

# Engine profiles (lanes, options, VRAM): profiles.json engineProfiles.
[engines.ninfer.profiles.merged-inkling-4lane]
title    = "ninfer 'merged', Inkling 2 Mini, 4 lanes, images, 64k"
lanes    = 4
ctx      = 65536
vram_gib = 7.0
build    = "merged"
mems     = "1"                           # docker --cpuset-mems (NUMA node for pinned host memory)
options  = { vision = true, kv-dtype = false, spec = false, draft-tokens = false, lm-head-draft = false,
             host-state-slots = false, host-kv-mib = false, device-state-slots = false, max-context = 65536 }

[engines.ninfer.profiles.merged-4lane]
title    = "ninfer 'merged', 4 lanes, images"
lanes    = 4
ctx      = 131072
vram_gib = 7.5
build    = "merged"
mems     = "1"
options  = { vision = true, host-state-slots = false, host-kv-mib = false, host-context-mib = 31744 }
env      = { NINFER_OPENAI_SHARED_PREFIX = "structural" }
# … ninfer-2lane, ninfer-lanes4-bench (imported the same way)

[engines.strata]
title       = "Strata"
api         = "openai-chat"
driver      = "compose"                  # services of the TrueNAS app qfn-llm
compose     = { project = "ix-qfn-llm", file = "{latest_rendered_compose:qfn-llm}",
                service = { A = "strata-a", B = "strata-b" }, container = { A = "qfn-strata", B = "qfn-strata-b" } }
port        = { A = 8080, B = 8091 }
served_id   = "qwen3.8-flash-next"
max_context = 262144
health      = { path = "/health", expect_body = '"loaded": true', timeout_seconds = 900 }
stop        = { mode = "stop", timeout_seconds = 30 }   # docker stop -t 30 (the container is kept)
start_order = "sequential"               # one card at a time, each healthy before the next
share       = "expert-arena"             # coupling group (5.5)
before_start = [ { builtin = "numa-room", node = 0, need_gib = 64, drop_caches = true },
                 { builtin = "json-set", file = "{var:qfn}/sharedarena/strata-sa-{card_lc}.json",
                   key = "parallel", value = "{lanes}" } ]
[engines.strata.params]
default_effort = "low"                   # gateway.json litellmParams on Strata
[engines.strata.profiles.1slot]
lanes = 1
ctx = 262144
vram_gib = 4
# … 2slot, 3slot

[engines.llamacpp]
title       = "llama.cpp"
api         = "openai-chat"
driver      = "docker"
image       = "{build_image}"
container   = "llama-{card_lc}"
port        = { A = 8081, B = 8097 }
served_id   = "qwen3.8-27b-pi"
max_context = 131072
health      = { path = "/health", expect_body = '"ok"' }
tool_parser = "auto"                     # GGUF templates sometimes emit XML tool calls (10.4)
[engines.llamacpp.builds]
b12620 = "local/llama-cpp-v100:b12620"   # default
b11936 = "local/llama-cpp-v100:b11936"
# … run/defaults/fixed as for ninfer (LLAMA_DEFAULTS, LLAMA_FIXED; "--api-key-file" instead of "--api-key")

# ── models: artifacts per engine (profiles.json models) ───────────────────────────────────
[models.inkling2-mini]
title    = "Inkling 2 Mini"
vision   = true
[models.inkling2-mini.artifacts.ninfer]
file      = "inkling2-mini-q8.ninfer"
served_id = "inkling-2-mini"
vram_gib  = 23.0
profiles  = ["merged-inkling-4lane"]
restore   = { from_manifest = "{var:archive_manifest}" }    # copy back + sha256 before a switch
params    = { thinking_off = "none" }                       # gateway.json modelThinkingOffEffort
[models.inkling2-mini.artifacts.llamacpp]
file      = "$LLAMA_MODELS/inkling-2-mini/Inkling-2-Mini-UD-Q4_K_XL.gguf"
served_id = "inkling-2-mini"
vram_gib  = 17.0
options   = { mmproj = "mmproj-F16.gguf", spec-type = false, temp = 1.0, top-p = 1.0, top-k = 0, min-p = 0.0 }
profiles  = ["llamacpp-inkling-4x32k", "llamacpp-inkling-2x128k"]

[models.dense-pi-abl-v3fork]
title = "Dense Qwen3.8-27B, pi-adapted, abliterated (v3 file for 'merged')"
vision = true
artifacts.ninfer = { file = "qwen3_8_27b_piadapted_abl_v3.ninfer", vram_gib = 22.5 }
# … flash-next, dense-pi, dense-pi-abl, dense-stock, qwenpi

# ── layouts: what runs on which card (profiles.json profiles) ─────────────────────────────
[layouts.merged-inkling-x2]
title  = "Inkling 2 Mini on both cards, 'merged' build"
mode   = "auto"                          # OpenOrca agent mode; carried for the profiles shim
cards.A = { engine = "ninfer", profile = "merged-inkling-4lane", model = "inkling2-mini" }
cards.B = "same"
client = { "agent.reasoning.lead" = "medium", "agent.reasoning.worker" = "medium" }  # tuning keys
aliases = []

[layouts.merged-a-inkling-b]
title  = "Dense 27B (abliterated, 'merged') on A + Inkling 2 Mini on B"
mode   = "auto"
cards.A = { engine = "ninfer", profile = "merged-4lane", model = "dense-pi-abl-v3fork" }
cards.B = { engine = "ninfer", profile = "merged-inkling-4lane", model = "inkling2-mini" }

[layouts.flash-x2]
title  = "Flash on both cards (A 3 slots, B 2)"
mode   = "auto"
cards.A = { engine = "strata", profile = "3slot", model = "flash-next" }
cards.B = { engine = "strata", profile = "2slot", model = "flash-next" }
client = { "slots.agentsPerCopy" = 2, "slots.leadCopy" = "A" }
aliases = ["flash2"]
# … 12 more layouts; bridge profiles (claude, codex) become [client_profiles.*] with no cards

[servers.deepthought.state]
initial_layout = "merged-inkling-x2"     # only used when router.db has no active layout yet

# ── routes: public names (4.6) ─────────────────────────────────────────────────────────────
[route_defaults]
cold     = "reject"                      # U5: fallback or reject; on_demand only where set
access   = "users"                       # users | admins | keys:<list>
adapters = []

[routes."qwen3.8-27b"]                   # generic alias: whatever is live
targets  = [{ pool = "live" }]           # every serving card whose engine kind matches any
[routes."qwen3.8-27b-coder"]
same_as  = "qwen3.8-27b"
# … qwen3.8-flash-next, qwen3.6-27b-coder, qwen3.6-35b-a3b-awq, jarvis-brain

[routes."qwen3.8-flash-next-a"]          # pinned card, falls back to the other
targets  = [{ card = "A" }, { card = "B", fallback = true }]
[routes."qwen3.8-flash-next-b"]
targets  = [{ card = "B" }, { card = "A", fallback = true }]

[routes."inkling-2-mini"]                # catalog model: the cards running it
targets  = [{ model = "inkling2-mini" }]
cold     = "fallback"
fallback = ["qwen3.8-27b"]               # whatever is live, rather than a switch

[routes."dense-stock"]
targets  = [{ model = "dense-stock" }]
cold     = "on_demand"
on_demand = { layout_from = "pick", pick = { engine = "ninfer", profile = "merged-4lane" },
              cards = ["A", "B"], access = "keys:switchers", fallback_while_waiting = false }

[routes."dense-pi"]
targets  = [{ model = "dense-pi" }]
cold     = "reject"                      # gateway.json onDemand.exclude
hidden   = false

[routes."claude-sonnet"]
targets  = [{ upstream = "anthropic", model = "claude-sonnet-5-5" }]
access   = "admins"
adapters = ["chat-to-anthropic"]

[routes."qwen3.8-27b-cloud-backup"]
targets  = [{ pool = "live" }, { upstream = "openrouter", model = "qwen/qwen3.8-27b", fallback = true }]

# ── upstreams other than local engines (9) ─────────────────────────────────────────────────
[upstreams.anthropic]
kind     = "anthropic"
base_url = "https://api.anthropic.com"
key      = "secret:anthropic"
prices   = { "claude-sonnet-5-5" = { input = 2.00, output = 10.00, cache_read = 0.10 } }   # $/MTok
limits   = { concurrency = 4, requests_per_minute = 30 }

[upstreams.openrouter]
kind     = "openai"
base_url = "https://openrouter.ai/api/v1"
key      = "secret:openrouter"
headers  = { "X-Title" = "abhoth" }

[upstreams.litellm-legacy]               # router chaining, kept for the rollback window
kind     = "router"
flavour  = "litellm"                     # never GET /health on it (9.4)
base_url = "http://gpu-host:8010"
enabled  = false

# ── budgets (9.3) ──────────────────────────────────────────────────────────────────────────
[[budgets]]
scope   = "user:*"                       # every user, each on their own
upstream = "*cloud"
period  = "month"
usd     = 10.0
[[budgets]]
scope   = "user:admin"
upstream = "anthropic"
period  = "month"
usd     = 100.0

# ── guards (7.1) ───────────────────────────────────────────────────────────────────────────
[guards.quiet-window]
kind    = "schedule"
windows = ["06:45-07:30"]                # router.timezone
blocks  = ["switch", "on_demand"]        # not reconcile (layoutctl ignores the window there)
[guards.video]
kind    = "process"
server  = "deepthought"
match   = { argv_any = "run_daily.sh", exe = ["bash", "sh", "dash", "run_daily.sh"], label_arg = "--channel", label_default = "ai-news" }
blocks  = ["on_demand"]
retry_seconds = 300
[guards.bench]
kind    = "json_file"
server  = "deepthought"
path    = "{var:ai_root}/orca/state/bench/queue.json"
active_when = ["running", "current"]
blocks  = ["on_demand"]
retry_seconds = 600
[guards.music]
kind    = "file_exists"
server  = "deepthought"
path    = "/run/qfn-musicmode.state"
blocks  = ["switch", "on_demand", "ensure"]
[guards.dense-lead]
kind    = "file_exists"
server  = "deepthought"
path    = "{var:qfn}/combos/DENSE_LEAD"
blocks  = ["switch"]

# ── slots and placement (8, ROUTER.md D-R11/D-R12) ────────────────────────────────────────
[slots]
ttl_seconds  = 90
lead_reserve = 1
naming       = "{card}{n}"               # A1..A4, B1..B4 from the active layout's lanes

[placement]
lease_aware = true
admission   = true
affinity    = true
affinity_ttl_seconds = 900
queue_seconds = 300
min_move_gain_seconds = 1.0

[on_demand]                              # server-wide brakes (7.3)
queue_seconds = 300
min_stay_seconds = 300
idle_grace_seconds = 10
switch_timeout_seconds = 900
failure_hold_seconds = 60
max_switches_per_hour = 4
allow_both_cards = false
stream_note = "reasoning"
```

### 4.3 Entities

| Entity | Key | Holds | Notes |
|---|---|---|---|
| `servers.<id>` | short id | host, exec (local/ssh), SSH settings, vars, locks, readiness, post-check, templates, `model_dir` (D152: where downloads go), `browse_roots` (folders the folder browser may show) | one orchestrator lock per server |
| `cards.<id>` | letter or name | server, `gpu_uuid` (required), label, VRAM, reserve, CPU set, clock expectation, `managed`, `on_demand`, `engines` (D152: engines allowed per kind of model) | `admin_state` (online/offline/excluded, with who and why) is runtime, in `router.db` |
| `engines.<id>` | engine kind | API kind, driver and its inputs, ports per card, key ref, health, verify, stop, ordering, coupling group, hooks, defaults, builds, request params, `caps` (D152: inherits, add/remove, probe) | the lifecycle unit is (card, engine) |
| `engines.<id>.profiles.<id>` | profile | lanes, ctx, VRAM, build, options, env, NUMA mems | profiles.json `engineProfiles` |
| `models.<id>` | catalog key | title, vision, artifacts per engine (file, served id, VRAM, options, fitting profiles, restore source, params); D152: kind, primary, prefer, allow, deny, format, arch, quant, servers, context, size, licence, source | profiles.json `models` |
| `layouts.<id>` | layout id | per card `{engine, profile, model}` / `"same"` / `"off"`, mode, client tuning, aliases, title/description | profiles.json `profiles` |
| `client_profiles.<id>` | name | client-only setups (Claude Code, Codex bridges) | served by the profiles shim, never switch anything |
| `routes.<name>` | public model name | targets in order, cold policy, fallbacks, adapters, access, hidden, `same_as` | the only thing users see |
| `upstreams.<id>` | id | kind, base URL, API, key ref, headers, limits, prices, models | local engines are implicit upstreams `engine:<card>` |
| `budgets[]` | | scope, upstream pattern, period, USD, requests/tokens per minute | ledger in `router.db` |
| `guards.<id>` | id | kind, source, which actions it blocks, retry | evaluated continuously |
| `secrets.<id>` | id | `file` or `env` | values never in config |
| `hf`, `inventory` | | Hugging Face endpoint and token ref; the inventory schema version | D152, [`docs/design/ROUTER-INVENTORY.md`](docs/design/ROUTER-INVENTORY.md) |

### 4.4 Route targets

Targets are tried in order; the first that has a **ready** upstream wins, and placement picks
among that target's upstreams.

| Target | Means | Today's equivalent |
|---|---|---|
| `{ pool = "live" }` | every ready local engine on a managed card (optionally `engine = "ninfer"`) | generic aliases over qfn-lb `/ninfer/` |
| `{ card = "A" }` | the engine on card A, whatever it runs | `-a` names, qfn-lb `/a/` |
| `{ model = "m" }` | ready engines serving catalog model `m` | catalog names |
| `{ upstream = "u", model = "x" }` | a cloud or router upstream, with the model id it expects | none |
| `fallback = true` | only when every earlier target has no ready upstream | pinned names' fallback |

`model` in the forwarded body is rewritten to the chosen upstream's served id (built). Route
names may not collide with served ids of other models unless `same_as` says so.

### 4.5 Template variables

Every substitution is **shell-quoted as one word** (a list variable becomes several quoted
words); `{{` and `}}` are literal braces. Unknown variables fail validation, not at run time.

| Variable | Value |
|---|---|
| `{server}`, `{host}` | server id, host |
| `{card}`, `{card_lc}` | card id, lower case |
| `{gpu_uuid}`, `{gpu_index}` | from config / discovery (`gpu_index` only for display: index order is not stable) |
| `{cpus}`, `{mems}` | card CPU set, profile NUMA nodes |
| `{engine}`, `{profile}`, `{model}` | ids |
| `{model_file}`, `{served_id}`, `{lanes}`, `{ctx}`, `{port}` | resolved from artifact/profile/engine |
| `{build_dir}`, `{build_image}` | from the profile's build |
| `{args}` | the engine's `args_prefix` + defaults overridden by profile, then artifact options, minus `fixed` keys, rendered by `options_style` (layoutctl `ninfer_cli`/`llama_cli`) |
| `{env_args}`, `{mount_args}`, `{label_args}` | rendered lists |
| `{spec_json}` | the card spec as sorted JSON (the `layouts.spec` label; detection and rollback) |
| `{secret_file:id}` | the path of a mounted secret file on that server; never the value |
| `{var:name}` | server vars |
| `{job_id}`, `{state_dir}` | for logs |

### 4.6 Drivers

A driver is a set of generated templates plus a status parser. `template` means the admin
writes them all.

| Driver | start | stop | status | logs |
|---|---|---|---|---|
| `docker` | `docker rm -f` (robust) then `docker run -d --name … --label … {args}` | `docker rm -f` loop until gone (layoutctl `remove_container`) | `docker inspect` → running?, `layouts.spec` label, start time, exit code | `docker logs --tail N` |
| `compose` | `docker start` the service container, else `compose -p P -f F up -d --no-recreate --no-deps S` | `docker stop -t T` | as docker | as docker |
| `systemd` | `systemctl start unit@{card}` (a unit file the admin installs; the env file names the secret file) | `systemctl stop` | `systemctl show -p ActiveState,ExecMainStartTimestamp` | `journalctl -u … -n N` |
| `process` | the router's executor spawns the command detached under a supervisor (`setsid`, pid file in `{state_dir}`) | SIGTERM, then SIGKILL after T | pid alive + command line hash | its log file |
| `template` | `start = '''…'''` | `stop = '''…'''` | `status = '''…'''` printing JSON `{"running": bool, "spec": {...}}` | `logs = '''…'''` |

Every driver must report `spec` (what it runs) so reconcile can compare; docker and compose
read it from the label, systemd and process from a sidecar file the start wrote.

### 4.7 Secrets

- Values come only from `[secrets]`: `file` (read at load and on reload; must be 0600 or
  0640 and not world-readable) or `env`. Config strings that look like a key (`sk-`, `abk_`,
  `Bearer `, long base64) in any field are a validation error with the field's path.
- Engine keys reach engines as **files** (`--api-key-file`, a docker secret mount, a systemd
  `LoadCredential=`), never argv or env; the remote file is written once by hostctl's
  `secret-put` builtin from stdin, 0600, owned by the engine's uid. ninfer needs an
  `--api-key-file` option (we own ninfer; open question 5); llama.cpp has `--api-key-file`.
- Logs, job steps, `/v1/model/info`, the admin API and the UI never show values; captured
  command output is scrubbed for every loaded secret value before it is stored.
- The SSH private key is a secret like any other; there is no password field anywhere in the
  schema.

### 4.8 Validation

`abhoth sothoth config check [--strict]` and every UI save run the same checks:

1. **Syntax and schema** (serde with `deny_unknown_fields`; errors carry file, line and the
   TOML path).
2. **References:** every card's server, layout card's engine/profile/model, artifact for that
   engine, profile listed in the artifact's `profiles` (warning only, as today), route
   targets, upstream ids, secret ids, guard servers, `same_as` chains (no cycles).
3. **Uniqueness:** one card per GPU UUID; no two (server, port) pairs used by one layout;
   route names unique, case-insensitive; container names unique per server.
4. **Capacity:** per layout card, artifact VRAM + profile VRAM ≤ card VRAM − reserve (error;
   `force = true` on the layout makes it a warning, as `compose --force`); lanes ≥ 1; ctx ≤
   engine `max_context`.
5. **Templates:** render every template for every (layout, card) with real values; unknown
   variables, unbalanced braces and fixed-option overrides fail. In `hostctl = "strict"` mode
   the rendered set must match the server's installed allowlist, or the check says which
   templates need `abhoth sothoth hostctl export` (6.2).
6. **Safety:** at least one route reachable by users; `cold = "on_demand"` only on routes
   whose targets are catalog models, never on `pool` or `card` targets (generic and pinned
   names never switch); `on_demand.cards` ⊆ managed cards with `on_demand = true`;
   on-demand impossible for routes with `access = "users"` unless the admin ticks
   `allow_user_switch` (7.2).
7. **Secrets:** inline-looking keys rejected; referenced files exist and are readable.
8. **Drift report** (not an error): the active layout's definition differs from what runs.

### 4.9 Hot reload

- Triggers: file change (mtime, size, inode, as now), `SIGHUP`, `POST /admin/api/reload`, and
  UI saves. A reload parses all included files, validates, and swaps one immutable
  `Arc<Snapshot>`. Invalid config keeps the old snapshot; `/status` shows `configError`.
- `rev` = the first 12 hex digits of SHA-256 over the normalised config (comments stripped,
  keys sorted). It replaces `gateway_rev` in `/v1/model/info`.
- What a reload changes, by class:

  | Change | Effect |
  |---|---|
  | routes, adapters, access, upstreams, budgets, limits, placement, slots TTL | new requests only; in-flight requests keep their snapshot |
  | guards, on-demand settings | next evaluation (within a second) |
  | secrets (file content) | new connections; engine key changes are flagged "engines need a restart to pick up the new key" |
  | engine/profile/model/layout definitions | **never restart anything.** If the active layout's effective spec changed, the server shows **drift** with a plan; the admin applies it (a switch to the same layout name) |
  | cards (new UUID, label, CPU set) | as layouts; a removed card that runs an engine is refused until it is offline |
  | servers (host, SSH) | new connections; running engines are kept |

- Boot is the one exception: at boot (no engines running), reconcile starts the active layout
  **as the file defines it now** (layoutctl does the same).

### 4.10 Versioning, backups, audit

- `version = 1`; `abhoth sothoth config migrate` rewrites older files with `toml_edit` and keeps
  comments. The router refuses a newer major version.
- Every write (UI, CLI `config set`, importer): `router.toml.bak-<UTC time>-<old rev>` in
  `state_dir/backups` (last 100 kept, plus one per day for 90 days), then an atomic rename,
  then an audit row in `router.db`: who (user id, key id or "file"), when, old and new rev,
  the TOML diff (secrets are references, so diffs are safe), and the reason text.
- Saves use optimistic concurrency: the UI sends `If-Match: <rev>`; if the file changed on
  disk meanwhile, the save is refused with both diffs shown.
- Edits made by hand on the file are picked up by the watcher and audited as `file` with the
  diff against the previous snapshot.
- `abhoth sothoth config history` and `config revert <rev>` (which is itself an audited write).

## 5. Engine lifecycle and layout switches

### 5.1 The state machine

One instance per (card, engine). States:

```
            plan/start                     health ok        verify ok
  Off ──────────────────▶ Preparing ──▶ Starting ──────▶ Warming ──────▶ Ready
   ▲                           │             │                │              │
   │                           │fail         │fail/timeout    │fail          │ drain
   │                           ▼             ▼                ▼              ▼
   │                         Failed ◀────────┴────────────────┘          Draining
   │                           │                                             │ idle or drain timeout
   │        stop ok            │ stop/clear                                  ▼
   └──────────────────────── Stopping ◀────────────────────────────────── (stop)
                                ▲
   Unknown (drift, foreign, unlabelled) ──adopt / stop──┘
```

| State | Routable | Entered by | Left when | Timeout (default) |
|---|---|---|---|---|
| Off | no | stop done, card offline | plan starts it | |
| Preparing | no | `prepare` hooks (model restore, `secret-put`, `before_start`) | hooks done | restore: size ÷ learned rate × 2, min 600 s |
| Starting | no | driver start | `/health` passes | engine `health.timeout_seconds` (900) |
| Warming | no | health ok | verify chat ok (restarted cards only) and clocks checked | `verify.timeout_seconds` (180) |
| Ready | **yes** | verify ok, or adopted healthy at router start | drain, crash | |
| Draining | in-flight only | switch, offline, admin | no in-flight requests and no leases, or drain timeout | 120 s, then go anyway only if `force_drain` (switch default: refuse) |
| Stopping | no | driver stop | process gone and GPU memory under the threshold | `stop.timeout_seconds` + `gpu_free_timeout_seconds` |
| Failed | no | any failure | admin clears, or the reconcile loop retries (5.7) | |
| Unknown | no (unless adopted) | observed state does not match a known spec | adopted or stopped | |

A crash (container exits) in Ready goes to Failed and is routed around at once (the data
plane's cooldown already does this on connect errors).

### 5.2 Health, warmth and readiness

- **Health** is the engine's own endpoint with a body check, polled every 3 s, failing fast
  when the driver reports the process gone (log tail attached, secrets scrubbed).
- **Verify** is the one-token chat, sent **directly to the engine**, not through routes, and
  only for instances that (re)started in this job. It cannot trigger anything because it
  never passes the route layer (7.4).
- **Ready** also requires: card telemetry shows the engine's process on the right UUID, and
  the clock check (6.6) passed or is set to `warn`.
- While Ready, health is polled every 15 s. Three consecutive failures (or a connect error from
  real traffic plus one failed poll) mark it unhealthy: not routable, not stopped. The engine
  goes back to Ready on the next pass. Only a gone process is Failed.

### 5.3 Plans

Everything that changes engines is a **plan** first. `plan(server, target)` is pure: inputs are
the snapshot, the observed state, `router.db` (offline cards, leases, guards, active jobs,
history for estimates), outputs are:

- per card: keep / stop / start / restart, with the reason (spec diff, coupling rule);
- the order of steps, grouped (sequential starts, parallel starts);
- preconditions with their status (pass / fail with reason / unknown);
- blocking guards and leases;
- files to restore, with sizes and the estimated time;
- routes whose targets change, slots before and after;
- requests in flight on cards that will drain;
- the estimated duration.

`abhoth sothoth plan <layout> [--server s]`, `GET /admin/api/servers/<s>/plan?layout=` and the
UI plan view (11.4) show it. Executing a plan re-plans first and refuses if the plan changed in
a way that matters (new blocker, different restart set).

### 5.4 Switch orchestration

One job per server at a time; steps are journalled in `router.db` before and after each one, so
a crash can resume or roll back (5.6).

| # | Step | Detail | On failure |
|---|---|---|---|
| 1 | Plan and preconditions | as 5.3, plus per-engine checks: build present, image present, model files present or restorable, templates render | refused (nothing touched) |
| 2 | Guards | `switch` blockers; on-demand switches re-evaluate here with fresh data | refused |
| 3 | Prepare | restore model files from the archive (sha256 checked), write secret files; nothing is stopped yet | refused |
| 4 | Withdraw slots | all cards moving: freeze claims (`switching`); some: remove the moving cards' slots, others keep serving | undo, refused |
| 5 | Lease check | after 1.5 s: any lease (or orphan) on a moving card refuses | undo, refused |
| 6 | Drain | moving cards leave every route at once; in-flight requests finish (up to `drain_timeout`); new requests for routes with other targets go there, the rest wait in admission up to `queue_seconds` | undo, refused (unless `force_drain`) |
| 7 | Lock | server `locks` (watchdog), via hostctl `flock` held for the job | refused |
| 8 | Stop | moving cards, driver stop; wait GPU memory < threshold | rollback |
| 9 | Start | `before_start` hooks; sequential groups first (`start_order = "sequential"`, `share` members in card order), each to Warming; then the rest in parallel | rollback |
| 10 | Verify | chat on restarted instances, health on others; clocks (6.6); compute apps on the right UUID | rollback |
| 11 | Commit | instances Ready; routes, slots and placement switch atomically to the new snapshot of running state; active layout recorded; legacy state files written | |
| 12 | Post | `post_check` route test (informational), OOM kills since step 8, history row, gpu-power hold released | logged only |

**Rollback** runs steps 8-11 towards the previous layout. If that also fails, the job ends
`failed`, the server's active layout becomes `custom` (what was detected), cards with failed
instances stay out of routes, and an alert is raised. Rollback never runs on a refused job
(nothing changed).

`gpu-power` is held (`GET /hold?secs=`) for the job's estimated duration plus 120 s, so the
verify chat and first requests see full clocks (6.6).

### 5.5 Coupling rules

Two rules from `restart_set`, written generally:

- **Shared resource (`share = "expert-arena"`)**: instances of engines in one share group on
  one server form a set. If any member restarts while another member stays, the stayer
  restarts too, after it, in card order. (Strata B maps the arena Strata A fills.)
- **Host-memory order (`restart_if_peer_starts = ["strata"]` on ninfer):** an instance that
  stays while a peer of the listed engines (re)starts on the same server restarts after the
  peer is healthy. (ninfer's pinned host memory must not take NUMA node 0 before the arena
  fill.)

Plus the generic ordering: `start_order = "sequential"` engines start before others and one
at a time. These three keys reproduce today's restart sets for all 15 layouts; a table test
compares them with layoutctl's `restart_set` for every (from, to) pair (14.2).

### 5.6 Concurrency rules

- One switch job per **server** (host-wide resources: NUMA, the arena, watchdog locks); jobs on
  different servers run in parallel.
- Card offline/online and admin restarts are jobs too and share the server's queue.
- On-demand requests for the same target share one job (built); a job for another target makes
  them wait (built).
- The data plane never waits on the control plane's locks: routing reads the snapshot of
  running state that step 11 swaps.
- Config reloads during a job are applied to routing at once; the job keeps the plan it
  started with.
- Leases may be claimed on cards that do not move while a partial switch runs.

### 5.7 Crash recovery and reconcile

- **Router start:** load config; open `router.db`; observe every server (driver status,
  labels, telemetry). Each running instance whose `spec` matches a card of the active layout
  is **adopted** as Ready after one health pass, with no restart. A router restart must
  never restart an engine.
- **Interrupted job** (journal shows a job without an end): mark it `interrupted`. If the
  observed cards match the job's target, finish steps 10-12 (verify restarted cards, commit);
  if they match the source, record `rolled-back`; else run the rollback steps.
- **Reconcile loop** (every 60 s per server; replaces `ensure` and the watchdog hooks):
  - an instance the active layout wants and that is not running: start it (no lease check,
    as now), **3 tries per card per boot** with backoff 30 s, 120 s, 600 s, then Failed plus an
    alert; skipped while a guard blocks `ensure` (music mode);
  - running something else on a managed card: Unknown, never stopped automatically, except
    at boot when `reconcile.switch_on_boot = true` (layoutctl switches then, ignoring the quiet
    window);
  - foreign GPU holders: shown, block switches, never touched;
  - unlabelled engine containers: Unknown with what could be read from their args.
- **Boot reconcile** (router starts with the host): wait for the server's `ready` checks up to
  `ready_timeout_seconds`, then the loop above with `switch_on_boot`.
- **SSH unreachable:** the server goes `degraded`; its engines stay routable while their HTTP
  health passes (the data plane does not need SSH); switches and reconcile wait; telemetry is
  marked stale.

## 6. Execution: local and SSH

### 6.1 Executors

`exec = "local"` spawns `/bin/sh -c <rendered>` with a timeout, captured stdout/stderr (capped
at 64 KiB each, scrubbed of secret values), exit code and duration. `exec = "ssh"` sends the
same request over SSH. Both implement one trait (`Executor::run(Request) -> Output`), so tests
use a fake executor.

On DeepThought the router runs in a container, so "local" is not the host. The recommendation is
**SSH to the host's loopback with the same forced-command setup as any remote server** (D-RC4):
one security model, no docker socket and no root inside the container.

### 6.2 The security model

```
router container ──SSH (ed25519, pinned host key)──▶ gpu-host sshd
                                                      └─ authorized_keys: command="abhoth hostctl serve",
                                                         restrict,no-pty,no-port-forwarding,from="<router addr>"
                                                         user abhoth-exec (no password, no shell login)
                                                              └─ sudo (one entry): /usr/local/bin/abhoth hostctl exec
                                                                      checks /etc/abhoth/hostctl.toml (root-owned)
```

- **Key:** one ed25519 key per server, generated by `abhoth sothoth ssh keygen <server>` into
  the secrets dir; the public half is installed by an admin. No passwords anywhere; the
  router never asks for one.
- **User:** a dedicated `abhoth-exec` per host, no password (`passwd -l`), shell
  `/usr/sbin/nologin` except through the forced command.
- **Forced command:** `authorized_keys` pins `command="abhoth hostctl serve"` with `restrict`
  and `from=`. Whatever the client asks to run is ignored; `hostctl serve` reads framed JSON
  requests on stdin.
- **The allowlist:** `/etc/abhoth/hostctl.toml`, root-owned 0644, holds every template the
  router may run on this host **as text**, with a type for each variable (`card ∈ {A,B}`,
  `port: u16 in 8000-8999`, `model_file: ^[A-Za-z0-9._-]+$`, `path: under $AI_ROOT`, …).
  A request names a template id plus variable values; hostctl validates the values, renders
  the template itself and runs it. **The router never sends command text in strict mode**,
  so a compromised router can only run the allowlisted templates with valid values.
- **Privilege:** most templates run as `abhoth-exec`. Those that need root (docker, `nvidia-smi
  -i … -rgc`, drop caches, reading `/proc/*/cmdline` of other users) are marked `root = true`
  in the allowlist and run through the single sudoers entry
  `abhoth-exec ALL=(root) NOPASSWD: /usr/local/bin/abhoth hostctl exec`. Note: access to
  docker is root-equivalent; the allowlist, not the user, is the boundary.
- **Installing changes:** `abhoth sothoth hostctl export <server>` writes the allowlist the
  current config needs; an admin copies it to the host (`sudo abhoth hostctl install
  file.toml`, which shows a diff). The UI shows "hostctl allowlist out of date" with the
  missing template ids until the hash matches, and plans that need a missing template are
  refused. Template edits therefore need a person with root on the host, on purpose.
- **Builtins** in hostctl cover what is awkward in bash and safety-critical: `flock` (hold
  named locks for a job's lifetime), `gpu-free-wait`, `numa-room`, `json-set`, `restore-file`
  (copy from the archive manifest with sha256 check and rate log), `oom-kills`, `proc-scan`
  (video guard), `secret-put`, `docker-rm-robust`, `telemetry`.
- **Trusted mode** (`hostctl = "trusted"`, development and single-user hosts only): the router
  sends rendered command text; the forced command still logs every request. Validation warns
  loudly while any server is in trusted mode.
- **Timeouts:** connect 5 s; per command `timeout` (default 120 s; restores and starts have
  their own); on timeout hostctl kills the process group. **Output:** captured and capped as
  in 6.1, stored with the job step.
- **Audit on the host:** hostctl appends one line per request to
  `/var/log/abhoth-hostctl.log` (template id, variables, exit code, duration), independent of
  the router's own audit.
- **Transport:** `russh` (pure Rust, pinned), one connection per server with multiplexed
  channels, keepalive 15 s, host key pinned in config (`host_key`); first contact shows the
  fingerprint in the UI for an admin to accept, never trust-on-first-use silently.

### 6.3 Locks shared with the old tooling

Until the qfn-llm watchdog loses its layout hooks (13.3), the router takes the same lock files
(`/run/qfn-watchdog.lock`, `/run/qfn-start.lock`) through hostctl `flock` during steps 7-10,
exactly as layoutctl does. After cutover the watchdog keeps only "start CRASHED GPU apps", and
the locks keep it from touching layout containers mid-switch.

### 6.4 Engine keys

`secret-put` writes the engine key to `/run/abhoth/secrets/<id>` (tmpfs, 0600, owned by the
engine uid) on the host; drivers mount it read-only (`/run/secrets/engine-key`) and pass
`--api-key-file`. The router injects `Authorization: Bearer` from its own copy (built). Keys
never appear in `docker inspect`, `ps` or job logs.

### 6.5 Telemetry and discovery

- **Stream:** hostctl `telemetry` runs `nvidia-smi --query-gpu=uuid,index,name,pci.bus_id,
  memory.total,memory.used,utilization.gpu,temperature.gpu,temperature.memory,power.draw,
  clocks.sm,clocks.max.sm,clocks.applications.graphics,clocks_event_reasons.active
  --format=csv,noheader,nounits -lms 2000` and streams lines back on one channel; the router
  parses by UUID. Every 10 s also `--query-compute-apps=gpu_uuid,pid,used_memory` plus the
  owning container (cgroup → container name), for foreign holders and "is the engine on its
  card".
- **Fallback:** if streaming is not possible, poll every 5 s.
- **Discovery:** the same query lists every GPU; UUIDs not in config appear as "discovered" in
  the UI with an "adopt as card" action that writes a `[cards.x]` block (`managed = false` by
  default). A configured UUID that disappears marks the card `missing` (switches refused,
  routes skip it).
- **Staleness:** telemetry older than 15 s is marked stale; anything that needs it (on-demand
  idleness, clock verify) treats stale as unknown, which means wait, never act.
- **Host facts** for guards (`proc-scan`, file checks) use the same connection every 5 s.

### 6.6 Clocks

The router does not set clocks in normal operation; `gpu-power`, `clockctl` and the boot script
do. It does:

- **Verify after start** (step 10): `clocks.max.sm` reaches the card's `max_sm_mhz` (A: 1597)
  when unlocked, and `clocks_event_reasons` shows no application/user cap; applications clocks
  match `app_clocks` (B: 877,1380). `expect = "unlocked"` with `on_mismatch = "warn"`
  (default) or `"fix"`, which runs the allowlisted `nvidia-smi -i {gpu_uuid} -rgc` /
  `-ac` template, or `"fail"`.
- **Hold** gpu-power during switches (5.4) and expose `POST /admin/api/cards/<c>/hold` for
  benches.
- **Show** clocks, power and temperatures per card in the UI and `/status`.

Taking over clock management is a later, separate decision (open question 7).

## 7. Guards and the cold-model policy

### 7.1 Guards

A guard is a named condition, evaluated continuously, that blocks a set of actions:
`switch` (any layout change, admin or on-demand), `on_demand`, `ensure` (reconcile starts),
`offline`/`online`.

| Kind | Source | Today |
|---|---|---|
| `schedule` | time windows in `router.timezone` (chrono-tz, pinned; replaces the hand-made DST code of D-R14) | quiet window 06:45-07:30 |
| `process` | hostctl `proc-scan` with argv/exe matching and a label from an argument | video pipeline `run_daily.sh` |
| `json_file` | a JSON file's keys truthy | bench queue `running`/`current` |
| `file_exists` | a path exists | music mode, `DENSE_LEAD` |
| `http` | GET a URL, active when the JSON path is truthy | (new) |
| `lease` | an active guard lease (8.3) of a given kind | (new) bench, video, maintenance |
| `admin_hold` | set from the UI with a reason and expiry | `gateway --hold`, but for switches |

Rules carried over from `ondemand.py`:

- each guard has its own `retry_seconds` for `Retry-After`;
- **a guard whose source has not reported for 45 s counts as active** ("stale") for
  `on_demand` and `switch` blocks; for `ensure` stale counts as inactive (starting a missing
  engine is the safe default after a reboot);
- on-demand jobs re-evaluate guards at step 2 with fresh data.

### 7.2 Cold-model policy per route

A route is **cold** when none of its targets has a Ready upstream. The policy:

| `cold` | Behaviour |
|---|---|
| `reject` (default) | 503 `model_not_live` in the D-R4 error shape, `Retry-After` when a switch is in progress anyway |
| `fallback` | use `fallback = [routes…]` in order (each with its own targets); response header `x-abhoth-fallback: <route>`; the served model header says what really answered |
| `on_demand` | ask for a switch (below), then forward; while waiting: hold the request (SSE notes and keepalives for streams), or answer from `fallback` if `fallback_while_waiting = true` |

**On-demand** requires **all** of:

1. the route says `cold = "on_demand"` and its targets are catalog models (validation 4.8);
2. the request came through a **chat endpoint** (`/v1/chat/completions`, `/v1/responses`,
   `/v1/messages`) with a body that passed parsing; no other code path can construct a switch
   request (7.4);
3. the caller's principal is allowed: route `on_demand.access` (default `admins`, or a key
   list such as `keys:switchers`); anonymous LAN callers never switch;
4. the request does **not** carry `x-abhoth-no-switch: 1` (set by routers chaining to us, by
   the canary and by every internal caller);
5. `decide()` (ported, built) returns `switch`: guards, leases on the chosen card, minimum stay,
   idleness from in-flight counts plus engine `/slots` polling, idle-longest choice (ties to the
   card listed last in `on_demand.cards`, B today), other jobs, pending switch sharing;
6. **rate limits:** `max_switches_per_hour` per server (default 4), per principal 2 per hour,
   `failure_hold_seconds` per target;
7. the chosen card has `on_demand = true` and is not offline/excluded; the target model is not
   excluded (`cold = "reject"` on its route is how `exclude` maps).

The switch itself is an ordinary job (5.4) with `requested_by = "on_demand:<route>:<principal>"`,
built as a one-card change of the active layout (`layout_from = "pick"`: card ← engine/profile/
model), registered as a `source = "on_demand"` layout so the profiles shim shows it as
layoutctl's composed ones were.

### 7.3 Server-wide brakes

`[on_demand]` holds the server defaults (queue 300 s, minimum stay 300 s, idle grace 10 s,
timeout 900 s, failure hold 60 s, `allow_both_cards = false`, `max_switches_per_hour`). Routes
may only tighten them.

### 7.4 Why a probe can never switch

The class of bug from ROUTER.md 1.1 (a health check that sends real chats) is closed
structurally, not by convention:

- **Type-level:** `SwitchRequest` can only be built from a `ChatAdmission` value, which only
  the three chat handlers produce after authentication. `/health`, `/health/liveliness`,
  `/v1/models`, `/v1/model/info`, `/status`, `/metrics`, the canary, verify chats and
  `post_check` never produce one. Verify and `post_check` go to engines directly, not through
  routes.
- **Principal-level:** switching needs a principal with the switch permission; health probers
  (dash, LiteLLM if chained, Prometheus) have none.
- **Header-level:** every request the router itself sends upstream carries
  `x-abhoth-no-switch: 1`, so a chain of routers cannot switch each other's GPUs by probing.
- **Default-level:** routes default to `reject`; `on_demand` is opt-in per route.
- **Test-level:** a test walks every route in the axum router with every method and asserts
  that only the chat handlers can reach `ondemand::request_switch` (14.2).

## 8. Leases and slots

### 8.1 What exists

`slots.rs` already serves the pig-slots protocol with the same JSON, TTL and freeze, knows
each lease's card, and feeds lease-aware placement and Lead-priority admission (D-R11, D-R12).

### 8.2 What changes

- **Slot list from the active layout:** `naming = "{card}{n}"` over each serving card's lanes
  (A1-A4, B1-B4 today), instead of `layout.json`. Offline cards and moving cards lose their
  slots exactly as in 5.4 steps 4-5.
- **Freeze** is set and cleared by the orchestrator in memory (no file), and also written to
  the legacy `layout.json` while the mirror is on.
- **Persistence:** leases in `router.db`; on restart, leases not renewed within TTL expire as
  now.
- **Lead reserve and agent lanes** (ROUTER.md D-R11): a `kind: main` lease reserves
  `lead_reserve` lanes on its card; agents and unleased traffic queue at the lane limit; the
  Lead never queues. `agentsPerCopy` stays a client setting (layout `client` tuning).
- **Principal:** claims through the native API record the user/key; claims through the shim
  record the shim's principal (anonymous LAN or a service key).

### 8.3 New lease kinds

- `kind: guard` with a `reason` (bench, video, maintenance): blocks switches and on-demand
  while held, renewed like any lease. The bench and video pipelines can move from file/process
  probes to leases at their own pace; the probes stay for parity.
- Native endpoints `POST /v1/leases`, `POST /v1/leases/renew`, `DELETE /v1/leases/{key}`,
  `GET /v1/leases` (own leases; admins see all), authenticated.

### 8.4 Compatibility shims

| Listener | Serves | Auth |
|---|---|---|
| `:8099` | pig-slots `/claim`, `/renew`, `/release`, `/list`, `/`, `/health`, unchanged JSON | as today (LAN, anonymous), switchable to "service key required" |
| `:8092` | qfn-lb paths (built) | engine key added by the router, as today |
| `:8098` | profiles API: `GET /health`, `/profiles` (ETag, summaries, facts), `/live`, `/plan`, `/jobs`, `/jobs/<id>`, `/cards`, `/ondemand`; `POST /switch`, `/compose`, `/profiles/save`, `/profiles/delete`, `/cards/<X>/offline|online` | GETs: anonymous on the LAN (no secrets in them). POSTs: an admin `abk_` key or a service key with the `switch` permission (HA, Jarvis); anonymous POSTs answer 401 with a message naming the admin UI |

The `:8098` shim renders profiles.json-shaped JSON from the config (layouts + client
profiles + catalog), so Abhoth's `ProfileService`, the web UI's profile builder, HA and Jarvis
keep working. Jobs keep their shape (`state` queued/running/done/rolled-back/failed/refused,
`steps` with seconds).

## 9. Upstreams

### 9.1 Kinds

| Kind | API spoken | Auth | Health | Notes |
|---|---|---|---|---|
| `engine` (implicit) | engine `api` | engine key file | engine `health` | one per Ready instance |
| `openai` | openai-chat (+ responses if `also_speaks`) | Bearer from secret | `GET /models` once a minute, optional | OpenAI, OpenRouter, vLLM, anything compatible |
| `anthropic` | anthropic-messages | `x-api-key` + `anthropic-version` | none (no free probe); passive from traffic | |
| `router` | openai-chat (+ responses/messages if declared) | Bearer (a key the other router issued us) | `GET /health/liveliness` (`flavour = "litellm"`) or `/health` (`flavour = "abhoth"`) | chaining (9.4) |
| `bridge` | reserved | | | 9.5 |

Per upstream: `base_url`, `key`, `headers`, `timeout_seconds`, `connect_timeout`,
`limits.concurrency`, `limits.requests_per_minute`, `limits.tokens_per_minute`, `models`
(allowlist; `*` allowed), `prices` ($/MTok input, output, cache read, cache write), `retry`
(429/5xx before first byte: 1 retry with `Retry-After` respected, cloud only).

Passive health for cloud: 5 consecutive 5xx or connect errors → cooling 60 s, then a single
request probes it again. Never a synthetic chat.

### 9.2 Keys for cloud

Cloud keys are the new accounts' API keys, each a `[secrets]` entry. One key per upstream by
default; `keys = ["secret:a", "secret:b"]` spreads load (round robin, skipping a key that
answered 429 for its `Retry-After`).

### 9.3 Budgets and limits

- **Scopes:** `user:<name>`, `user:*` (each user separately), `key:<key id>`,
  `route:<name>`, `global`. **Upstream match:** an id, a glob, or `*cloud` (every non-engine
  upstream).
- **Limits:** `usd` per `period` (day, week, month; calendar in `router.timezone`),
  `requests_per_minute`, `tokens_per_minute`, `concurrency`.
- **Enforcement:**
  1. before sending: estimate the worst case: input tokens (characters ÷ 3.5, plus image token
     estimates) × input price + `max_tokens` × output price; **reserve** it in the ledger;
     refuse with 429 `budget_exceeded` (OpenAI error shape, `type: "insufficient_quota"`) if
     any matching budget would go over;
  2. after the answer: settle with real usage (cache reads at their price) and release the
     rest; for streams, settle from the final usage event, or from counted output if the
     stream was cut;
  3. when a route has a `fallback` and the budget refuses, use the fallback instead of failing
     (header `x-abhoth-fallback: budget`).
- **Ledger:** `router.db` table `spend(ts, principal, key_id, route, upstream, model, input,
  output, cache_read, cache_write, usd, request_id)`, plus per-period rollups. Local engines
  are logged with `usd = 0` so the same reports show local use.
- **Views:** users see their own spend and remaining budget (`GET /v1/me/usage`); admins see
  all, with 80% and 100% alerts.
- **Without max_tokens:** cloud requests get the route's `default_max_tokens` (Anthropic needs
  one anyway); the reservation uses it.

### 9.4 Router chaining

- `kind = "router"` sends with `x-abhoth-no-switch: 1`, `x-abhoth-hop: <n+1>` and
  `x-abhoth-via: <router id>`. Incoming requests with `hop ≥ max_hops` (2) or our own id in
  `via` are refused (loop), with 508.
- An Abhoth upstream router authenticates us with a key it issued (a service user there);
  budgets apply on both sides.
- LiteLLM as an upstream (`flavour = "litellm"`) is only probed on `/health/liveliness`; the
  validator refuses any configured probe path of `/health` for it.
- Model names on the far side are given per target (`model = "…"`), so names never leak
  through.

### 9.5 The bridge hook

BRIDGE.md 10.3 needs private upstreams registered at run time and user-bound keys. Reserved now:

- `kind = "bridge"` is a valid kind that the validator accepts only with `enabled = false`;
- the upstream registry has a second source besides the config: `RuntimeUpstreams`
  (`POST/DELETE /admin/api/upstreams/runtime`, admin or the Abhoth server's service key),
  whose entries carry `owner` and are never part of pools, fallbacks or on-demand;
- route resolution already takes the principal, so "only routable for its owner, unknown
  model for everyone else" is a filter in one place.

## 10. Translation adapters

### 10.1 Architecture

```
client API (ingress)        route/engine adapter chain                       upstream API (egress)
 chat | responses | messages ─▶ decode ─▶ [request stages] ─▶ encode ─▶ upstream
                              ◀─ encode ◀─ [response stages] ◀─ decode ◀─ (JSON or SSE)
```

- **Passthrough first.** When the client's API equals the upstream's API and the chain has
  no stage that needs the body, bytes pass untouched (today's path, D-R7). The JSON body is
  still parsed once for routing and the `params` stage edits it in place (built).
- **Canonical form** for translation: an internal `Turn` model that is a superset of the three
  APIs (system/developer text, messages with typed parts: text, image, tool call, tool result,
  reasoning with an opaque provider payload; tools; sampling; reasoning settings; response
  format; cache hints; stop sequences; metadata). Decoders and encoders exist per API; any pair
  translates through it. Fields the target cannot express follow the route's `unsupported`
  policy: `drop` (default for sampling knobs), `error` (default for `n > 1`, `logprobs`),
  `warn` (drop plus a response header `x-abhoth-dropped: <fields>`).
- **Streaming** is an event state machine per direction: upstream events are decoded into
  canonical deltas (`text`, `reasoning`, `tool_call_start{index,id,name}`,
  `tool_call_args{index,fragment}`, `tool_call_end`, `usage`, `finish{reason}`, `error`) and
  re-encoded; no buffering beyond what a format forces (10.4).
- **Chains** are lists of named stages, set on an engine (`[engines.x] adapters = [...]`), an
  upstream, or a route; the effective chain is route ∘ upstream ∘ engine, each stage at most
  once. Stages: `responses-in`, `messages-in`, `chat-to-anthropic`, `anthropic-to-chat`,
  `tool-normalise`, `reasoning`, `params`, `reasoning-out` (where reasoning text goes in the
  answer). Ingress stages are chosen by the endpoint, egress by the upstream's `api`; the
  config only names the optional ones.

### 10.2 Responses API in

`POST /v1/responses` (plus SSE). Upstreams that speak Responses (ninfer, OpenAI) get it
passed through; others get chat.

| Responses request | Chat request |
|---|---|
| `instructions` | first `system` message |
| `input` string | one `user` message |
| `input[]` message items, `role` user/assistant/system/developer | messages (developer → system) |
| `input_text` / `input_image` (`image_url` or `file_id`) | text / `image_url` parts (`file_id` refused: no file store) |
| `function_call` item `{call_id, name, arguments}` | assistant `tool_calls[{id: call_id, function{name, arguments}}]` (consecutive calls merged into one assistant message) |
| `function_call_output` `{call_id, output}` | `tool` message `{tool_call_id, content}` |
| `reasoning` items | dropped, or `reasoning_content` on the assistant message for engines with `preserve-thinking` |
| `tools[{type: function, name, description, parameters, strict}]` | `tools[{type: function, function{…}}]` |
| built-in tools (`web_search`, `file_search`, …) | refused 400 unless the upstream speaks Responses |
| `tool_choice` `auto`/`none`/`required`/`{type: function, name}` | same values, nested form |
| `max_output_tokens` | `max_tokens` |
| `reasoning.effort`, `reasoning.summary` | `reasoning_effort` (then the `reasoning` stage); summary ignored |
| `text.format` `{type: json_schema …}` / `json_object` | `response_format` (then `params` may drop it) |
| `temperature`, `top_p`, `parallel_tool_calls`, `metadata`, `user` | same |
| `previous_response_id` | the stored conversation (below) prepended |
| `store`, `background`, `include`, `truncation` | `store` honoured; `background` refused; others ignored |

**Stored responses:** with `store` (default true in the OpenAI API) the router keeps the full
input and output items of each response for 24 h (`responses.ttl_hours`), keyed by id and
owned by the principal, in `router.db`; `previous_response_id` from another principal is "not
found". `store: false` keeps nothing.

| Chat response / stream | Responses output / events |
|---|---|
| `id`, `created`, `model` | `id: resp_…`, `created_at`, `model` |
| `message.reasoning_content` | `output[]` item `{type: reasoning, summary: [{type: summary_text, text}]}` |
| `message.content` | item `{type: message, role: assistant, content: [{type: output_text, text, annotations: []}]}` |
| `message.tool_calls[]` | items `{type: function_call, call_id, name, arguments}` |
| `finish_reason` stop / tool_calls | `status: completed` |
| `finish_reason` length | `status: incomplete`, `incomplete_details.reason: max_output_tokens` |
| `finish_reason` content_filter | `status: incomplete`, reason `content_filter` |
| `usage.prompt_tokens`, `completion_tokens`, `prompt_tokens_details.cached_tokens`, `completion_tokens_details.reasoning_tokens` | `usage.input_tokens`, `output_tokens`, `input_tokens_details.cached_tokens`, `output_tokens_details.reasoning_tokens` |
| stream: first chunk | `response.created`, `response.in_progress` |
| reasoning deltas | `response.output_item.added` (reasoning), `response.reasoning_summary_text.delta` … `.done`, `response.output_item.done` |
| content deltas | `response.output_item.added` (message), `response.content_part.added`, `response.output_text.delta` …, `response.output_text.done`, `response.content_part.done`, `response.output_item.done` |
| tool call deltas | `response.output_item.added` (function_call), `response.function_call_arguments.delta` … `.done`, `response.output_item.done` |
| end | `response.completed` (or `response.incomplete`), with usage; `sequence_number` on every event |

### 10.3 OpenAI chat ⇄ Anthropic Messages

Both directions: `chat-to-anthropic` (an OpenAI-style client using a Claude upstream) and
`anthropic-to-chat` (a Messages client, e.g. Claude-Code-style tools, using a local engine).
`POST /v1/messages` is a new router endpoint for the second.

**Requests**

| OpenAI chat | Anthropic Messages | Notes |
|---|---|---|
| `system`/`developer` messages | top-level `system` (blocks) | leading ones; later ones become mid-conversation `role: system` messages where the model supports them, else appended to `system` with a warning |
| `user` text / `image_url` (data URL) | `text` / `image {source: {type: base64, media_type, data}}` | |
| `image_url` (http URL) | `image {source: {type: url, url}}` | |
| assistant `content` + `tool_calls[{id, function{name, arguments}}]` | assistant `text` + `tool_use {id, name, input}` | `arguments` (a JSON string) parsed into `input`; invalid JSON → 400 naming the call, never sent half-parsed |
| `tool` messages `{tool_call_id, content}` | **one** `user` message with all consecutive `tool_result {tool_use_id, content}` blocks | splitting them across messages teaches the model to stop calling in parallel |
| assistant `reasoning_content` + opaque `reasoning_details` | `thinking` block `{thinking, signature}` passed back unchanged | only when the payload came from the same upstream model (10.3 hard cases) |
| `tools[{function{name, description, parameters, strict}}]` | `tools[{name, description, input_schema, strict}]` | |
| `tool_choice` `auto` / `none` | `{type: auto}` / `{type: none}` | |
| `tool_choice` `required` / named | `{type: any}` / `{type: tool, name}` | models that refuse forced choice (capability table): `auto` plus a system line naming the tool; header `x-abhoth-downgraded: tool_choice` |
| `parallel_tool_calls: false` | `tool_choice.disable_parallel_tool_use: true` | |
| `max_tokens` / `max_completion_tokens` | `max_tokens` (required: route default when absent) | |
| `stop` | `stop_sequences` | |
| `temperature`, `top_p` | same, or dropped for models that refuse them (capability table) | |
| `reasoning_effort` | `output_config.effort` (+ `thinking: {type: adaptive}` where needed) | 10.5 mapping; `none` → the model's thinking-off form, or `low` where thinking cannot be disabled |
| `response_format` json_schema | `output_config.format` | `json_object` → refused by policy `error` or translated to a permissive schema (`warn`) |
| `user` | `metadata.user_id` (hashed principal by default) | |
| `n > 1`, `logprobs`, `logit_bias`, `seed`, `presence/frequency_penalty` | none | `unsupported` policy |
| `stream_options.include_usage` | always available (`message_delta.usage`) | |
| `prompt_cache_key` | none; cache hints below | |

**Responses**

| Anthropic | OpenAI chat |
|---|---|
| `text` blocks | `message.content` (concatenated) |
| `thinking` blocks | `reasoning_content` (text; may be empty when the model omits it) + `reasoning_details: [{type: "anthropic.thinking", data: <block>}]` |
| `tool_use {id, name, input}` | `tool_calls[{id, type: function, function{name, arguments: JSON string of input}}]` |
| `stop_reason` `end_turn`, `stop_sequence` | `finish_reason: stop` |
| `max_tokens`, `model_context_window_exceeded` | `length` |
| `tool_use` | `tool_calls` |
| `refusal` | `content_filter` (with `stop_details.category` in `x-abhoth-refusal`) |
| `pause_turn` | not exposed: only server tools cause it, which are not offered through chat |
| `usage.input_tokens + cache_read_input_tokens + cache_creation_input_tokens` | `prompt_tokens` (sum) |
| `usage.cache_read_input_tokens` | `prompt_tokens_details.cached_tokens` |
| `usage.output_tokens` | `completion_tokens` |

**Streams (Anthropic → chat):** `message_start` → first chunk (role, id, model);
`content_block_start` text / `content_block_delta` `text_delta` → `delta.content`;
`thinking_delta` → `delta.reasoning_content`; `signature_delta` kept for `reasoning_details`
in the final chunk; `content_block_start` tool_use → `delta.tool_calls[{index, id, function{name,
arguments: ""}}]`; `input_json_delta.partial_json` → `delta.tool_calls[{index, function{arguments:
fragment}}]`; `message_delta` → `finish_reason` + usage chunk (when the client asked for it);
`message_stop` → `[DONE]`; `ping` → SSE comment; `error` event → an error chunk then close.

**Streams (chat → Anthropic, for `/v1/messages` clients):** first chunk → `message_start`
(usage input tokens if known, else 0 and corrected in `message_delta`); text deltas → a text
block (`content_block_start`, `text_delta`…, `content_block_stop`); `reasoning_content` deltas
→ a `thinking` block before it (no signature: local engines have none, and the adapter marks
these blocks so they are never sent to a real Claude upstream); tool call deltas → one
`tool_use` block per index with `input_json_delta`; `finish_reason` → `message_delta`
(`stop_reason` per the reverse of the table above) then `message_stop`; keepalives → `ping`.

**Hard cases**

- **Interleaved parallel tool calls.** OpenAI chunks may interleave indexes; Anthropic blocks
  are sequential. The encoder streams the first open call and buffers later indexes until it
  closes (in practice engines stream calls in order, so buffering is rare and bounded by the
  response).
- **Thinking round-trips.** Claude models check that thinking blocks are passed back unchanged
  and in the same conversation; an OpenAI client drops unknown fields. The adapter returns
  `reasoning_details` (OpenRouter's convention, kept by clients that preserve message objects,
  Abhoth included) and, as a fallback, keeps the last 64 thinking payloads per principal for
  1 h keyed by the hash of the assistant message they belong to, re-attaching them when that
  message comes back. Blocks from another model are dropped (the API would drop them anyway).
- **Images:** size and type limits differ (Anthropic: base64 or URL, set media types; local
  engines: data URLs only). The adapter converts URL images to data URLs for engines that
  cannot fetch (`fetch_images = true` on the engine; off by default: the router fetching URLs
  is a server-side request risk, so it is admin-enabled and limited to http(s), 20 MB, 10 s,
  no private address ranges).
- **Cache control.** Anthropic `cache_control` breakpoints from `/v1/messages` clients pass
  through to Claude and are removed for local engines (they cache prefixes automatically;
  `prompt_cache_key` is set from the session key instead). For chat clients on a Claude route,
  `cache = "auto"` (route option, default on) adds top-level automatic caching; explicit
  breakpoints can't be expressed in chat and are not invented.
- **Stop reasons and errors:** upstream HTTP errors keep their status; bodies are re-shaped
  into the client's error format (`{"error":{…}}` vs `{"type":"error","error":{…}}`) with the
  original message kept.
- **Usage accounting:** prompt token totals differ in meaning (Anthropic splits cached input
  out); the ledger stores the raw fields and the translated view is computed, so budgets use
  real cache prices.
- **System text in mid-conversation** and **developer** roles: mapped as above; never dropped
  silently.

### 10.4 Tool-call normalisation

For engines whose templates sometimes emit tool calls as text (`tool_parser` per engine or per
model artifact; `auto` tries the known formats in order):

| Parser | Recognises |
|---|---|
| `hermes-json` | `<tool_call>{"name": …, "arguments": {…}}</tool_call>` (Qwen, Hermes) |
| `qwen-xml` | `<tool_call><function=name><parameter=k>v</parameter>…</function></tool_call>` (Qwen3 coder templates) |
| `json-content` | the whole content is a JSON object or array with `name` + `arguments`/`parameters` |
| `mistral` | `[TOOL_CALLS][{…}]` |
| `pi` | the Pi tool-call format our Pi-tuned models emit (recorded examples decide its grammar) |

- **Streaming:** the parser holds back text only while it could still be the start of a
  marker (lookahead at most the longest marker, ~16 bytes), so normal text streams with no
  added delay; inside a call, arguments are emitted as `tool_calls` deltas once a parameter
  value is complete (XML) or as JSON fragments (JSON forms).
- **IDs:** missing, empty, duplicate or invalid ids (Anthropic allows `^[a-zA-Z0-9_-]+$`) are
  replaced with `call_<12 base62>`; the mapping is kept for the response so tool results that
  come back with our id match.
- **Arguments:** always a JSON string in chat; an object is serialised; invalid JSON is passed
  as text with `finish_reason: stop` and `x-abhoth-tool-parse: failed` under `repair = "none"`
  (default), or closed with a minimal repair (missing braces/quotes) under `repair = "light"`.
- **Finish reason:** set to `tool_calls` when calls were parsed and the engine said `stop`.
- **Parallel calls:** several calls in one answer become several `tool_calls` entries with
  sequential indexes.
- Native `tool_calls` from the engine (ninfer) pass untouched; the parser is off for them.

### 10.5 Reasoning and params mapping per engine

The existing rules (ported from `effort_hook.py`, D-R6) are the first case of a general table.
Per engine (and per model artifact, which wins), `params` sets:

| Setting | Effect | DeepThought values |
|---|---|---|
| `effort_levels` | clamp `reasoning_effort` (accepted passes; else highest below, never `none` unless asked; else lowest above) | ninfer `none, low, medium, xhigh` |
| `thinking_off` | when `enable_thinking: false` anywhere (top level, `chat_template_kwargs`, `extra_body`, nested): `drop` the effort, or set a level | ninfer `drop`; Inkling artifact `none` |
| `default_effort` | added when the client sent none | Strata `low` |
| `drop` | fields removed | ninfer `response_format` |
| `strict_fields` + `allow_fields` | only listed top-level fields pass (engines that 400 unknown fields) | ninfer (list verified in shadow, since `uncensored` passes today) |
| `effort_to` | where effort goes: `reasoning_effort` (OpenAI/llama.cpp/ninfer), `chat_template_kwargs.reasoning_effort`, `output_config.effort` (Anthropic), `reasoning.effort` (OpenRouter) | |
| `thinking_to` | where thinking on/off goes: `chat_template_kwargs.enable_thinking`, top-level `enable_thinking`, Anthropic `thinking` | |
| `reasoning_out` | where the engine puts reasoning: `reasoning_content`, `reasoning`, `<think>` tags in content → normalised to `reasoning_content` (or left as is: `reasoning_out = "raw"` for clients that strip `<think>` themselves) | |

The effort vocabulary is ordered `none < minimal < low < medium < high < xhigh < max`; every
mapping is a clamp onto the target's list, so new engines only declare their list.

### 10.6 Golden corpus

- Layout: `crates/orca-router/tests/golden/<adapter>/<case>/` with `client.request.json`,
  `upstream.request.json` (expected), `upstream.response.{json,sse}` (recorded),
  `client.response.{json,sse}` (expected), `meta.toml` (route, engine, flags).
- Recorded from real upstreams once (ninfer, llama.cpp, Strata when live, OpenAI, Anthropic,
  OpenRouter) with tiny `max_tokens`, keys stripped and ids/timestamps normalised by a
  recorder tool (`abhoth sothoth record`); re-recording is a manual step that shows a diff.
- Replayed by a fake upstream; SSE compared event by event after normalising ids and times.
- Minimum cases per adapter: text; system + developer; images (data URL, http URL); one tool
  call; parallel tool calls; tool results with an error; streaming of each; reasoning on and
  off; effort at every level; json response format; max_tokens stop; stop sequence; refusal;
  upstream 4xx and 5xx before and during the stream; client disconnect; usage with and without
  cache reads; Responses `previous_response_id`; text-embedded tool calls of each parser with
  markers split across chunks.
- The existing effort-hook table tests and the shadow `shapes.py` cases (19 client shapes)
  join the corpus as `params` cases.

## 11. Access and the admin web UI

### 11.1 Principals and permissions

- **Users and keys come from D140** (`abhoth.db`: users with roles, per-user `abk_…` keys
  stored as SHA-256). The router does not get its own user store.
- **How the router checks a key** (recommended, D-RC6): introspection. The router calls the
  Abhoth web server's `POST /api/internal/keys/introspect` (authenticated with a service
  token, `secrets.accounts_token`) with the key's SHA-256, gets `{user_id, name, role,
  key_id, disabled}` and caches it for 60 s (negative answers 10 s). A revoked key stops
  working within a minute. If the web server is down, cached keys keep working for up to 15
  min; uncached ones fail with 503. Alternative: open `abhoth.db` read-only through a shared
  volume (open question 2).
- **Service keys** (HA, Jarvis, the bench, another router) are D140 API keys of service users
  with a narrow role.
- **Permissions:**

  | Permission | admin | user | service (switch) | anonymous LAN (transition) |
  |---|---|---|---|---|
  | `GET /v1/models` (filtered to routes they may use) | all | their routes | their routes | `access = "users"` routes |
  | chat endpoints | all routes | routes with `access = "users"` or listing them | listed routes | `access = "users"` routes, `anonymous_lan = true` |
  | on-demand switch | yes | only routes with `allow_user_switch` | if granted | never |
  | leases | all | own | own | via `:8099` shim |
  | `/v1/model/info` | yes | no (403) | no | no; the transition keeps it open on the LAN for layoutctl-era scripts until cutover + 30 days |
  | read config, state, jobs, plans | yes | no | no | `:8098` GETs only |
  | edit config, switch layouts, offline/online, holds | yes | no | switch only (if granted) | no |

- **Anonymous LAN** (`[access] anonymous_lan = ["lan-cidr"]`) keeps today's clients (Jarvis,
  the video pipelines) working without keys at cutover; each gets a key afterwards, and the
  option is removed later (open question 6).
- Upstream (engine and cloud) keys are never visible to any principal.

### 11.2 Where the pages live

In the **Abhoth web UI** (React client, admin section), not in a second web app (D-RC5): it
already has sign-in, 2FA, roles, CSRF and the audit table. The Abhoth server proxies
`/api/admin/router/*` to the router's `/admin/api/*`, adding the acting user's id in a signed
header, which the router writes into its own audit. The router's admin API also accepts
admin `abk_` keys directly (CLI, scripts).

### 11.3 Pages

| Page | Shows | Actions |
|---|---|---|
| **Overview** | per server: reachability, active layout, job running; per card: engine, model, state, in-flight/lanes, queue, leases, clocks, power, temperature, VRAM; guards with reasons; budgets near limit; config rev and errors | switch layout (via plan), hold, open job |
| **Cards** | configured and discovered GPUs by UUID, managed or not, offline/excluded with who/why, foreign holders | adopt a discovered GPU, offline/online, exclude from on-demand, hold clocks |
| **Engines** | engines, profiles, builds, templates (rendered preview for each card), hostctl allowlist status | edit, add profile, "export allowlist" |
| **Models** | catalog, artifacts per engine, file present / archived (size, restore estimate) | edit, add |
| **Layouts** | layouts and client profiles, the profile builder (per card engine → profile → model with a VRAM bar and the 4.8 check), summaries | create, edit, save as, delete, **plan & switch** |
| **Routes** | public names, targets, cold policy, fallbacks, adapters, access, which upstream would serve now | edit; "try" sends one chat as the admin, with `x-abhoth-no-switch` |
| **Upstreams** | cloud and router upstreams, health, limits, prices, key reference (never the value) | edit, test (a 1-token chat, admin-triggered only) |
| **Budgets & usage** | spend per user/key/upstream/period, charts, ledger | edit budgets |
| **Leases** | leases (incl. guard leases), freeze state | release (admin) |
| **Jobs** | running job with live steps and seconds; history with filters (requested by, state) | cancel before step 8, retry |
| **Config** | raw TOML editor with validation as you type, history with diffs, revert | save, revert |
| **Audit** | config writes, switches, admin actions, key use for admin endpoints | |

### 11.4 Edit flow and the plan view

1. The page loads the item and the file's `rev`.
2. The admin edits a form (or the raw TOML).
3. **Check:** the server applies the change to an in-memory `toml_edit` document, validates
   (4.8) and returns: errors by field; the TOML diff; the **consequences**: routes affected,
   and if the active layout's definition changed, the **plan** to apply it.
4. **Save** (with `If-Match: rev`): backup, atomic write, audit row, hot reload (4.9). Nothing
   restarts.
5. If there is drift, the Overview shows "active layout changed: apply" with the plan.

**Plan view** (for switches, applying drift, offline/online, on-demand previews): a card-by-card
before/after table; each step with its reason ("restart: coupling expert-arena with A");
preconditions as pass/fail rows; blocking guards and leases with who holds them; files to
restore with size and time; requests in flight that will drain; routes and slots before/after;
the estimate. Buttons: **Run**, **Run when unblocked** (queues until guards clear, with an
expiry), **Cancel**. The same JSON is what `abhoth sothoth plan` prints.

**Live state:** one SSE stream `/admin/api/events` (card telemetry every 2 s, state
transitions, job steps, guard changes, config reloads), so pages update without polling.

## 12. Observability

### 12.1 Metrics (`GET /metrics`, Prometheus text)

Built today: requests, errors, latency (TTFT, total) per route/card; in-flight and queued per
card; leases; affinity; on-demand decisions. Added:

- `abhoth_engine_state{server,card,engine,state}` (1 for the current state),
  `abhoth_engine_transitions_total{…,from,to}`, `abhoth_engine_health_failures_total`;
- `abhoth_switch_jobs_total{server,state,requested_by_kind}`,
  `abhoth_switch_duration_seconds` (histogram), `abhoth_switch_estimate_error_seconds`;
- `abhoth_guard_active{guard}`, `abhoth_guard_stale{guard}`;
- `abhoth_gpu_*{server,card,uuid}`: memory used/total, utilisation, power, temperature (GPU and
  memory), SM clock and max, clock-event reasons as a bitmask, telemetry age;
- `abhoth_ssh_up{server}`, `abhoth_ssh_command_seconds{template}`,
  `abhoth_ssh_command_failures_total{template,reason}`;
- `abhoth_upstream_requests_total{upstream,status}`, `abhoth_upstream_tokens_total{upstream,kind}`,
  `abhoth_spend_usd_total{upstream,principal_kind}`, `abhoth_budget_remaining_ratio{scope}`;
- `abhoth_adapter_translations_total{stage}`, `abhoth_tool_parse_failures_total{parser}`;
- `abhoth_config_rev_info{rev}`, `abhoth_config_reload_errors_total`.

### 12.2 Logs

- Request log (JSONL, built) gains: principal (user id or key id, never the key), route, cold
  policy outcome, fallback used, adapters applied, cost.
- Job log: every step with timing and captured output, in `router.db` and as JSONL in
  `state_dir/jobs/`.
- Structured `tracing` JSON on stdout; promtail already ships container logs to Loki.

### 12.3 The dash ("Glance")

The deepthought-dash board (`:8077`) today probes LiteLLM, qfn-lb, pig-slots and the profiles
API separately. With the router:

- one probe of `GET /status` (extended, read-only, no secrets): per server, card (engine,
  model, state, lanes, in-flight, queue, leases, clocks, temps), guards, active job (step,
  progress), last switch, on-demand pending, config rev and error, upstream health, spend
  today;
- one "router" row replaces the three gateway rows; the cards panel reads engine state from
  the router instead of probing engines;
- the offline/online buttons open the admin UI's card page (the router refuses anonymous
  switches); see open question 6;
- Grafana gets a router dashboard from the metrics above (switch history, GPU clocks vs
  expectation, spend).

### 12.4 Alerts

Sent to the existing Home Assistant notification path (HA webhook URL in config, a secret):
engine Failed; switch failed or rolled back; reconcile gave up (3 tries); clock mismatch;
guard source stale more than 5 min; SSH down more than 2 min; budget at 80% and 100%;
config reload failing; hostctl allowlist out of date.

## 13. Migration

### 13.1 The importer

`abhoth sothoth import --from-deepthought <dir> --out router.toml [--report import.md]`, where
`<dir>` holds read-only copies of: `profiles.json`, `spool/user/{saved,composed}.json`,
`gateway.json`, LiteLLM `config.yaml`, qfn-lb `nginx.conf`, `state/live.json`,
`state/offline.json`, pig-slots `data/layout.json`, `history.jsonl` (for estimates), plus
`layoutctl.py` constants given as a **preset** (`--preset deepthought-2026-11`) because the
Python file is code, not data.

| Source | Becomes |
|---|---|
| `profiles.json` `engines` | `[engines.*]` (ports, model id, builds, client `extraBody` → layout `client` defaults) |
| `engineProfiles` | `[engines.*.profiles.*]` |
| `models` + artifacts | `[models.*]` |
| `profiles` with cards | `[layouts.*]` (mode, client tuning, title, description) |
| bridge profiles | `[client_profiles.*]` |
| `aliases`, `layoutAliases` | `aliases = [...]` on the layout |
| user saved/composed | `[layouts.*]` with `source = "user"` / `"composed"` |
| `server.estimates`, history | `start_estimate_seconds`; learned values seeded into `router.db` |
| layoutctl `CARDS` (preset) | `[cards.A/B]` (UUID, CPU set, label), container names and ports |
| `NINFER_DEFAULTS/FIXED`, `LLAMA_DEFAULTS/FIXED`, images, model dirs, `HEALTH_OK`, `ENGINE_CTX` (preset) | engine `defaults`, `fixed`, `run`, `health`, `max_context` |
| Strata coupling, NUMA rule, page-cache drop (preset) | `share`, `restart_if_peer_starts`, `before_start` |
| `QUIET`, `MUSICMODE`, `DENSE_LEAD`, video and bench detection (preset) | `[guards.*]` |
| `gateway.json` `aliases`, `pinned` | `pool` and `card` routes |
| `modelAliases`, catalog names in LiteLLM `config.yaml` | `model` routes |
| `onDemand` (`enabled`, picks, names, exclude, timings) | per-route `cold` policy (`on_demand` only if `enabled` was true; today it is false, so `reject`/`fallback`), `[on_demand]` |
| `litellmParams`, `engineParams`, `effortLevels`, `modelEffortLevels`, `thinkingOffEffort`, `modelThinkingOffEffort` | engine/artifact `params` |
| LiteLLM `router_settings` | `[router.limits]` (cooldown, timeout) |
| qfn-lb weights | lanes (already in profiles; cross-checked) |
| `live.json`, `offline.json` | `router.db` seed: active layout, offline cards |
| ROUTER.md `router.json`/`base.json` (`placement`, `slots`, `observe`) | same sections |

The report lists every source key with where it went, everything dropped (with why), and
every value that needed a guess. **Keys are never copied**: secrets become references the
admin fills in.

**Render parity test** (the importer's acceptance test): a Python harness imports
`layoutctl.py` with `subprocess.run` stubbed, calls `start_ninfer`/`start_llamacpp`/`start_strata`
for every (layout, card), and records the docker argv (key masked). The router renders the
same from the imported config. They must match exactly, except `--api-key` → `--api-key-file`
and the secret mount. The same harness compares `restart_set`, `estimate`, `slot_names`,
`render_litellm`'s names and `decide()` outcomes on a table of inputs.

### 13.2 Dry run and the control-plane shadow

1. **Plans only:** `abhoth sothoth plan <layout>` against the live host (read-only hostctl
   templates: status, telemetry). For every layout the plan must match what layoutctl's
   `plan` says (restart set, estimate within 10%).
2. **Observe mode** (`control.mode = "observe"`, minutes, part of the cutover session; no
   multi-day trial): the router adopts what runs, reports drift, and its plans for a couple of
   real layouts are compared with layoutctl's. Nothing is changed. The same tooling stays
   available afterwards for comparing decisions if something looks off.
3. **Rehearsal on the router alone** (the first minutes of the cutover session itself, quiet,
   admin present, layoutctl's cron runner paused): the router runs a real B-only switch to another layout and
   back, then a both-cards switch and back, then a forced failure (a profile pointing at a
   missing build, which must refuse) and a failure after stop (a bad option, which must roll
   back). Then layoutctl's runner is re-enabled and must find the cards in the layout
   `live.json` says (the legacy mirror, 13.5).

### 13.3 Cutover runbook

**One cutover, as soon as the control plane is wired and tested** (no multi-day trial periods:
a user decision). Preconditions: all tests green (14); plans for every layout match
layoutctl's (13.2 step 1); the dash and Abhoth's slot client tested against the shims; the
allowlist installed on the host; keys for HA/Jarvis issued (keys are required from day one,
switching included); a quiet slot chosen (not 06:45-07:30, no bench, no video run). The
observe-mode check and the rehearsal (13.2 steps 2-3) run at the start of the same session.
Every step is reversible on its own, and rollback (13.4) takes minutes.

| # | Step | Check |
|---|---|---|
| 0 | Announce; freeze profile edits in the old UI | |
| 1 | Back up `$AI_ROOT/layouts`, the LiteLLM config dir, qfn-lb conf, pig-slots data, the dash; record the TrueNAS cron and init-script states | backups listed |
| 2 | Disable the TrueNAS cron job "inference layouts runner"; set `/run/qfn-watchdog.off`-style switch for the watchdog's LAYOUTS/GATEWAY hooks (env flag in the script; its CRASHED-apps job stays); disable the `boot-reconcile.sh` init script | `crontab`/`midclt` show them off |
| 3 | Router: `control.enabled = true`, `mode = "act"`, legacy mirror on; restart the router container | it **adopts** both ninfer instances (no restart; check container start times unchanged) |
| 4 | Stop pig-slots; router `:8099` shim on | Abhoth slot client claims/renews; bench `--wait-registry` sees slots |
| 5 | Stop the profiles API app; router `:8098` shim on | `GET /profiles`, `/live`, `/cards` from the web UI's picker; plan for the live layout says "already live" |
| 6 | Stop qfn-lb; router `:8092` shim on | `chk.sh` `/health`; `/a/slots` |
| 7 | Stop LiteLLM; router moves from `:8020` to `:8010` (compose change, ROUTER.md step 3) | the 19 client shapes from `replay.py` against `:8010`; Jarvis and the dash rows green |
| 8 | Smoke: `post_check`; a no-op switch to the live layout ("already live"); offline B → online B via the admin UI | job states `done` |
| 9 | On-demand on (a user decision: enabled at cutover, with the guards of 7.4): one real chat to a cold `on_demand` route with a switch key switches the idle card and serves; `/health`, `/v1/models` and `/health/liveliness` probes switch nothing | job `done`; probe check |
| 10 | Done. Normal use from here; roll back (13.4) on any regression. The legacy mirror and the stopped apps stay as rollback material for 30 days, then are removed; anonymous LAN chat ends per client as keys roll out | dash, Grafana |

### 13.4 Rollback

At any step, in reverse order of the steps done:

1. Router: `control.enabled = false` (data plane only) and back to `:8020`, or stop it.
2. Start LiteLLM, qfn-lb, pig-slots and the profiles API apps (their configs are untouched and
   were last rendered for the layout that is still running, because the router's legacy mirror
   kept `live.json` current and layoutctl's `gateway` re-renders from it).
3. Re-enable the runner cron job, the watchdog hooks and the init script.
4. Run `sudo python3 layoutctl.py reconcile` once: it must report "cards already run
   <layout>" (containers have the same names and `layouts.spec` labels) and re-render the
   gateway.

Rollback is rehearsed in 13.2 step 3 (layoutctl taking over again after router switches) and
once more right after cutover step 3 (back and forth before step 4).

### 13.5 The legacy state mirror

For 30 days after cutover (`control.legacy_state_mirror.until`) the router writes, after every
job and offline/online change, the files the old tooling reads: `state/live.json`,
`state/want-services`, `state/offline.json`, `state/history.jsonl` lines, job files, and
pig-slots `data/layout.json` and `leases.json`. Engine containers keep layoutctl's names and
labels. So a rollback finds a consistent world without a switch.

## 14. Test strategy

### 14.1 Fakes

- **Fake executor:** records every request (template id, variables, rendered text) and
  answers from a script: exit codes, output, delays, hangs. Plans and jobs are tested as
  sequences of executor calls against golden step lists.
- **Fake host:** a model of a GPU host (cards with memory, containers with labels, processes,
  files, clocks) behind the fake executor, so docker/compose/systemd drivers and builtins run
  for real against it; `nvidia-smi` output generated from the model.
- **Fake engines:** `orca-testkit` fake OpenAI servers with controllable startup time, health
  body, readiness flapping, crash on Nth request, 429s, slow first byte, mid-stream cut.
- **Fake cloud:** Anthropic/OpenAI/OpenRouter fakes replaying the golden corpus, with usage
  and rate-limit headers.
- **Real SSH:** an integration test with a containerised sshd, the real `hostctl serve`
  forced command and a strict allowlist: allowed templates run, everything else (other
  templates, bad variable values, shell metacharacters, oversized output, timeouts) is
  refused and logged.

### 14.2 Unit and table tests

- Config: every validation rule with a failing and a passing example; `toml_edit` round-trips
  keep comments; reload classes (4.9).
- Templates: quoting (spaces, quotes, `$`, backticks, newlines in values), list expansion,
  `{{ }}`, unknown variables, option rendering = layoutctl `ninfer_cli`/`llama_cli`.
- **Plans:** `restart_set` parity for every (from, to) pair of the imported layouts (15×15),
  with offline overlays; estimates; VRAM checks.
- `decide()`: the 16 ported tests plus the per-route policy, rate limits and permission rules.
- **Probe safety:** the route walk of 7.4.
- Guards: stale rules, DST edges of the quiet window (last Sunday of March/October),
  process matching with lock waiters.
- Budgets: reservation and settlement, streamed and cut streams, period edges, concurrency
  (two requests racing the last dollar).
- Adapters: the golden corpus (10.6); property tests that chat → canonical → chat is
  identity on supported fields; tool-parser fuzzing with markers split at every byte.

### 14.3 Chaos

Run against the fake host and fake engines, each asserting the invariants below:

- kill the router at every step of a switch (1-12), restart, check recovery (5.7);
- SSH drops during stop, during start, during a restore; hostctl hangs past its timeout;
- engine crashes in Starting, Warming and Ready; health flapping; a container that will not
  die (zombie); GPU memory never freeing;
- a foreign process appearing on a card mid-switch;
- guard sources going stale mid-decision; clock jumps; config edits every 100 ms during a job;
- disk full for backups and the job log (the switch must still finish or refuse cleanly);
- the web server (introspection) down; the database locked.

**Invariants:** never two engines on one card; a refused job changed nothing; a failed job
leaves no card routable that is not Ready; no switch while a blocking guard was active at step
2; no restart of an adopted engine on router restart; no secret value in any log, job step,
audit row or response; leases never granted on a card that is draining or frozen.

### 14.4 Soak and benches

- A short soak in CI (minutes, time-compressed) against fake engines with traffic shapes
  from the request log.
- Data-plane overhead must stay as measured in ROUTER.md 11.1 (a few ms); the bench rule (3
  runs, merge if no slower) applies before cutover, with the router in front of the bench's
  slot traffic.

## 15. Build plan

Agent-day estimates for code plus tests, in the order they can be built. Everything sits behind
`control.enabled` (default false), so main stays deployable: with it off the router is
today's data plane.

| Phase | Content | Agent-days |
|---|---|---|
| C1 | Config schema (TOML), loader with includes, validation (4.8), `toml_edit` writer, backups, audit table, `rev`, reload classes; `router.json` compatibility | 3 |
| C2 | Importer + DeepThought preset + import report; render-parity harness against `layoutctl.py` | 2.5 |
| C3 | Executors: local, SSH (`russh`), `hostctl serve/exec/install/export` with the allowlist and builtins; telemetry stream; discovery | 3.5 |
| C4 | Drivers (docker, compose, systemd, process, template), state machine, health/verify, adoption, reconcile loop and boot readiness | 3.5 |
| C5 | Plans and the switch orchestrator: couplings, prepare/restore, slots withdraw/freeze, lease check, drain, locks, rollback, journal and crash resume, jobs/history/estimates, legacy mirror, clock verify and gpu-power hold | 4 |
| C6 | Guards (all kinds), cold-model policy per route, on-demand on top of the built `decide()`, permissions and rate limits, the probe-safety test | 2 |
| C7 | Leases in `router.db`, guard leases, native lease API, `:8098` profiles shim (summaries, facts, compose, save) | 2 |
| C8 | Accounts: introspection client and cache, permissions, `/v1/models` filtering, admin API, SSE events | 2 |
| C9 | Admin web UI pages (11.3) in the Abhoth React client and its proxy | 4.5 |
| C10 | Upstreams: OpenAI-compatible, Anthropic, router chaining, cloud keys, passive health, budgets ledger and enforcement, usage views | 3 |
| C11 | Adapters: canonical model, Responses in (with stored responses), chat ⇄ Anthropic both ways incl. streams, tool normalisation parsers, reasoning/params generalisation, recorder and golden corpus | 6 |
| C12 | Metrics, `/status` v2, alerts, dash changes, Grafana dashboard | 1.5 |
| C13 | Chaos suite, soak, SSH integration test in CI | 2 |
| C14 | Observe-mode check tooling, rehearsal, cutover and rollback scripts, docs | 1.5 |
| **Total** | | **~41** |

Calendar on top: one cutover session (observe check, rehearsal and cutover together, a short
smoke check of minutes; no multi-day trial). Parallel tracks: C9 (UI) and C11 (adapters) and C10 (upstreams) are independent
of C3-C5 once C1 lands; with three agents the critical path (C1 → C3 → C4 → C5 → C6/C7 →
C13/C14) is about 20 agent-days.

What the cutover needs (U3, replace in one go): C1-C9, C12-C14 (~32). C10 and C11 add cloud
and translation and can land before or just after; nothing in the cutover depends on them.

## 16. Open questions

| # | Question | Recommendation |
|---|---|---|
| 1 | TOML or YAML for `router.toml`? | **TOML** (`toml_edit` keeps comments and order on writes; no indentation traps) |
| 2 | How does the router check `abk_` keys: introspection to the web server, or reading `abhoth.db` read-only? | **Introspection with a 60 s cache** (no schema coupling; revocation in a minute); the read-only DB as a fallback only if the web server must not be a dependency |
| 3 | Where do the admin pages live? | **In the Abhoth web UI** (accounts, 2FA, CSRF and audit exist there); the router keeps an API only |
| 4 | Local execution on DeepThought: SSH to the host's loopback from the container, or run the router on the host as a service? | **SSH loopback with the forced command**: one security model, no docker socket, no root in the container |
| 5 | Engine keys in argv: add `--api-key-file` (or an env var) to ninfer, and check Strata? | **Yes**, add it to ninfer (we own it); keep `--api-key` only for Strata if it has nothing else, via a wrapper reading the file inside the container |
| 6 | Anonymous LAN clients (Jarvis, video pipelines, the dash buttons, HA's switch buttons) after cutover? | Keep **anonymous LAN chat** for 30 days and issue keys meanwhile; **switching needs a key from day one** (HA and Jarvis get service keys with `switch`; dash buttons link to the admin UI) |
| 7 | Should the router own GPU clocks (absorb clockctl/gpu-power)? | **No, not now.** Verify after starts (A at 1597 unlocked, B app clocks), hold gpu-power during switches; revisit when a second host exists |
| 8 | Responses API `previous_response_id`: store responses (24 h, per principal) or refuse it? | **Store for 24 h** (clients assume it works); `store: false` keeps nothing |
| 9 | Which text tool-call formats do our local models really emit? | **Record first** (C11 recorder against each local model with tools), then implement only those parsers plus `hermes-json` |
| 10 | On-demand for users: should non-admin users ever start a switch? | **No by default**; per route `allow_user_switch` for models the household uses on purpose |
| 11 | Should on-demand be re-enabled at cutover? | **Yes** (user decision). Enabled at cutover with strong guards: never during a bench, a video pipeline (incl. `run_daily.sh` waiting on its lock) or an active lease; guard data older than 45 s means wait; excluded cards never switch; rate limits; only real chat requests on an `on_demand` route with a key that may switch, never health checks or model listings (7.4) |
| 12 | Cloud access for users | **Admin-only until budgets are set**; then per-user monthly budgets (start at $10) on `*cloud` |
| 13 | Guard probes vs guard leases for the bench and video pipelines | Keep **both**: probes for parity at cutover; move the bench to a guard lease (it is ours) soon after; leave the video pipeline on the probe |
| 14 | The qfn-llm TrueNAS app also defines `lb` and `pig-slots` services: remove them or leave them stopped? | **Leave stopped for 30 days** (rollback), then remove from the app; Strata services stay in that app (compose driver) |
| 15 | `/v1/model/info` is admin-only in the new model, but layoutctl-era scripts and the dash read it | Open on the LAN until legacy mirror end (30 days), then admin-only |
| 16 | Allowlist updates need root on the host for every template change: acceptable friction? | **Yes** for production; `trusted` mode exists for experiments and is flagged in the UI |
| 17 | Thinking round-trip store for chat clients on Claude routes (1 h, 64 per principal) | Accept; Abhoth keeps `reasoning_details` itself, so the store is only for other clients |

## 17. Decisions in this document

| # | Decision | Why |
|---|---|---|
| D-RC1 | TOML, edited with `toml_edit`; includes allowed | comments and order survive UI edits |
| D-RC2 | Config says what can exist; `router.db` holds what was chosen and what happened | edits never fight with runtime state; the file stays diffable |
| D-RC3 | Reloads never restart engines; definition changes show as drift with a plan | restarts drop requests; only an admin action may cause one |
| D-RC4 | Execution over a forced-command SSH key with a root-owned allowlist, also for the local host | least privilege, one model; a compromised router can't run new commands |
| D-RC5 | Admin UI in the Abhoth web UI; the router exposes an API | reuse D140 sign-in, 2FA, CSRF, audit |
| D-RC6 | Keys checked by introspection with a cache | no shared database file between containers |
| D-RC7 | Switches can only be built from authenticated chat admissions; `x-abhoth-no-switch` on every outgoing request; routes default to `reject` | closes the health-check switch class structurally |
| D-RC8 | Same container names and labels as layoutctl; legacy state mirror for 30 days | rollback without a switch |
| D-RC9 | Engine keys only as files | keys out of `ps` and `docker inspect` |
| D-RC10 | Canonical form for translation, passthrough when APIs match | no cost for today's traffic, one place per mapping |
| D-RC11 | Budgets reserve the worst case before sending and settle after | can't overspend with concurrent requests |
| D-RC12 | A router restart adopts running engines; never restarts them | crash recovery must not cause outages |
