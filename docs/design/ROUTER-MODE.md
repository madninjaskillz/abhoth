# Router mode (D149)

Status (2026-11-26): **built.** The app's settings know two kinds of model server:

- **Abhoth router** (`lmStudio.connection: "router"`): an `abhoth router`. It knows each model's
  context window, output limit, vision, tool calling and reasoning levels, fails over between
  cards and routes, holds the slot leases and owns the GPU layouts. The app needs its URL, a key
  (when the router asks for one) and which model to use; everything else comes from the router.
- **OpenAI-compatible server** (no `connection`, or `"server"`; the default, and what every
  existing config is): LM Studio, llama.cpp, vLLM, Ollama or a gateway, set up here as before.

Pasting a router's URL, or picking one the app found on the LAN, offers router mode; nothing
switches by itself.

## 1. The settings, per mode

Every setting in `config.json` (`orca-core` `config::model`), and what router mode does with it.
**Hidden**: not shown in router mode, the value is ignored (the router owns it, or it has no
meaning there). **Router**: not shown as a setting, the value comes from the router's metadata
for the chosen model and is shown read-only in the connection panel. **Shown**: unchanged. The
settings page's rows carry this as `routerMode` (`config::schema::InRouterMode`); the rest of the
table is applied by `orca_core::router_mode::overrides`.

Router values are put over the config like a profile's: for the session only, never written to
`config.json` (`ConfigManager::apply_router`, kept out of saves as the overlay named `router`).
Switching back to server mode finds the file's own values untouched.

### 1.1 The model server (`lmStudio`, settings tab Models)

| Setting | Router mode | Why / where it comes from |
|---|---|---|
| `connection` (new) | the panel's choice | "Abhoth router" or "OpenAI-compatible server" |
| `baseUrl` | Shown | the router's URL (`…/v1`) |
| `apiKey`, `apiKeyEnv` | Shown | an Abhoth API key (`abk_…`) when the router needs one |
| `models` (priority list) | **Router** | one default model, picked from the router's list; the router's routes do the failing over, so client-side backups are off (the list becomes the one chosen model) |
| `model`, `backupModel` (legacy) | Hidden | migrated into `models` on load, as before |
| `backupMinResponseLength` | **Hidden** | client-side failover between models: the router's job |
| `backupOnRefusal` | **Hidden** | same |
| `pairModelProgramming` | **Hidden** | needs several models of its own |
| `nativeToolCalling` | **Router** | the model's `tools` |
| `vision` | **Router** | the model's `vision` (kept as configured when the router does not know) |
| `provider` (`anthropic`) | **Hidden** | the router speaks the OpenAI API and adapts itself |
| `temperature` | Shown | the user's choice; sent on every request |
| `sampling` | Shown | the user's choice |
| `modelSampling` (per model) | **Router** (merged) | the router's per-model defaults (`params` of its deployments: temperature, top_p, top_k, min_p, penalties) under the chosen model; a value set here for that model still wins |
| `maxTokens` | Shown | a cap the user chooses (blank: none sent). The start-of-session negotiation is skipped: it sends real chat requests |
| `timeoutSeconds`, `streamingTimeoutSeconds` | Shown | how long this client waits |
| `promptProfile` | Shown | the prompt family |

### 1.2 Context (tab Models)

| Setting | Router mode | Why |
|---|---|---|
| `contextWindowSize` | **Router** | the model's `contextWindow` (`max_input_tokens`) |
| `autoDetectContextWindow` | **Hidden** (off) | the LM Studio / Ollama probes are not needed |
| `autoCompact*`, `reserveTokens`, `keepRecentTokens`, `keepFirstTokens`, `compactAtRatio`, `compactCostGate`, `thinkingAwareCompaction`, `compactPreserveLastN` | Shown | how this client manages its conversation |
| `charsPerToken`, `prefillTokensPerSecond` | Shown | estimates |

### 1.3 Inference profiles, slots, layouts

