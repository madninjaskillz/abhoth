# `abhoth router`: a gateway to replace LiteLLM on DeepThought (design)

Status (2026-11-25): **phases 1-3 built** in `crates/orca-router` (`abhoth router`), tested
against fake engines and a local LiteLLM 1.106.6 shadow replay; see section 10 for the
implementation decisions and section 11 for the replay results. Nothing on DeepThought uses it
yet. Survey date: 2026-11-25, read-only, against the live layout `merged-inkling-x2` (ninfer
`inkling-2-mini` on both cards, 4 lanes each). Keys and tokens are left out of this document on
purpose. Host-specific values are placeholders: `gpu-host` (the GPU server), `$AI_ROOT` (its
AI tree), `$LITELLM_CONFIG_DIR`, `$TRUENAS_APP_CONFIGS`, `$HA_BRIDGE_DIR`.

## TL;DR

- **What LiteLLM actually does for us is small.** It handles `POST /v1/chat/completions` only.
  It has no auth, no DB, no UI, no spend tracking and no cloud providers, and it serves no
  embeddings, images, audio or `/v1/messages`. It routes with `simple-shuffle` and 2 retries,
  plus about 200 lines of our own Python: the effort clamp and a hot-reload hack. Behind it,
  nginx `qfn-lb` does the real balancing, and the profiles API does on-demand switching.
- **Recommendation: replace it, in phases.** Ship a parity gateway first, which absorbs both
  LiteLLM and qfn-lb. Server-side slots and session affinity follow, then built-in on-demand
  routing. The alternative, keeping LiteLLM and only absorbing slots, is worse: placement has
  to sit in the request path, and in LiteLLM that means more Python plugin code on top of the
  hacks we already carry.
- **Size:** about 3.3k lines of Rust code plus about 2.5k of tests, in a new crate
  `orca-router`. That is about 8-9 agent-days across three phases (MUST alone: about 4). No
  multi-week shadow: tests, a short smoke check, then switch with a ready rollback.
- **Top risks:**
  - wire details that clients rely on without knowing it (the `/v1/models` shape that makes
    OpenOrca treat us as a gateway, error shapes, the `x-litellm-model-name` header);
  - ninfer's strict validation, which fails unknown fields with 400;
  - layoutctl's coupling to LiteLLM (its config generator, hot reload, `gateway --hold`);
  - one more home-grown service to maintain.

---

## 1. Survey: what we depend on

### 1.1 LiteLLM itself

How it runs: as the TrueNAS app `litellm`, from the rendered compose at
`$TRUENAS_APP_CONFIGS/litellm/versions/1.0.0/templates/rendered/docker-compose.yaml`.

- **Image:** `ghcr.io/berriai/litellm:main-stable`. This is a floating tag, so a redeploy can
  pull new code.
- **Ports and mounts:** `8010:4000`, on its own bridge `ix-litellm_default` (172.16.3.0/24).
  Two read-only bind mounts: `$LITELLM_CONFIG_DIR/config.yaml` to `/app/config.yaml`, and
  `effort_hook.py` to `/app/effort_hook.py`.
- **Environment:** no DB or master-key variables.

`config.yaml` is **generated** by `layoutctl.py gateway` from `$AI_ROOT/layouts/gateway.json`
and the live layout. It is rewritten on every switch; the previous version is kept as `config.yaml.prev`.

| Feature | In use? | Detail |
|---|---|---|
| Model list and aliases | **yes** | 15 deployments, 14 names (listed below) |
| Router | **yes** | `simple-shuffle`, `num_retries: 2`, `allowed_fails: 1`, `cooldown_time: 10`, `timeout: 3600`; `weight: 4` on the two `inkling-2-mini` deployments |
| Fallbacks | no | none configured ("No fallback was attempted" in the logs) |
| Param handling | **yes** | `drop_params: true`; `additional_drop_params: [response_format]` on every ninfer deployment; `reasoning_effort: low` injected on the Strata deployment (`flash-next`) |
| Callbacks | **yes** | `effort_hook.handler`. It (a) clamps `reasoning_effort` to `model_info.effort_levels`; (b) handles thinking-off (`enable_thinking: false` at top level or in `chat_template_kwargs`) with `thinking_off_effort`: `drop` (ninfer) or a level (`none` for Inkling); (c) hot-reloads the model list (a 2 s watcher plus a check before each request, using `Router.set_model_list`) so a switch does not redeploy |
| Auth | **no** | `dangerously_permit_weak_or_unset_master_key: true`, no master key and no virtual keys. Clients send whatever key they have, or none |
| DB / spend logging / UI | no | no `DATABASE_URL`. The UI portal exists but is unusable without a DB or key |
| `/v1/messages`, embeddings, images, audio | no | no hits in the logs |
| Caching, rate limits | no | none configured |
| Telemetry | off | `telemetry: false` |

Model names served (`GET /v1/models`, all `owned_by: "openai"`):

- **Generic aliases:** `qwen3.8-flash-next`, `qwen3.8-27b-coder`, `qwen3.6-27b-coder`,
  `qwen3.6-35b-a3b-awq`, `jarvis-brain`, `qwen3.8-27b`. These are balanced over the whole live
  pool via `qfn-lb /ninfer/`, whatever model is live.
- **Pinned per card:** `qwen3.8-flash-next-a` goes to `qfn-lb /a/`, `qwen3.8-flash-next-b` to `/b/`.
- **Live catalog model:** `inkling-2-mini`, two deployments (`/a/` and `/b/`).
- **On-demand catalog models** (relayed to `:8098/ondemand/v1` while not live): `flash-next`,
  `dense-pi`, `dense-stock`, `qwenpi`, `dense-pi-abl-v3fork`.

`model_info` is extended with our own keys, which layoutctl and the dash read:
`engine`, `effort_levels`, `thinking_off_effort`, `live`, `ondemand`, `catalog_model`, `title`,
`cards`, `switch_to {engine, engineProfile}`, `gateway_rev`.

**Traffic.** The container started 2026-11-24 16:16Z and the logs run to the survey time
(about 22 h). Liveliness probes are counted separately.

| Endpoint | Count | Who |
|---|---|---|
| `GET /health/liveliness` | 4028 | the dash, every 20 s (via 127.0.0.1, so it shows as 172.16.3.1) |
| `POST /v1/chat/completions` | 382 (372 OK, 4×429, 4×404, 2×503) | 187 from .52 (host: video pipelines, OpenOrca web), 91 from .254 (HeyEmDee: OpenOrca/Abhoth CLI, bench), 82 from .70 (Home Assistant / Jarvis), 22 from localhost (`layoutctl gateway_test`) |
| `GET /v1/models` | 49 | OpenOrca (ServerIdentity, model discovery), Jarvis config flow |
| `GET /v1/model/info` | 19 | layoutctl (`gateway_rev` confirmation after hot reload) |
| `GET /api/v0/models`, `POST /api/show` | 26 + 26, all 404 | OpenOrca's LM Studio / Ollama context-window probes. A 404 is the right answer |

Errors seen:

- `429` passed through from ninfer (`inference request queue is full`);
- `404 model 'qwen3.8-27b' not found`, from an engine that did not accept the alias as a model
  id at the time;
- `503` from the on-demand relay while `gateway --hold` was set.

Retries made the 404s cost three attempts each, which is pure waste.

**Hazard: LiteLLM's `GET /health` switches GPUs.** It sends a test chat to every deployment,
including the on-demand relay ones (`flash-next`, `dense-stock`, `qwenpi`,
`dense-pi-abl-v3fork`), and the relay treats each as a real request. On 2026-11-25 15:12 UTC
one `GET :8010/health` (made while capturing shapes for this work) started job
`20261125T151234Z-combo-7a3bf854`, switching card B to `dense-stock`. On-demand switching was
then turned off in `gateway.json` (`onDemand.enabled=false`) and stays off until the router
replaces LiteLLM. Rules that follow:

- Never call LiteLLM's `/health` (the dash and layoutctl only use `/health/liveliness`).
- The router's `/health` makes no model calls at all, and its on-demand decision ignores
  nothing: a request for a not-live name is a switch request, so only clients that mean it may
  send one. Shadow testing never names a not-live model.

### 1.2 Clients

