# `abhoth router`: a gateway to replace LiteLLM on DeepThought (design)

Status: **design only, nothing built.** Survey date: 2026-11-25, read-only, against the live
layout `merged-inkling-x2` (ninfer `inkling-2-mini` on both cards, 4 lanes each). Keys and
tokens are left out of this document on purpose.

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
  `orca-router`. That is about 8-9 agent-days across three phases (MUST alone: about 4), plus
  1-2 weeks of shadow/switch calendar time.
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
`/mnt/.ix-apps/app_configs/litellm/versions/1.0.0/templates/rendered/docker-compose.yaml`.

- **Image:** `ghcr.io/berriai/litellm:main-stable`. This is a floating tag, so a redeploy can
  pull new code.
- **Ports and mounts:** `8010:4000`, on its own bridge `ix-litellm_default` (172.16.3.0/24).
  Two read-only bind mounts: `/home/admin/litellm/config.yaml` to `/app/config.yaml`, and
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
| **HA ha-bridge** (`/home/admin/ha-bridge/ha_bridge.py`), **Jarvis** `deepthought.py` | `:8098` profiles API only (switch buttons, live view) | n/a | n/a | none (LAN) |
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
1. **Shadow on `:8020`.**
   - Run the router container next to LiteLLM, with the same generated config and the same
     upstreams.
   - Compare with read-only GETs (`/v1/models`, `/v1/model/info`, health).
   - Replay a small captured set of request bodies at a quiet time, outside 06:45-07:30, with
     no bench running and with `max_tokens` small. Check for byte-equal JSON shapes and
     equivalent upstream bodies (the router's debug log of the rewritten body against an echo
     upstream).
   - The router takes no production traffic yet.
2. **Switch clients one by one.** Each step is a single URL change and can be reverted at once.
   1. `layoutctl gateway_test` and the dash (internal, lowest risk).
   2. The bench kit and the user's Abhoth CLI on HeyEmDee (`lmStudio.baseUrl` to `:8020`):
      the most features (tools, streaming, effort, images).
   3. OpenOrca web container config.
   4. Jarvis (HA options: base URL). Do it in the evening, not at the morning briefing.
   5. Video pipelines (`config.json research_llm_url`). Do it last, after a full day's run on
      the router, and never during a run or the quiet window.

   Watch each for a day: error rate, TTFT and the request log, against LiteLLM's numbers.
3. **Cut over the port.**
   - Stop LiteLLM and move the router to `:8010`; clients revert their URLs to `:8010`.
   - Then retire qfn-lb (router compat listener `:8092`) and pig-slots (compat listener
     `:8099`), each separately.
   - layoutctl stops rendering LiteLLM and nginx (`gateway.json router.enabled = true,
     litellm.enabled = false`).
4. **SHOULD features.** Enable lease-aware placement, Lead priority and affinity behind config
   flags. Bench them (3 runs, per "Bench sizing") against the old placement before making them
   the default. Then on-demand in the router, with the profiles relay as fallback.
5. **Rollback.**
   - LiteLLM's app definition, `config.yaml`, `effort_hook.py` and nginx.conf stay in place,
     stopped rather than deleted, for 30 days.
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

Calendar time on top: shadow plus client-by-client switching, about 1-2 weeks of light
attention; benches for the SHOULD features about 3 hours (3 short runs).

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