| Setting | Router mode | Why |
|---|---|---|
| `profiles.active` (Default profile) | **Hidden** | inference profiles switch GPU layouts: the router's. Not applied at startup (`--profile` is ignored with a note) |
| `profiles.serverUrl`, `profiles.timeoutMs` | **Hidden** | no profiles API in router mode: the profile pickers (settings, chat footer) are gone |
| `slots` (pig-slots: copies, lead, registry, leases) | **Hidden** (off) | the router balances the cards and holds the leases |
| layout switching | Hidden | the router's layouts and its admin pages (the web UI's Inference tab, for router admins) |

### 1.4 Everything else: Shown

`permissions`, `session`, `agent` (engine, iterations, modes, peer/lead settings, reasoning
caps, `providerSchedule`, `planner`, `uncensored`), `thinking` (`reasoningEffort`, `level`: the
router maps efforts per model with its `effort_levels`), `providers` (other named endpoints),
`imageGen` (its own server), `hooks`, `memory`, `shell`, `lsp`, `formatter`, `tools`,
`toolProfile`, `pi`, `mcpServers`, `bridges` (the local Claude Code and Codex CLIs), `ui`,
`desktop`. They are about this client and its agent, not about the model server.

## 2. What the router serves

### 2.1 `GET /.well-known/abhoth-router` (identify)

Unauthenticated, no secrets, CORS `*`, read-only (a GET never switches: `switch::gate`; also in
`tests/probe_safety.rs`'s every-path, every-method, every-listener sweep).

```json
{"service": "abhoth-router", "api": 1, "name": "Abhoth router on gpu-host", "version": "0.16.9",
 "capabilities": ["chat", "models", "model-meta", "user-view", "keys", "claude", "mdns"],
 "auth": {"required": true, "keys": true, "keyPrefix": "abk_", "keyUrl": "https://…"},
 "paths": {"openai": "/v1", "models": "/v1/abhoth/models", "view": "/v1/abhoth/view"}}
```

`auth.required` is for a caller from where this request came from (anonymous LAN callers may
be allowed; `access.anonymous_lan`). `name`: `--name`, else `ABHOTH_ROUTER_NAME`, else "Abhoth router on <Host>" (short host
name, capitalised; "Abhoth router" for a container id);
`keyUrl`: `--key-url` (where users make keys, the public web UI). The client never probes
anything else on an unknown URL: LiteLLM's `/health` sends a real request to every model.

### 2.2 `GET /v1/abhoth/models` (metadata)

Authenticated like `/v1/abhoth/view` (with the control plane on; open without it), filtered by
route access and `hidden` like the user view. Per model: `id`, `contextWindow`,
`maxOutputTokens`, `vision`, `tools`, `reasoning`, `effortLevels`, `engine`, `kind` (`local`,
`relay`, `cloud`), `status` (`live`, `cold`, `switching`), `cold` policy, `sampling` defaults.
Also `principal` (who the router took the caller for, never a key), `claude` (a Claude model is
among them: this key may use Claude), `cards` (id, engine, model, online; only for callers that
may read control state) and `rev`.

Capabilities come from the deployments' `model_info`: LiteLLM's own keys
(`supports_vision`, `supports_function_calling` / `supports_tool_choice`, `supports_reasoning`)
win (the override); otherwise tools are implied by the engine (ninfer, Strata, vLLM, SGLang,
llama.cpp, the Claude CLI), reasoning by `effort_levels`, and **vision by the engines**: each
online card's own `GET /v1/models` (ninfer / OpenRouter `architecture.input_modalities`,
llama.cpp `capabilities: ["multimodal"]`), else `GET /props` (`modalities.vision`), with the
engine's key and `x-abhoth-no-switch: 1`, cached 5 minutes (`discovery::caps`). A name over
several cards takes images when any of them does. Offline cards are never asked; for a cold
catalog model the control plane's catalog answers (`models.<id>.vision`, and on an engine that
needs an option for images, ninfer `vision` / llama.cpp `mmproj`, whether the on-demand
pick's profile sets it; `resolve::route_vision`). A route is `live` while an online card serves
it (`pool:live`, `card:X`, or a card whose catalog model is the route's). When nothing knows,
`vision` is left out and the client keeps its own setting. The engines' context window fills a
missing `max_input_tokens`.

### 2.3 mDNS / DNS-SD

`_abhoth-router._tcp.local.`, instance = the display name, TXT `v` (version), `api`, `path`
(`/.well-known/abhoth-router`), `name`. `--mdns auto` (default) advertises when the gateway
listens beyond loopback; `--mdns on|off` forces it. One daemon thread (`mdns-sd` 0.21.7,
`#![forbid(unsafe_code)]`, default features off: no async runtime, no logging; its two small
dependencies with FFI, `if-addrs` and `socket-pktinfo`, read interface addresses). Dropping the
advert sends a goodbye. A specific `--host` IP is announced alone; `0.0.0.0` announces every
interface's addresses and the browser ranks them (private LAN IPv4 first, Docker bridges and
link-local last).