| Client | Endpoint | Models | Features used | Auth |
|---|---|---|---|---|
| **Video pipelines** (`$AI_ROOT/videoPipeline`: `pipeline/research_local.py`, used for research, ranking, script writing and the claims check via `rl.LLM`; AI News, Game News, Car News) | `http://gpu-host:8010/v1` (`config.json research_llm_url`) | `qwen3.8-flash-next-a` (pinned card A) | Non-streaming; `temperature`, `max_tokens`, `response_format: json_object` (dropped for ninfer; drops itself on a 400); retry with `chat_template_kwargs.enable_thinking: false`; strips `<think>`. Reads `usage`, `finish_reason` | none |
| **Jarvis** (HA custom component `jarvis`, `llm.py`) | config entry base URL. Default is `:8012`; traffic shows it on `:8010` from .70 | `jarvis-brain`, plus an optional `image_model` | Non-streaming and **streaming** SSE; `temperature`, `max_tokens` (400); `response_format: json_object`; **always** `chat_template_kwargs.enable_thinking: false`; `image_url` content for vision turns. Config flow calls `GET /v1/models`. Its own tool loop, no OpenAI `tools` | none |
| **OpenOrca / Abhoth** (C# and Rust; on HeyEmDee and in the `openorca-web` container) | `lmStudio.baseUrl` `:8010/v1`, model `qwen3.8-27b`; provider `lmstudio`, `qwen3.8-27b-coder` | aliases above, or any catalog model the user picks | Streaming SSE; native `tools`; `reasoning_effort`; `chat_template_kwargs` (`preserve_thinking`, `enable_thinking`); extra body and session field (`uncensored`, sent only where `UncensoredGate` allows); images. Reads `x-litellm-model-name` / `x-served-model` (`ServedModels`); `GET /v1/models` (`ServerIdentity`: a list with more than one model or `owned_by: openai` means "gateway", whose `/health` is never asked); probes `/api/v0/models`, `/api/show` | an `apiKey` is configured and sent as a Bearer token; LiteLLM ignores it |
| **OpenOrca slots** (same apps, `slots.enabled`) | **direct** to ninfer `:8094/v1` and `:8096/v1`, **not via :8010**. Leases from pig-slots `:8099` | `slots.model` `qwen3.8-27b` (ninfer accepts any name) | `extraBody {reasoning_effort: low, chat_template_kwargs.preserve_thinking: true}`; Lead pinned to copy A (`leadCopy`, `pinLeadPrefix`); `agentsPerCopy 2`; lazy Lead lease; idle release after 15 min | the ninfer key (`slots.apiKeyEnv`) |
| **Bench kit** (`bench/` in `openorca-rust`) | `run.py` drives OpenOrca (above) and waits on the registry (`--wait-registry http://…:8099`); `pigrun.py` / `pig_vs_orca.py` use `:8099` and `:8010` | `pigrun.py` still names **`qwen-flash`**, an alias that no longer exists (stale) | `reasoning_effort`, `chat_template_kwargs` through PiG's `samplingParams` | none |
| **Dash** (`$AI_ROOT/dash/app/server.py`) | `:8010/health/liveliness`, `:8092/health`, `:8099/health` and `/list`, `:8098/live` and `/cards`; engines directly for `/v1/models`, `/props`, `/metrics` and ninfer request logs | n/a | GETs only | engine key for direct probes |
| **layoutctl.py** (root cron runner) | writes `config.yaml` and nginx.conf; `GET /health/liveliness`, `GET /v1/model/info` (`gateway_rev`); `gateway_test` POSTs one chat to `:8010` with the first alias | first alias | non-streaming, `max_tokens` 64 | none |
| **HA ha-bridge** (`$HA_BRIDGE_DIR/ha_bridge.py`), **Jarvis** `deepthought.py` | `:8098` profiles API only (switch buttons, live view) | n/a | n/a | none (LAN) |
| **music** `chk.sh` | `:8092/health` | n/a | n/a | n/a |
| **ComfyUI / image-api shim / SA3 / ha-bridge GPU** | none of 8010, 8092, 8098 or 8099 | | | |
| **Cloud providers** | none in any LiteLLM config | | | |

`llm-rr` (nginx on `:8011`, with a conversation-key JS hash) and `jarvis-brain` (llama.cpp
`:8080`) are TrueNAS apps that are configured but **not running**. Neither is a dependency.

### 1.3 Our glue services

- **qfn-lb** (`nginx:alpine`, service `lb` of the TrueNAS app `qfn-llm`, `:8092`).
  - Its config is generated by `layoutctl.py gateway` into
    `$AI_ROOT/qwen-flash-next/twocopy/lb/nginx.conf`.
  - Routes:
    - `/ninfer/` goes to a `least_conn` pool over `:8094` and `:8096`, `weight=4` (the slot count);
    - `/a/` and `/b/` go to one card each;
    - `/` goes to the main engine's pool.
  - It **injects the engine API key**, so callers need none.
  - Single worker, so least_conn counts are global. Timeouts are 3600 s, with no buffering.
  - Failover: `proxy_next_upstream error timeout http_502 http_503`, 2 tries,
    `max_fails=1 fail_timeout=10s`.
  - Its state is the generated file.
- **pig-slots** (`:8099`, `$AI_ROOT/pig-slots/server.py`, Python standard library,
  service of `qfn-llm`).
  - **Lease protocol:**
    - `POST /claim {key, owner, host, pid, label, kind: main|agent, copy?, avoid?, slot?}` returns
      `{ok, slot, copy, ttl}`, or `{ok:false, reason: full|switching}`. It is idempotent per key.
    - `POST /renew {keys}` returns `{renewed, lost}`.
    - `POST /release {key}` or `{host, pid}`.
    - `GET /list` returns the slots, free slots, load per copy, layout, frozen and orphans.
  - **TTL** is 90 s; unrenewed leases are reaped.
  - **Placement:** the least-loaded copy first, then A.
  - **"Lead pinning"** is simply the client asking with `copy: "A"` and `kind: main`. The
    registry has no priority. Lead/agent policy (Lead on A, agents per copy, lazy Lead lease,
    balanced placement) lives **in the client** (`orca-core::slots::SlotAllocator`, C# `Slots`).
  - **Slot list:** from `data/layout.json`, written by layoutctl (now `A1-A4, B1-B4`). It is
    re-read on change. `frozen: true` during a switch refuses claims.
  - **State:** `data/leases.json`.
  - **Not enforced:** a lease is advisory. Nothing stops traffic that holds no lease (Jarvis,
    the pipelines and LiteLLM users all go to the same lanes unleased).
- **On-demand switching** (`layouts/ondemand.py` + `profiles_api.py /ondemand/v1/chat/completions`).
  - Flow:
    - LiteLLM sends a not-live catalog model to the relay.
    - `decide()` is a pure, unit-tested function. Its checks: enabled; target known; already
      live; a switch pending; the quiet window 06:45-07:30 UK; a video pipeline running; the
      bench queue running; music mode; `gateway --hold`; a card offline; another job; stale
      guards; pig-slots leases on the card; the minimum stay (300 s); busy within the idle
      grace (10 s).
    - It picks the card idle longest (B on ties).
    - `Coordinator.ensure()` spools a switch; the root cron runner `layoutctl.py runner` runs
      it.
    - The relay then forwards to `qfn-lb /<card>/` with `model` rewritten to the served id.
  - Streaming clients get an early `delta.reasoning_content` note and SSE keepalives while they
    wait.
  - A refusal is a 503 JSON with `Retry-After`. Queue up to 300 s, switch timeout 900 s,
    failure hold 60 s.
- **Gateway hold** (`layoutctl.py gateway --hold REASON` / `--release` / `--check`).
  - `state/litellm-hold` stops LiteLLM from being redeployed; the pending config goes to
    `config.yaml.pending`.
  - qfn-lb still follows the layout.
  - The hold is not set now.
  - The reason the hold exists: **a LiteLLM redeploy drops requests in flight.** Hot reload
    (2026-11-24) mostly avoids redeploys now.
- **Profiles API** (`:8098`, `layouts/profiles_api.py`, TrueNAS app `inference-profiles`,
  read-only container, host network).
  - Endpoints: catalog and profiles (`/profiles` with ETag), `/live`, `/plan`, `/switch`
    (spooled), `/jobs`, `/cards` (online/offline), `/ondemand` (view) and the relay.
  - It never touches containers itself: the root cron runner carries out every change. Clients
    are OpenOrca's profile picker, the web UI, HA and Jarvis.

### 1.4 Engines behind the gateway

| Engine | Where | Needs per request | Cache / slot hint |
|---|---|---|---|
| **ninfer** `merged` build (`local/ninfer-build:cuda12.8.1`, v0.18.0) | `ninfer-a :8094` (card A), `ninfer-b :8096` (card B); model `inkling-2-mini`; `--max-concurrency 4`; `--vision`; `--preserve-thinking`; per-card request log JSONL | Bearer key (qfn-lb adds it). Accepts any `model` name (it answered the aliases), though the 404s above show at least one build did not. **Rejects unknown top-level fields** (`unknown_parameter`), `response_format` json modes (400), `reasoning_effort` together with `enable_thinking: false` (`conflicting_template_option`), efforts other than `none/low/medium/xhigh` (`reasoning_effort_not_supported`), the `uncensored` field (`speed_projection_not_supported`), and non-null `chat_template_kwargs` other than the known ones. Returns 429 when its queue is full | Prefix cache is **per process, shared across its 4 lanes**. It accepts `prompt_cache_key`, `prompt_cache_options` and breakpoints, but "`prompt_cache_key` is not an Engine session key or prefix identity" (`docs/serving.md`). **No lane or slot pinning.** So affinity means **card** affinity: send the same session to the same process |
| **Strata** (Flash, `qfn-strata` / `-b`, `:8080` / `:8091`) | not live; reached only on demand (`flash-next`, a switch that restarts both cards; `allowBothCards: false`, so refused today) | Key; `reasoning_effort` (central default `low`); accepts the `uncensored` field; `/health` says `service: strata` | Strata's own slots; leaked-pin fix live (memory). Hint support not re-checked here |
| **llama.cpp** (Inkling, QwenPi variants in some profiles) | not live | top-level `reasoning_effort`, `enable_thinking`, `json_object` accepted; ignores the model name | `id_slot` and `cache_prompt` exist in llama-server: real slot pinning when a profile runs it |
| **Cloud** | none | | |

---

## 2. Recommendation

**Replace LiteLLM. Absorb qfn-lb at the same time, then pig-slots and the relay's forwarding
step. Leave layoutctl and the profiles API as the control plane.**

Why replacing is right here:

1. **Parity is small and well defined** (section 3, MUST). We use one endpoint family and none of
   LiteLLM's heavy features. Everything specific to us is already our own code inside LiteLLM:
   the effort clamp, thinking-off handling and hot reload.
2. **Four hops collapse into one.** A request now goes client → LiteLLM → qfn-lb → ninfer, or
   client → LiteLLM → profiles relay → qfn-lb → ninfer. Each layer has its own retry and
   timeout rules. LiteLLM retries every failure twice, 4xx included (the logs show a
   404 tried three times), and nginx retries each of those again on errors; the router does one
   attempt plus one failover, and only on errors before headers.
3. **Placement must be in the request path.** Server-side slots, Lead priority and session
   affinity cannot be done well from outside the proxy. In LiteLLM they would be a custom
   routing strategy in Python, which is more of what we already find fragile: the hot-reload
   monkey-patch and `Router.set_model_list` on a floating `main-stable` image.
4. **Redeploys drop requests in flight.** That is why `gateway --hold` exists. Our router reloads
   its config atomically, and requests in flight keep the snapshot they started with.
5. **Same language and wire types as Abhoth.** It reuses `server_identity` (engine kinds),
   `served_models` (header names) and the slots types, and it is tested with `orca-testkit`'s
   fake servers.

What would argue for keeping LiteLLM: if we expected cloud providers, virtual keys or spend
tracking soon. Nothing in the survey suggests that. If it changes, LiteLLM can sit **behind** the
router as one more upstream, for cloud models only.

The halfway option, keeping LiteLLM and only absorbing slots, means writing the same proxy
anyway (to place requests) and still running LiteLLM in front of it. Rejected.

## 3. Feature list

### MUST (parity: no client changes)

- **`POST /v1/chat/completions`**, streaming (SSE passthrough, no buffering, flush per chunk)
  and non-streaming. Body size limit at least 64 MB (images). Timeout 3600 s. Detect client
  disconnects and cancel upstream.
- **Names:** the same 14 model names with the same semantics:
  - generic aliases go to the whole live pool;
  - `-a` / `-b` pin a card, falling back to the other card if theirs is offline (as
    `gateway.json` says);
  - catalog names go to the cards running them;
  - a catalog name not live goes to on-demand.
  - `inkling-2-mini` keeps a weight per card.
- **Model rewrite:** `model` is replaced by the served id of the card that was picked. The
  relay already does this; LiteLLM's `openai/<id>` does it implicitly.
- **Effort translation** exactly as `effort_hook.py`:
  - clamp to the deployment's `effort_levels`;
  - `thinking_off_effort` `drop` or a level, checked at top level, in `chat_template_kwargs`
    and in `extra_body.chat_template_kwargs`;
  - per-engine and per-served-id defaults (`reasoning_effort: low` for Strata, none for ninfer).
  - Port the hook's cases as table tests.
- **Param hygiene per engine:**
  - drop `response_format` for ninfer;
  - drop OpenAI params the engine rejects (the `drop_params` set we actually see).
  - **Verify in shadow** what LiteLLM does today with unknown top-level fields
    (`uncensored`, `metadata`, `user`) before choosing pass or drop per engine. Today
    `UncensoredGate` sends `uncensored` to a gateway and expects the 400 safety net.
- **Upstream auth:** add each engine's key from a file (`/run/secrets/...`), never from
  config text. Ignore or accept any client `Authorization` header.
- **Retries:** one failover to another eligible upstream on connect error, timeout before
  headers, 502 or 503, and **never after the first byte has been sent**. A 429 from ninfer
  passes through unchanged (clients back off). Cooldown: an upstream that fails is skipped for
  10 s, like `allowed_fails: 1, cooldown_time: 10`.
- **`GET /v1/models`:** the same shape: `{"object":"list","data":[{id, object:"model", created,
  owned_by:"openai", max_input_tokens, max_output_tokens?}]}`. **`owned_by: "openai"` and more
  than one entry are load-bearing**: OpenOrca's `ServerIdentity::is_gateway_list` uses them to
  classify the server as a gateway, and never calls its `/health` then.
- **`GET /v1/model/info`:** `{"data":[{model_name, litellm_params:{model, api_base}, model_info:{id,
  engine, effort_levels, thinking_off_effort, live, ondemand, catalog_model, title, cards,
  switch_to, gateway_rev, max_input_tokens, max_output_tokens}}]}`. Our keys only, not LiteLLM's
  ~150 cost fields (nothing reads them; checked: layoutctl reads `gateway_rev`, the dash reads
  nothing). Never include keys.
- **`GET /health/liveliness`** returns `"I'm alive!"` with 200 (dash, layoutctl).
  **`GET /health`** returns `{"status":"ok","service":"abhoth-router"}` and **makes no model
  calls** (LiteLLM's does).
- **Response headers:** `x-litellm-model-name` (the served id) plus `x-served-model`. Both are
  read by `ServedModels` (C# and Rust); LiteLLM sets the first in `common_request_processing.py`.
  Also `x-abhoth-card` and `x-abhoth-upstream` for debugging.
- **Error shape:** `{"error":{"message","type","param","code"}}` with the upstream status. For
  unknown models: 404 `model_not_found`, without retrying.
- **Unknown paths:** 404, including `/api/v0/models` and `/api/show`, which OpenOrca expects
  to fail.
- **Per-card routes** compatible with qfn-lb (`/ninfer/v1/...`, `/a/v1/...`, `/b/v1/...`), so the
  relay, `chk.sh` and the dash keep working while qfn-lb is retired. Keep `GET :8092/health`.
- **Config generated by layoutctl** (see 4.3), with atomic hot reload and `gateway_rev`
  reported in `/v1/model/info`.

### SHOULD

- **Server-side slots (the pig-slots API, served by the router).**
  - `/claim`, `/renew`, `/release` and `/list` with the same JSON, TTL, layout file and freeze.
  - Existing clients and the bench's `--wait-registry` keep working against the new address.
    A compatibility listener on `:8099` makes it a drop-in.
- **Lease-aware placement for gateway traffic.**
  - A request that carries a lease key (header `x-abhoth-slot-key`, or `prompt_cache_key` that
    matches a lease) goes to that lease's card.
  - Unleased traffic (Jarvis, the pipelines) is counted against free lanes: the router knows
    in-flight requests per card, which the advisory registry does not.
- **Lead priority.**
  - `kind: main` leases, or a header `x-abhoth-priority: lead`, skip the per-card admission
    queue.
  - Agents and unleased traffic queue when a card's in-flight count reaches its lane count,
    instead of hitting ninfer's own queue (429).
  - Default policy: keep one lane on the Lead's card free for the Lead while it holds a lease.
- **Session affinity.**
  - Key from `prompt_cache_key`, else `x-session-id` / `x-litellm-session-id`, else
    `metadata.session_id`.
  - The key maps to a card by rendezvous hashing over the eligible cards, with a sticky table
    (TTL 15 min) so a card that comes back does not reshuffle live sessions.
  - Overflow (the affine card is full and the other is free) moves the session once and
    records it.
  - `prompt_cache_key` is passed through to ninfer, which accepts it.
- **Built-in on-demand routing.** The router runs `decide()` itself (ported from `ondemand.py`,
  with its unit tests). It asks the profiles API / runner to switch via the existing spool API
  (`POST :8098/switch`-style, or a dedicated `POST /ondemand/ensure`). While waiting, it holds
  the request with the same SSE note and keepalives. After the switch it forwards directly.
  The profiles API relay becomes a fallback and is later removed.
- **Graceful switches.** layoutctl tells the router to *drain* a card (`POST /admin/cards/A/drain`):
  no new requests there, wait for in-flight requests (bounded), then layoutctl switches. This
  replaces `gateway --hold`, the 30 s confirmation dance and the pig-slots freeze, with one
  mechanism.
- **Metrics for the dash.**
  - `GET /metrics` (Prometheus text), scraped by the existing Prometheus. It reports:
    - requests, errors and latency (TTFT, total) per model name, per card and per client IP;
    - in-flight and queued per card;
    - leases, affinity hits and moves;
    - on-demand decisions.
  - `GET /status` (JSON) for the dash card, replacing the dash's separate `:8010`, `:8092` and
    `:8099` probes.
- **Request log.** One JSONL line per request: time, client, model asked, served id, card,
  status, TTFT, duration, tokens from `usage`, retry and affinity notes. No bodies. This
  replaces the LiteLLM logs we grepped for this survey.

### WON'T

- Cloud provider adapters, cost or spend tracking, budgets, virtual keys, teams, an admin UI or
  a DB. (If needed later: LiteLLM as one upstream behind the router.)
- `/v1/messages` (Anthropic), embeddings, images, audio and the Responses API. None are used.
  If wanted later, they are pure passthrough to ninfer, which implements Responses.
- Prompt or response caching in the router. ninfer's prefix cache is the cache.
- Router-side prefix trees or tokenisation (SMG-style). With two cards, session-key affinity
  gets the benefit without the cost. Revisit if the card count grows.
- Running layout switches itself (docker, GPUs). That stays with layoutctl's root runner; the
  router has no docker socket.
- Client authentication in phase 1 (LAN only, as today). It is designed in, though (4.4).

## 4. Architecture

### 4.1 Crate and binary

- **New crate `rust/crates/orca-router`** (library). It depends on `orca-core` only for small,
  pure pieces:
  - `client::server_identity`: `ServerKind`, `parse_models`, `parse_health`; the router uses
    them to probe upstreams and to label engines;
  - `client::served_models::HEADERS`;
  - the slots wire types from `slots::registry` (`SlotClaim`, `SlotClaimResult`) and
    `slots::http`, so the registry JSON is defined once for both client and server;
  - effort vocabulary from `client::request_parameters` (`ORDER`; `apply_effort` semantics).
  - If depending on the whole `orca-core` makes the image too big, move those modules into a
    small `orca-wire` crate first. That is a mechanical refactor; the port's cross-check
    covers it.
- **Binary:** `abhoth router --config router.json [--listen 0.0.0.0:8010] [--compat-listen
  :8092,:8099]`. It is a subcommand in `orca-cli` behind a Cargo feature `router` (on by
  default).
- **Image:** the Docker image builds `orca-cli` with `--no-default-features --features router`
  (no desktop, PTY or MCP). It is a static musl build on `gcr.io/distroless/static` or
  `scratch`, with an expected size around 15 MB. Tags are versioned and pinned in compose,
  never floating.
- **Stack:**
  - `axum` (already pinned at 0.8.9; needs the `http1` server feature, plus `http2` off) and
    `hyper` 1.13 server;
  - upstream client: `reqwest` 0.13 (already pinned) with `stream` for bodies, or a
    `hyper-util` legacy client with a keepalive pool per upstream (8, as nginx);
  - `tokio`.
  - New dependencies must be pinned like the rest (PORTING.md, "Dependencies").
    `prometheus-client` is optional; plain text formatting is fine.

### 4.2 Request path

```
accept → parse JSON body (keep it as serde_json::Value, preserve_order) → resolve name
  → candidate cards (live, online, not draining, model matches)
  → policy: lease/affinity → priority admission (per-card semaphore = lanes, a queue per priority)
  → rewrite (model = served id; effort clamp; thinking-off; drop params; add the upstream key)
  → send upstream; wait for the status line and headers
       connect/timeout/502/503 before headers → mark the upstream cooling, try the next candidate once
  → stream the body through (Bytes chunks, flush each; no re-encoding of SSE)
  → on client disconnect: drop the upstream request (ninfer frees the lane)
  → log line + metrics; release the admission permit
```

- The body is parsed once. SSE is never parsed, except to read the final `usage` chunk for the
  log (scanned in passing).
- Non-JSON or oversize bodies get a 400 or 413 before any upstream is picked.
- Per-card in-flight counts replace nginx `least_conn`; weights equal the lane counts from the
  layout.

### 4.3 Config and hot reload

`layoutctl.py gateway` keeps owning the truth. It writes **`router.json`** instead of (in shadow
mode, as well as) LiteLLM's `config.yaml` and nginx.conf:

```json
{
  "rev": "0ffb2d5b5d46",
  "layout": "merged-inkling-x2",
  "listen": "0.0.0.0:8010",
  "upstreams": {
    "A": {"url": "http://gpu-host:8094", "engine": "ninfer", "servedId": "inkling-2-mini",
          "lanes": 4, "keyFile": "/run/secrets/ninfer-key", "online": true},
    "B": {"url": "http://gpu-host:8096", "engine": "ninfer", "servedId": "inkling-2-mini",
          "lanes": 4, "keyFile": "/run/secrets/ninfer-key", "online": true}
  },
  "engines": {
    "ninfer": {"effortLevels": ["none","low","medium","xhigh"], "thinkingOffEffort": "drop",
               "dropParams": ["response_format"], "defaultEffort": null},
    "strata": {"defaultEffort": "low"}
  },
  "modelOverrides": {"inkling-2-mini": {"thinkingOffEffort": "none"}},
  "names": {
    "qwen3.8-27b":          {"kind": "pool"},
    "qwen3.8-flash-next-a": {"kind": "card", "card": "A", "fallback": true},
    "inkling-2-mini":       {"kind": "catalog", "catalogModel": "inkling2-mini", "title": "Inkling 2 Mini"},
    "dense-pi":             {"kind": "catalog", "catalogModel": "dense-pi", "ondemand": true,
                             "switchTo": {"engine": "ninfer", "engineProfile": "ninfer-lanes4-bench"}}
  },
  "onDemand": {"enabled": true, "profilesApi": "http://gpu-host:8098", "queueSeconds": 300},
  "slots": {"ttl": 90, "layoutFile": "/data/layout.json", "leadReserve": 1},
  "affinity": {"ttlSeconds": 900}
}
```

- **Reload:** watch the file (mtime, size, inode, as `effort_hook` does) plus `SIGHUP` plus
  `POST /admin/reload`. Parse and validate into an immutable `Arc<Snapshot>` and swap it
  atomically (`arc-swap`, or `RwLock<Arc<_>>`). A half-written or invalid file keeps the old
  snapshot and logs why. Requests in flight keep their snapshot.
- **Rev:** `/v1/model/info` reports `rev` as `gateway_rev`, so layoutctl's existing confirmation
  works unchanged.
- **Who changes:** the generator becomes a third renderer in `layoutctl.py`
  (`render_router`), next to `render_lb` and `render_litellm`. The `gateway.json` semantics
  (aliases, pinned names, modelAliases, effortLevels, thinkingOffEffort, onDemand) map
  one-to-one onto the fields above.

### 4.4 Auth

- **Upstream:** the router reads key files and adds `Authorization: Bearer` per upstream, as
  qfn-lb does now. Keys never appear in `router.json`, in logs or in `/v1/model/info`.
- **Clients:** off by default, with an accept-anything policy (today's behaviour). Optional
  `clientKeys` holds a list of key *hashes* (sha256) with labels. It is useful for per-client
  metrics and, later, for allowing a non-LAN listener. Admin endpoints (`/admin/*`) need a
  separate admin key file, or listen on loopback only.

### 4.5 Metrics and the dash

- `/metrics` for Prometheus, `/status` JSON for the dash.
- The dash replaces its three health rows (LiteLLM, qfn-lb, pig-slots) with one "router" row,
  plus per-card in-flight, queue and lease counts from `/status`.
- `/health/liveliness` stays for the old probe during the migration.

### 4.6 Failure behaviour

| Failure | Behaviour |
|---|---|
| One card down (connect error, 502 or 503 before headers) | Fail over once to the other eligible card. Mark the failed card cooling for 10 s. Pinned names (`-a`) fall back only if `fallback: true` (today's generator does this when a card is offline) |
| Both down | 503 `{"error":{"code":"no_upstream"}}` with `Retry-After: 10` |
| Upstream 429 (queue full) | Pass through. With admission control on (SHOULD), this mostly stops happening |
| Upstream 4xx | Pass through unchanged, with no retry (fixes today's triple-retried 404) |
| Mid-stream upstream error | Close the client stream (no retry after the first byte). Log `truncated` |
| Router config invalid | Keep the old snapshot. `/status` shows `configError` |
| Profiles API unreachable (on-demand) | 503 with `Retry-After`, as the relay does now |
| pig-slots state | Leases persisted to `leases.json` (same format). After a restart, unrenewed leases expire in 90 s |
| Router crash | `restart: unless-stopped`. Clients see connection refused for a few seconds; OpenOrca's retry/failover covers it |

### 4.7 Clients keep working unchanged

- **Port:** in its final position the router listens on `:8010`, and LiteLLM moves away.
  Optional compatibility listeners on `:8092` (qfn-lb paths) and `:8099` (registry) make the
  other two retirements drop-in too.
- **Names:** the same names from the same generator.
- **Shapes:** `/v1/models` keeps `owned_by: "openai"`; `/v1/model/info` keeps our keys;
  `/health/liveliness` keeps its text.
- **Headers:** `x-litellm-model-name` is still set.
- **Error JSON:** same shape.
- **Effort and thinking-off:** same results; the hook's cases are ported as tests.
- **OpenOrca slot traffic:** can keep going **directly** to ninfer with registry leases (now
  served by the router). Moving it behind the router (`slots.copies[].baseUrl = router /a/`,
  `/b/`) is optional, done later, and gives the router visibility of all load.

## 5. Migration plan

0. **Prep (no live change).**
   - Build `orca-router` with tests: effort-hook table, alias resolution, the `/v1/models` and
     `/v1/model/info` golden files (captured from today's LiteLLM), SSE passthrough with a fake
     ninfer from `orca-testkit`, failover, and the `decide()` port.
   - Add `render_router` to layoutctl behind `gateway.json "router": {"enabled": false}`.
1. **Smoke check on `:8020`** (minutes, not days).
   - Run the router container next to LiteLLM, with the same generated config and the same
     upstreams.
   - Compare with read-only GETs (`/v1/models`, `/v1/model/info`, health).
   - Replay a small captured set of request bodies at a quiet time, outside 06:45-07:30, with
     no bench running and with `max_tokens` small. Check for byte-equal JSON shapes and
     equivalent upstream bodies (the router's debug log of the rewritten body against an echo
     upstream).
   - The router takes no production traffic yet.
2. **Switch the clients** in one session. Each is a single URL change and can be reverted at
   once.
   1. `layoutctl gateway_test` and the dash (internal, lowest risk).
   2. The bench kit and the user's Abhoth CLI on HeyEmDee (`lmStudio.baseUrl` to `:8020`):
      the most features (tools, streaming, effort, images).
   3. OpenOrca web container config.
   4. Jarvis (HA options: base URL). Do it in the evening, not at the morning briefing.
   5. Video pipelines (`config.json research_llm_url`). Never during a run or the quiet
      window.

   After each, a quick check (a request through it, the request log, the dash row); roll that
   client back at once if anything is off. No day-long watch per client.
3. **Cut over the LiteLLM port.**
   - Stop LiteLLM and move the router to `:8010`; clients revert their URLs to `:8010`.
   - layoutctl stops rendering LiteLLM (`gateway.json router.enabled = true,
     litellm.enabled = false`), but **keeps rendering qfn-lb's nginx.conf**: qfn-lb stays up,
     untouched, as part of the rollback.
   - pig-slots can follow (router compat listener `--slots-port 8099`) once the lease replay
     tests pass (`tests/leases_compat.rs`); separately from qfn-lb.
3b. **Retire qfn-lb (`:8092`)** as its own step, once every client is on the router and its
   smoke check passed (no timed wait).
   - The end state on DeepThought is NPM (TLS for the public names only) -> `abhoth router` ->
     the engines. Nothing talks to `:8092` any more.
   - Until then qfn-lb is not touched: it is the rollback for anything that still reaches the
     cards through it.
   - Everything that uses `:8092` today, and what replaces it:

     | User of `:8092` | Path | Router replacement |
     |---|---|---|
     | LiteLLM deployments | `/ninfer/v1`, `/a/v1`, `/b/v1` | the router itself (cards addressed directly) |
     | profiles API on-demand relay (`lbUrl`) | `/a/v1/chat/completions`, `/b/...` | phase 3 in the router; meanwhile `lbUrl` -> router `/a/`, `/b/` (same paths) |
     | profiles API `Loads` poll (`lbUrl`) | `/a/slots`, `/b/slots` every 3 s | router `/a/slots`, `/b/slots` (passthrough, key added) |
     | music `chk.sh` | `/health` | router compat listener (`--lb-port`): `/` = main pool, so `/health` = the engine's |
     | dash (older versions; check the live one) | `/health` | the router row (`/status`) |
     | layoutctl `gateway` | renders nginx.conf, `lb_seen()` | stops rendering it when `gateway.json lb.enabled=false` (layoutctl change, needs OK) |
     | anything with `lbUrl` / `LB_PORT` in `layouts/` | `/<engine>/`, `/<card>/` | the same prefixes on the router (`/ninfer/`, `/a/`, `/b/`) |

   - The switch is one of two, both reversible: point `gateway.json lbUrl` at the router's
     paths, or run the router's compat listener on `:8092` after stopping qfn-lb. The
     second needs no client change; the first lets both run side by side first.
   - Rollback: start qfn-lb again (its last nginx.conf is kept) and set `lbUrl` back.
4. **SHOULD features.** Enable lease-aware placement, Lead priority and affinity behind config
   flags. Bench them (3 runs, per "Bench sizing") against the old placement before making them
   the default. Then on-demand in the router, with the profiles relay as fallback.
5. **Rollback.**
   - LiteLLM's app definition, `config.yaml`, `effort_hook.py` and nginx.conf stay in place,
     stopped rather than deleted, until no longer needed as rollback material (qfn-lb: running
     until step 3b, then kept stopped the same way).
   - Rollback means: stop the router, start `litellm` and `qfn-lb` / `pig-slots`, and set
     `gateway.json router.enabled=false` so layoutctl renders the old configs again.
   - Client URLs never change in the final state, so nothing else is needed.

## 6. Size estimate

| Piece | Lines of Rust (code + tests) | Agent-days |
|---|---|---|
| Proxy core: axum server, upstream pool, streaming, failover, errors, logging | 900 + 600 | 1.5 |
| Config, snapshot reload, name resolution, `/v1/models`, `/v1/model/info`, health | 500 + 400 | 1 |
| Effort / thinking-off / param hygiene (hook port) | 200 + 300 | 0.5 |
| CLI subcommand, feature flag, Dockerfile, compose, musl build in CI | 150 | 0.5 |
| layoutctl `render_router` + tests (Python) | 150 + 100 | 0.5 |
| Registry (pig-slots API) + lease-aware placement + priority admission | 600 + 500 | 1.5 |
| Session affinity | 200 + 200 | 0.5 |
| On-demand in the router (`decide()` port, wait/notes/keepalives, profiles API calls) + drain endpoints | 500 + 400 | 1.5-2 |
| Metrics, `/status`, dash row | 250 + 100 | 0.5 |
| **Total** | **~3.3k code + ~2.5k tests** | **~8-9 (MUST alone: ~4)** |

Calendar time on top: one switch-over session (smoke check and client switches, with a
ready rollback); benches for the SHOULD features about 3 hours (3 short runs).

## 7. Risks

1. **Invisible wire contracts.** Examples: `ServerIdentity`'s gateway detection (`owned_by`, list
   length) and what LiteLLM does to unknown top-level fields (passes them as extra body or drops
   them; which one decides whether `uncensored` reaches ninfer and 400s). Mitigation: golden
   files captured from today's LiteLLM, and a shadow replay that compares upstream bodies
   before any switch.
2. **ninfer strictness.** Any field the router forgets to drop becomes a 400 for every client
   at once. Mitigation: per-engine allowlists derived from `docs/serving.md`, plus a test that
   sends every field each client is known to send (table in 1.2).
3. **layoutctl coupling.** Today it hot-reloads LiteLLM, confirms via `gateway_rev`, holds and
   redeploys. The router must answer the same confirmation, or layoutctl falls back to a
   "redeploy" path that no longer means anything. Mitigation: `render_router` lands with its
   own confirm step; the LiteLLM path stays until cutover.
4. **Single point of failure, home-grown.** LiteLLM is already a single point of failure, but
   its bugs are someone else's. Mitigation: a small surface (MUST only at first), restart
   policy, and the 30-day rollback.
5. **Placement changes performance.** Lead reservation or affinity can make benches slower if
   tuned wrong. Mitigation: flags default off; the bench rule (3 runs, merge if no slower).
6. **The on-demand move touches the riskiest logic** (switch guards, quiet window, the video
   pipeline). Mitigation: port `decide()` with its tests verbatim and keep the profiles API as
   the actuator; the router only decides and waits.
7. **Scope creep** into a general LLM gateway. Hold the line at the WON'T list.

## 8. Standard options considered

From a web check on 2026-11-25. Items marked UNVERIFIED could not be confirmed from a page
that was read.

| Option | What it needs | Fit here |
|---|---|---|
| **Gateway API Inference Extension (GIE) / llm-d inference scheduler** (Go Endpoint Picker behind Envoy, Istio, agentgateway or NGINX Gateway Fabric; GIE v1.6.2, llm-d scheduler v1.0.0 in autumn 2026) | Kubernetes + Helm (even the "standalone EPP" is a chart with a proxy); vLLM-style model-server metrics; llm-d's precise prefix mode needs KV events and tokenisation | **No.** No Kubernetes here, and ninfer does not speak the metrics protocol (exact metrics UNVERIFIED) |
| **SGLang Model Gateway, now SMG** (`lightseekorg/smg`, Rust, Apache-2.0; smg 1.9.0 on PyPI) | Standalone with `--worker-urls`; `cache_aware` keeps an approximate radix tree in the router; also power-of-two, consistent hashing, rate limits, priority admission, circuit breakers | **Closest off-the-shelf.** But per SGLang docs its "OpenAI backend" mode allows one worker URL per router (UNVERIFIED for SMG's newer versions). It has no leases, no on-demand switching, and none of our effort or param rules. A design to borrow from, not to adopt |
| **NVIDIA Dynamo router** (KV-aware; `python -m dynamo.router`) | Workers registered with Dynamo, pre-tokenised input, etcd or file discovery, ZMQ or NATS KV events | **No.** A custom engine would need a Dynamo integration |
| **vLLM production-stack router** (Python, Kubernetes discovery; round robin, session-ID, prefix-aware marked WIP) | Kubernetes (a static backend mode is UNVERIFIED) | **No.** Weaker than a small custom router for two backends. vLLM Semantic Router picks models by classifying requests, a different problem |
| **LiteLLM's own features** | Session affinity via `litellm_session_id` or the `x-litellm-session-id` header (rendezvous hashing); `deployment_affinity` with the `prompt_caching` check; a request-priority scheduler (beta, UNVERIFIED) | Could give session affinity without our code, but not leases, Lead reservation or drain, and it keeps the stack above |
| **llama-swap** (Go, MIT) | Starts and stops upstream servers on demand per model | Same idea as our on-demand switching. Our switching is GPU-layout-aware and guarded (quiet window, video, bench), which llama-swap is not |

Sources:
- <https://github.com/kubernetes-sigs/gateway-api-inference-extension/releases>
- <https://github.com/llm-d/llm-d-inference-scheduler/releases>
- <https://docs.sglang.io/docs/advanced_features/sgl_model_gateway>
- <https://github.com/lightseekorg/smg>
- <https://docs.nvidia.com/dynamo/components/router/router-operations>
- <https://github.com/vllm-project/production-stack>
- <https://docs.litellm.ai/docs/proxy/cache_aware_routing>
- <https://github.com/mostlygeek/llama-swap>

**Verdict:** none of these covers leases, Lead priority, guarded on-demand switching and our
engine quirks together, and the large ones assume Kubernetes and vLLM-class engines. Build the
small router, and borrow from SMG: power-of-two fallback, priority admission tiers and
session-key hashing.

## 9. Loose ends found during the survey (not router work)

- `bench/pigrun.py` names the LiteLLM alias `qwen-flash`, which no longer exists.
- LiteLLM runs on the floating `main-stable` tag. Pin it while it remains.
- `llm-rr` (`:8011`) and `jarvis-brain` (llama.cpp `:8080`) are configured TrueNAS apps that
  are not running.
- Retries multiply: LiteLLM's `num_retries: 2` times nginx's `proxy_next_upstream_tries 2`
  turned one 404 into repeated attempts. Setting `num_retries: 0` for 4xx would help even
  without the router.

## 10. Implementation decisions (2026-11-25, phases 1-3)

| # | Decision | Why |
|---|---|---|
| D-R1 | `router.json` is generated from LiteLLM's rendered `config.yaml` plus qfn-lb's `nginx.conf` (`abhoth router import-litellm`, `--watch N` for the shadow). | layoutctl is not edited until switch-over; the shadow follows every layout change for free. |
| D-R2 | A standalone `abhoth-router` binary as well as `abhoth router`. | A small static binary for the image; same code. |
| D-R3 | Shadow port `8020`. | Free, outside the reserved list. |
| D-R4 | Unknown model: 400 with LiteLLM's exact message, type and code (not 404). | Clients match on it today. |
| D-R5 | `strip_fields` defaults to `metadata`, `user`. | LiteLLM never forwards them; found in the replay. |
| D-R6 | `extra_body` is merged into the top level before the effort rules. | Fixes a LiteLLM ordering bug: it sent `reasoning_effort: high` with `enable_thinking: false`. |
| D-R7 | No `stream_options.include_usage` injection and no re-chunking. | The client gets the stream it asked for; usage is scanned from the stream when present. |
| D-R8 | Response headers `x-litellm-model-name: openai/<served>`, plus `x-served-model`, `x-abhoth-card`, `x-abhoth-upstream`. | Parity for anything reading LiteLLM's header; the rest is for debugging. |
| D-R9 | `/v1/model/info`: `max_output_tokens` = max input for card deployments, `null` for relay ones; keys whose value is null are left out. | Matches the live golden capture. |
| D-R10 | One failover, before any byte is sent, on connect error, timeout, 502 or 503; a 10 s cooldown; if every card is cooling they are all tried anyway. | Never an outage caused by the cooldown itself. |
| D-R11 | Admission load = max(admitted, in-flight); the Lead never queues; others wait up to `queueSeconds` then go anyway. | Requests that bypass admission (qfn-lb paths) still count. |
| D-R12 | Placement precedence: lease, then session affinity (rendezvous hash), then least loaded. All placement is OFF by default. | Phase 2 must not change behaviour until it is switched on. |
| D-R13 | On-demand acts only through the profiles API (`/compose`, `/plan`, `/switch`); a plan changing both cards is blocked unless `allowBothCards`. Dry-run is the default. | One actuator; on-demand is off on the gateway until the router replaces LiteLLM. |
| D-R14 | UK DST computed by hand (last Sunday of March/October, 01:00 UTC). | No tz database in a static binary. |
| D-R15 | `/admin/*` loopback-only or admin key; `/admin/cards/<X>/drain` and `undrain`. | Drain before a switch, without exposing it publicly. |
| D-R16 | Compat listeners `--lb-port` (qfn-lb paths) and `--slots-port` (pig-slots API). | Lets the router take over `:8092` / `:8099` later with no client change. |
| D-R17 | The router's `/health` makes no upstream calls. | See the hazard in section 1.1. |

## 11. Shadow replay results (local, 2026-11-25)

`tools/router-shadow/replay.py local`: echo engines, local LiteLLM 1.106.6 with `effort_hook.py`
and the live config, against the router importing the same config. Upstream bodies and
responses were diffed per client shape (`shapes.py`).

- Fixed in the router after the first run: the `x-litellm-model-name` value, `metadata`/`user`
  stripping, `extra_body` merge order (D-R5, D-R6, D-R8); `model_info` null handling (D-R9).
- Remaining differences, all intentional: the router passes an explicit `stream:false`
  through; it does not inject `include_usage` or re-chunk (D-R7); the `extra_body` case
  (D-R6); local LiteLLM reports `max_output_tokens: null` where the live one says 65536 (the
  router matches live).
- Everything else identical, including unknown top-level fields such as `uncensored`
  (passed through by both).
- Live comparison so far is GET-only (`/v1/models`, `/v1/model/info`,
  `/health/liveliness`). Chat traffic against DeepThought waits for the go-ahead, and only on
  live aliases.

### 11.1 Live shadow on DeepThought (2026-11-25 16:15-16:26 UTC)

The router runs in shadow on `:8020` (`deploy/router/compose.yaml`, image `local/abhoth-router:1`),
with the importer following LiteLLM's live config (it picked up rev `789bfdb0f66c`,
`merged-inkling-x2`). Nothing points at it. `replay.py live --chat` sent each client shape to
LiteLLM and to the router: 19 shapes, about 4 requests a minute, only on allowlisted names that
LiteLLM routes to the card pool (no relay names were configured; on-demand is off).

- GETs: `/v1/models`, `/v1/model/info` (model_info) and `/health/liveliness` are identical.
  `litellm_params` differs only where intended: `api_base` lists the cards instead of qfn-lb.
  LiteLLM's 90+ cost/capability keys in `model_info` are not reproduced; no client reads them.
- Chat: 18 of 19 shapes matched. The one difference, `extra-body-nested`
  (`extra_body: {chat_template_kwargs: {enable_thinking: false}, reasoning_effort: high}`),
  is the D-R6 fix: LiteLLM forwards both and ninfer answers 400 "reasoning effort conflicts
  with enable_thinking"; the router resolves it and answers 200.
- Fixed during the run: `litellm_params.additional_drop_params` now shows in model_info
  (the drop itself already matched).
- Latency: `GET /v1/models` median 15 ms on the router against 30 ms on LiteLLM. For chats, the
  router-minus-LiteLLM median was +22 ms to the first byte and -10 ms in total (n=19, model
  time dominates). The router's own log puts upstream time within about 20 ms (the network
  round trip from the test client) of the client's total, so its overhead is a few ms.
- One slow request (13.3 s on card B against 1.7 s via LiteLLM) was card B's own time to
  headers while a bench was running on it. With placement off, the router balances only on its
  own in-flight count; it cannot see bench traffic that reaches the cards directly. Turning on
  lease-aware placement (or busy polling) before switch-over addresses that.

### 11.2 Shadow trial with placement on (from 2026-11-25 18:52 UTC)

Not a gate for the switch-over (user decision: no multi-day trials); its findings are input,
and it can stop whenever the switch-over happens.

Clients stay on LiteLLM, and qfn-lb logs nothing, so the trial is **decision-only**
(`observe` in router.json, `src/observe.rs`):

- **What is mirrored.** Not the requests: their record. The router follows each card's own
  ninfer request log (`$AI_ROOT/ninfer/logs/req-layout-{a,b}.jsonl`, mounted read-only),
  merging both in time order. Every real request (LiteLLM's pool, the pinned `-a`/`-b`
  aliases, benches and slot clients) shows up as `request_start` / `request_done` on the card
  that served it. pig-slots `/list` (GET, every 5 s) gives where the Lead holds a lease.
- **How placement decides.** At each arrival it rebuilds every card's true in-flight count and
  applies the router's policy: cache affinity (a request that reused at least 1024 cached
  tokens on its card stays there: the stand-in for session affinity, since the logs carry no
  session key), else least load per lane, then admission with the Lead reserve (one of the
  Lead card's 4 lanes kept back). It scores that choice against the actual card. Where they
  differ, the predicted gain is the engine queue wait avoided (if the other card had a free
  lane) plus the batching slowdown (tokens out times the difference in per-row decode time at
  the two batch sizes), minus the cached prompt that would be recomputed on the other card.
- **Stay unless it pays (from 2026-11-25 evening).** Least load keeps the request on its
  card while that card has a free lane, unless the same gain model, fed with what is known
  at arrival (the recent average answer length, capped by the request's own
  `max_tokens`), predicts more than `placement.minMoveGainSeconds` (default 1 s). A full
  card still moves. The first day's two disagreements both had a negative predicted gain.
- **Speed model.** Per-row decode time is learned from finished requests (decode seconds
  over tokens, at the batch size the request started into; answers under 64 tokens are
  skipped), prefill speed likewise. The first version used the logs' 5 s throughput events,
  whose rate includes idle time: a lightly used card looked up to 6 times slower than it is,
  which put -7.8 s on an 80-token answer.
- **Limits of the score.** Pinned traffic (`-a`/`-b` aliases, slot clients) could not have
  moved, but shows up as a disagreement whenever the other card was idler. The logs do not say
  which requests were pinned, so the agreement figure is a lower bound for pool traffic.
- **Load sent.** None from the scoring. A canary measures added latency: one tiny chat pair
  (LiteLLM, then the router, alternating; 8 tokens, thinking off, `qwen3.8-27b`) every 15 min,
  only while every card has 2 or more free lanes, never 06:45-07:30 UK.
- **Live placement.** The router's own placement is on too (admission, affinity, lease-aware)
  and sees the engines' true load (`feedPlacement`), but only the canary goes through it.
- **Where it is logged.** `$AI_ROOT/router/observe/decisions-YYYY-MM-DD.jsonl` (UK
  dates; one line per scored request, canary pair and error), `summary-YYYY-MM-DD.{json,md}`
  written at UK midnight; `GET :8020/observe[?date=]` and
  `docker exec abhoth-router-router-1 /app/abhoth-router observe-summary --dir /state/observe`
  on demand.
- **How to stop it.** Set `"observe": {"enabled": false}` in `$AI_ROOT/router/base.json`
  (the importer and router pick it up within 10 s), or stop the shadow altogether with
  `sudo docker compose -p abhoth-router -f $AI_ROOT/router/compose.yaml down`.
  The previous config is kept as `base.json.bak-20261125-1852` and `compose.yaml.bak-...`.

## 12. The Claude engine (Claude models on the operator's subscription)

Claude models (Opus, Sonnet, Haiku, ...) as ordinary routes, answered by the Claude Code CLI
under the operator's own Claude subscription (`crates/orca-router/src/claude/`). Behind the
cargo feature `claude-engine` (off by default; `orca-cli/claude-engine` for `abhoth router`)
and `claude.enabled` in `router.json` (off by default). A router built without the feature
refuses a config that turns it on.

### 12.1 Shape

- **A virtual server with one virtual card.** `claude` adds an upstream `claude` (engine
  `claude-cli`, url `claude-cli://local`, `lanes = maxSessions`) and one deployment per route
  (`claude-opus` -> `--model opus`, ...). Routing, failover to other cards of the same name,
  placement (admission, affinity, leases: the card's lanes become slots `claude1..claudeN`),
  the request log and `/metrics` treat it like any card. It is always ready: layout switches
  and the control plane never touch it, on-demand never picks it (`exclude_cards`), and the
  qfn-lb paths (`/claude/...`, the plain `/` pool) never reach it.
- **One CLI per request, no idle processes.** `claude -p --input-format stream-json
  --output-format stream-json --include-partial-messages`, spawned when a request gets a
  session and stopped when its answer is complete, the client goes away or `timeoutSeconds`
  passes. At most `maxSessions` (default 32; no cap beyond the config) run at once; the rest
  queue for `queueSeconds`, then get 429 + `Retry-After`. The limit that matters is the
  subscription's rate limits, not hardware.
- **A plain model, not an agent.** `--tools ""` (none of Claude Code's tools),
  `--permission-mode dontAsk`, `--safe-mode` (none of the operator's CLAUDE.md, hooks,
  plugins, skills or MCP servers), `--strict-mcp-config`, `--no-session-persistence`, an
  empty working directory (`<workDir>/work`), and the caller's system and developer messages
  as `--system-prompt-file` in place of Claude Code's prompt (`defaultSystemPrompt` when the
  caller sends none). `ANTHROPIC_API_KEY` and `ANTHROPIC_AUTH_TOKEN` are removed from the
  CLI's environment so it always uses the subscription.
- **Why not `cc-sdk`.** The crate the request pointed at (`claude-code-sdk-rs` 0.8.1, last
  commit 2026-04-04) was read before deciding. Its default feature downloads
  `https://claude.ai/install.sh` and runs it with `bash -c` (PowerShell `iex` on Windows) when
  no CLI is found; it brings reqwest 0.11 next to our 0.13, `rand`, `crossbeam`, `which`,
  `dirs` and `unsafe` libc user lookups; and its own SDK-MCP dispatch path is a placeholder.
  What the engine needs is about 1k lines over the CLI's stream-json and control protocol, so
  it has its own small driver and no third-party code.

### 12.2 API surface

| | |
|---|---|
| `POST /v1/chat/completions` | streaming (SSE `chat.completion.chunk`, `[DONE]`) and not; `model` is the resolved Claude model (`claude-haiku-5-5`) |
| `GET /v1/models`, `/v1/model/info` | the routes, `max_input_tokens` = `contextTokens`, `max_output_tokens` = `maxOutputTokens` |
| usage | `prompt_tokens` = input + cache writes + cache reads, `prompt_tokens_details.cached_tokens` = cache reads, `completion_tokens`; on the finish chunk always, plus the empty-choices chunk with `stream_options.include_usage` |
| images | `image_url` parts (`data:` base64 or http(s) URLs) become image blocks |
| `reasoning_effort` | `--effort` (`minimal` -> `low`; `none` = the CLI's default); thinking streams as `reasoning_content` |
| `max_tokens` / `max_completion_tokens` | `CLAUDE_CODE_MAX_OUTPUT_TOKENS` |
| client tools | yes (12.3) |
| refused with 400 | `n > 1`, tool types other than `function`, tool names outside `[A-Za-z0-9_-]{1,56}`, audio and file parts |
| ignored | `temperature`, `top_p`, `stop`, `seed`, `response_format`, `tool_choice` other than `none` (the CLI has no way to pass them) |
| `/v1/messages` | not served: the router has no Anthropic surface yet (ROUTER-CONFIG.md 10.3) |

The CLI takes user turns only. A single user message goes as it is; a conversation with
history (earlier replies, tool calls and results) goes as one user turn holding a transcript
of it (`<user>`, `<assistant>`, `<tool_call>`, `<tool_result>` tags) with an instruction to
continue it.

### 12.3 Client tools

OpenAI `tools` reach Claude as real tool definitions: the engine passes
`--mcp-config {"mcpServers": {"c": {"type": "sdk", "name": "c"}}}` and `--allowedTools mcp__c`,
and answers the CLI's `mcp_message` control requests itself (`initialize`, `tools/list` with
the client's JSON schemas). Claude answers with real `tool_use` blocks; they stream out as
`tool_calls` deltas (`mcp__c__` stripped from the names) and the answer ends with
`finish_reason: "tool_calls"`. The CLI is stopped at the end of that message, before it would
call the tool, so the client runs it and sends the result in its next request (as transcript
history). `tool_choice: "none"` offers no tools.

### 12.4 Errors, rate limits, backpressure

| CLI says | Answer |
|---|---|
| `rate_limit_event` `rejected` / `error: rate_limit` / "hit your limit" | 429 `rate_limit_error`, `Retry-After` = seconds to `resetsAt` (60 if unknown). The engine remembers the reset: until then requests wait up to `rateLimitWaitSeconds` (default 30) for it, longer waits get 429 at once without starting a CLI |
| not logged in, token refused | 503 `upstream_auth_error` |
| overloaded, 5xx, `API Error` | 503 |
| ended without a result | 502 with the CLI's stderr tail (the token scrubbed) |
| no session free within `queueSeconds` | 429, `Retry-After: 5` |
| `timeoutSeconds` passed | 504 |

429, 502 and 503 before the first byte fail over to the route's other cards (a deployment of
the same name on a local card), like any engine; the virtual card is not put in cooldown (the
engine keeps its own state). An error after streaming started ends the stream with an
`{"error": ...}` event and `[DONE]`.

### 12.5 Access

`claude.access` (default `admins`) decides who may use the routes: `admins`, `users`,
`anonymous` or `keys:<names>`. It is one person's subscription and Anthropic's consumer terms
are about personal use, so it is not opened to service keys or other users unless the operator
changes it. With the control plane on, its principals decide (`may_use_route`); without it an
admin is a loopback caller or one sending `settings.adminKeyFile`'s key as Bearer. Callers who
may not use it keep the route's other cards, or get 403 when it has none.

### 12.6 Config

```jsonc
"claude": {
  "enabled": true,
  "card": "claude",                    // the virtual card's id
  "command": "claude",                 // or a full path
  "maxSessions": 32,                   // concurrent CLIs (lanes); spawned per request, none idle
  "queueSeconds": 300,
  "timeoutSeconds": 900,
  "rateLimitWaitSeconds": 30,
  "workDir": "/state/claude",          // default <temp>/abhoth-claude
  "oauthTokenFile": "/run/secrets/claude-oauth-token",  // containers; none = the CLI's login
  "home": "/state/claude/home",        // the CLI's own HOME; none = the router's HOME
  // admins only by default: it is the operator's personal subscription and Anthropic's
  // consumer terms are about personal use. Change only knowingly.
  "access": "admins",
  "safeMode": true,
  "routes": {"claude-opus": "opus", "claude-sonnet": "sonnet", "claude-haiku": "haiku"},
  "contextTokens": 200000,
  "maxOutputTokens": 32000,
  "extraArgs": []                      // after the engine's own, e.g. ["--fallback-model", "sonnet"]
}
```

A deployment written by hand with the same name and `cards: ["claude"]` keeps its place in
`deployments` (put it before a local deployment of the same name to prefer Claude). The JSON
version, with the note on `access` as a field, is `deploy/router/claude.example.json`.

### 12.7 Deploying (not done yet; needs the operator)

1. On a machine with a browser, the operator runs `claude setup-token` (interactive: it logs
   in with the Claude subscription and prints a long-lived OAuth token). Nobody else can do
   this step.
2. Save it on the host as `$AI_ROOT/router/secrets/claude-oauth-token` (owner 568:568, mode
   0600). Never in `router.json`, compose files or the environment.
3. Build the image with the engine and the CLI (Node from Debian, the CLI version pinned):
   `docker build -f deploy/router/Dockerfile --build-arg FEATURES=claude-engine
   --build-arg CLAUDE_CLI=@anthropic-ai/claude-code@<version> -t local/abhoth-router:1 .`
4. In `compose.yaml` (router service): mount the token read-only at
   `/run/secrets/claude-oauth-token`, give the CLI a writable home
   (`HOME=/state/claude-home`, inside the existing `/state` volume, 568:568), and add the
   `claude` block of 12.6 to `$AI_ROOT/router/base.json` (the importer carries it into
   `router.json`; the router hot-reloads it).
5. Check: `GET :8020/v1/models` lists the routes; one `claude-haiku` chat with
   `max_tokens: 8` (from loopback inside the container, or with the admin key) answers;
   `/metrics` shows `abhoth_claude_sessions_started_total 1`.
6. Rollback: `"enabled": false` in `base.json` (hot reload), or the previous image.

In the single container (D148: web UI, router and supervisor in one process) the web UI's
Claude Code bridge and terminal already use `HOME`, so the engine gets a `home` of its own
inside the router's state (`/data/router/claude/home`, with `workDir` `/data/router/claude`):
32 CLIs starting at once must not write the web UI's `~/.claude.json`. The image is built with
`--build-arg FEATURES=claude-engine` (the CLI is in it already), the token is mounted read-only
at `/run/secrets/claude-oauth-token` (owner 568, mode 0400) and the `claude` block goes into
the router config the container reads (`router.onedocker.json`, hot reloaded). Rollback is the
same: `"enabled": false`, or the previous image.

Measured on the operator's PC (2026-11-26, CLI 2.1.332, haiku): 2.1 s to the first token and
2.2 s in all for a short streamed answer, 3.8 s for a tool call, 1.7 s for the follow-up turn
with the tool result; the very first request after the router started took 51 s (the CLI's
first start in a new working directory).

## 13. How clients find and understand the router (D149)

Design and the per-setting table: [`docs/design/ROUTER-MODE.md`](docs/design/ROUTER-MODE.md).

- `GET /.well-known/abhoth-router`: unauthenticated identification (name, version,
  capabilities, whether this caller needs a key). No secrets; read-only.
- `GET /v1/abhoth/models`: the models this caller may use with their context window, output
  limit, vision, tool calling, reasoning levels, state and sampling defaults (from the
  deployments' `model_info` and `params`; LiteLLM's `supports_vision`,
  `supports_function_calling` and `supports_reasoning` keys win), whether Claude is among them
  and, for callers that may see them, the cards. Authenticated like `/v1/abhoth/view`.
  Image input (and a missing context window) comes from the engines themselves: each online
  card's `GET /v1/models` (`architecture.input_modalities`, llama.cpp `capabilities`) or
  `GET /props` (`modalities.vision`), cached 5 min (1 min when it did not answer), 2 s timeout;
  offline cards are never asked. Cold catalog models use the catalog: `models.<id>.vision` and,
  on engines that need an option for images (`vision`, llama.cpp `mmproj`), whether the
  on-demand pick's profile sets it. `supports_vision` in a deployment's `model_info` overrides.
- mDNS / DNS-SD `_abhoth-router._tcp` (TXT `v`, `api`, `path`, `name`): `--mdns auto|on|off`
  (auto: on unless the gateway is loopback-only), `--name NAME` (else `ABHOTH_ROUTER_NAME`, else "Abhoth router on
  <Host>": the short host name, capitalised; plain "Abhoth router" for a container id), `--key-url URL` (where users make API keys).

The app's settings (Models → Connection) use them for "router mode": the router then supplies
everything model-specific, and the client's failover, slots and inference profiles are off.

## 14. The admin pages (D151)

The router serves its own admin UI at **`http://<router>:8010/admin/`** (the gateway's port),
so a router running without the Abhoth web app can still be set up and watched in a browser.
Design: [`docs/design/ROUTER-ADMIN.md`](docs/design/ROUTER-ADMIN.md).

- **Start the router with `--control router.toml`.** A router with no GPU control yet needs only
  a minimal file: `version = 1`, `[router] state_dir = "/data/router/state"`,
  `[control] enabled = false`; the forms then add servers, cards, engines and layouts.
- **Make an admin key** (shown once; only its hash is kept):
  `abhoth router admin-key create <name> --control router.toml [--out FILE]`
  (`--out` writes it to a new file with mode 600). `admin-key list`, `admin-key revoke <id|name>`.
  In Docker: `docker exec <container> /app/abhoth router admin-key create …`.
- **Sign in** at `/admin/` with that key (or a web-server admin's `abk_` key). The session is a
  cookie (HttpOnly, SameSite=Strict, Secure over HTTPS) for 8 hours, 2 hours idle. Ten wrong
  keys from one address in 15 minutes lock that address out for the rest of the window.
- **What you can edit:** servers and cards, engines and their profiles (lanes), layouts
  (`router.toml`); the model list, upstreams, the Claude block and slots (`router.json`); plus
  both files as text under Advanced. Every save shows the diff and the router's own check first,
  keeps a backup (`router.toml`: the control plane's backups; `router.json`:
  `<dir>/backups/`), writes atomically, is audited and is reloaded. Saving never switches a
  layout: switching is still the Overview's plan and confirm.
- **Behind a reverse proxy:** any prefix works (the page uses relative URLs). Send
  `X-Forwarded-Proto: https` so the cookie is marked Secure. A request with forwarding headers
  is never treated as a loopback admin.
- **From the Abhoth app:** in router mode with an admin key, Settings → Models → Connection →
  Router admin.

## 15. The inventory (D152)

The admin pages' **Inventory** tab sets the router up as servers → cards → which engines may run
each kind of model, with an engine catalog and models whose engines are worked out from their
files. Design: [`docs/design/ROUTER-INVENTORY.md`](docs/design/ROUTER-INVENTORY.md).

- **Upgrading converts `router.toml` once** at start-up (cards get `engines`, models get `kind`,
  `primary`, `prefer`, `servers`; `[inventory] version = 1`), comments kept, the old file backed up
  to `<state_dir>/backups/router.toml.pre-inventory-<time>`. Nothing behaves differently. To go
  back to 0.16.13, restore that backup first (the old router refuses the new fields).
- **Set a model folder** per server (`servers.x.model_dir`) for Hugging Face downloads; they run on
  the server (`<model_dir>/<owner>/<name>/`), resumable. A Hugging Face token, if you need one for
  gated models, is set on the page and kept in `<state_dir>/secrets/hf-token`, never in the config.
- **Engines describe what they run** (catalog + a read-only probe of idle running engines +
  `caps` overrides). A fork: `[engines.my-fork] caps = { inherits = "llamacpp" }` runs every
  model llama.cpp runs.
- **Routes may let the router pick the card**: `on_demand = { layout_from = "auto" }` places the
  model on a card that has its files, allows the engine and has room (primary engine first). A
  route's `pick` still pins an engine and profile.
- **Strict hostctl hosts** need their allowlist re-exported for `download`, `download-status` and
  `gguf-meta`. New models and routes are switchable on demand after a router restart.