## 3. The client

- **The API key in router mode:** the panel says what the configured key is (never the key:
  `none`, `placeholder`, `abhoth`, `other`). LM Studio's placeholder `lm-studio` is never sent to
  a router and reads "not sent to the router. No key needed on your LAN" (plus "add an Abhoth key
  (`abk_…`) to use Claude" when the router has Claude); a non-`abk_` key on a router with keys
  is flagged as one it will refuse.
- **Settings → Models → Connection** (`client/src/features/settings/ConnectionPanel.tsx`): the
  two modes; in server mode, routers found on the LAN (each checked with its identification
  document before "Use it" is offered) and a URL box with "Check"; when the configured URL
  identifies as a router, "Use router mode". In router mode: the router's name, version and
  whether a key is needed; the default model picker (live models first); the models with their
  context, capabilities and state; Claude availability for this key; the cards when visible;
  and which settings the router now takes care of. The rows the router owns are left out
  (`connection.ts` `visibleInMode`, from the schema's `routerMode`).
- **Web server** (`orca-webui` `connection.rs`, admin-only under `/api/settings`):
  `GET /api/settings/connection`, `POST …/identify {url}`, `GET …/discover`, `GET …/models`,
  `POST …/use {url, model?}` (connection and base URL in one save, audited
  `settings.connection`; only for a URL that identifies as a router).
  `/api/projects/{pid}/settings/connection[/models|/use]` are the same for the settings page's
  Project scope: that project's running session's config, changed as /config in its terminal
  would (saved to config.json too; the session connects that way when it restarts). The panel
  always talks to the scope it is in (it once wrote the server's config.json while
  showing the project's session, so "Use it" and the router card seemed to do nothing there).
- **Switching back:** going to router mode keeps the server mode's Base URL, key and models
  (`lmStudio.serverMode`, written only while in router mode); choosing "OpenAI-compatible
  server" again puts them back (`LmStudioConfig::set_connection`, the connection setting's
  setter, so /config does the same). Clicking the router card uses the Base URL when it is a
  router, else the one router found on the LAN, else asks for a URL (the URL box, focused).
- **Sessions** (`orca_core::router_mode::apply`, at startup and for each project session):
  metadata fetched with the configured key (cached 30 s), overrides applied, notices for a
  model the router does not offer to this key or when none is chosen (the first live one is
  used). When the router can't be reached the router-owned settings are still turned off and
  the model settings stay as configured.
- **Probes never switch:** every request the client makes for discovery and metadata is a GET
  with `x-abhoth-no-switch: 1`, without redirects. The `max_tokens` negotiation (real chats) is
  skipped in router mode.

## 4. Migration

Existing configs have no `connection`: server mode, unchanged, byte-identical on save. When the
configured Base URL identifies as a router, the Connection panel says so and offers router mode;
it never switches by itself. A router older than D149 has no identification document: it is
treated as an OpenAI-compatible server (as today) until it is updated.

## 5. Discovery on the real network

See the D149 entry in PORTING.md for the result of the first deployment (router in a
Docker container with host networking on TrueNAS SCALE).
