# Local Engines, Cloud, and a local Sothoth (D156)

**Names ([NAMES.md](../NAMES.md)):** Sothoth Router (router), Migo Bridge (bridge), Nodens
Supervisor (supervisor). Builds on [MACHINE.md](MACHINE.md) (D155) and ROUTER-CONFIG.md.

## 1. What the user asked for (2026-11-27)

- A **Local Engines** settings tab listing every engine this PC uses:
  - **managed**: Abhoth installs and runs it (This Machine's inventory: store packages, cards,
    lifecycle);
  - **unmanaged**: already running, here or elsewhere. The minimum is a **kind** (llm, tts,
    stt, image, music, embedding, video), a **name** and a **URL**, plus an optional API style
    and key.
- Models belong to engines. An unmanaged engine's model may be **Engine default** (no model
  list: the engine's own default is used).
- Each engine has **Expose on Migo Bridge** (only used while Migo Bridge is on). An engine on
  another machine may be exposed too, off by default, labelled "relayed through this PC".
- Local Engines replaces the plain LLM server and image server settings. The connection is
  **Sothoth Router** or **Local Engines**.
- Non-LLM engines have **Expose as tool**: one agent tool per kind (`generate_image`, `speak`,
  `transcribe`, `make_music`), each with a default engine; the others are picked by model name.
- **Cloud**: a pseudo-server type in the Sothoth Router's inventory with no cards, whose engines
  are cloud providers (Anthropic, OpenAI-compatible, OpenRouter…), each with a key (a secret,
  never shown), a base URL and models. The router routes to them like any engine. The same Cloud
  group appears in Local Engines, and config.json's `providers` move into it.
- Every host (the desktop app and the web UI on a server) gets Local Engines, for admins.
- In the end, Local Engines is a **local Sothoth**: with no router server, the PC's own
  inventory (This PC, Cloud and unmanaged URLs) is routed by an embedded Sothoth.

## 2. Where things are kept

Everything stays in the machine folder (`<home>/machine/`, MACHINE.md 2.2). New files sit
beside the existing ones, so a version without Local Engines still reads the folder:

| File | Holds | New |
|---|---|---|
| `router.toml` | the inventory (server, cards, managed engines, models) | `[upstreams.*]` mirrors of the endpoints (2.1, phase 2) |
| `engines.toml` | manual engines | |
| `endpoints.json` | unmanaged engines and Cloud providers (2.1) | yes |
| `share.json` | Expose on Migo Bridge | `endpoints` map |
| `tools.json` | Expose as tool, and each kind's default engine | yes |
| `secrets/<id>` | endpoint keys (mode 600; the API never returns them) | yes |

### 2.1 Endpoints (`endpoints.json`)

```json
{
  "rev": "…",
  "endpoints": {
    "lm-studio": { "title": "LM Studio", "group": "unmanaged", "kind": "llm",
                   "url": "http://127.0.0.1:1234/v1", "api": "openai", "key": "secret:lm-studio",
                   "models": [] },
    "anthropic": { "title": "Anthropic", "group": "cloud", "kind": "llm",
                   "url": "https://api.anthropic.com/v1", "api": "anthropic", "key": "secret:anthropic",
                   "models": ["claude-sonnet-5-5"] },
    "comfy-nas": { "title": "ComfyUI on the NAS", "group": "unmanaged", "kind": "image",
                   "url": "http://nas:8188", "api": "comfyui", "models": [] }
  }
}
```

- `group`: `unmanaged` or `cloud` (Cloud providers are listed under Cloud, on the router too).
- `api`: how to talk to it. LLM: `openai` (default), `anthropic`, `ollama`. Media: the router's
  `media_api` values (`openai-images`, `comfyui`, `openai-speech`, `wyoming`, `openai-stt`,
  `sa3`, `synth`).
- `models: []` is **Engine default**: requests carry the model the caller asked for, else none.
- From phase 2, each endpoint is mirrored into `router.toml` as `[upstreams.<id>]` with only fields older
  versions already parse (`kind`, `base_url`, `key`, `models`), like manual engines are mirrored
  as external engines. The router tooling (checks, remote config, the Cloud group on a router)
  sees them; `endpoints.json` keeps what the router schema has no field for.

## 3. Phases (each its own release, with tests and screenshots)

1. **Local Engines tab** (0.16.37). A native settings tab (not a frame), admins, every host
   (the machine is spawned on every web host, not only the desktop app):
   - lists managed engines (from the inventory and This Machine's status), unmanaged
     endpoints and Cloud providers, each with its kind, state, Expose on Migo Bridge and (for
     non-LLM kinds) Expose as tool with a default per kind;
   - add, edit and remove unmanaged endpoints and Cloud providers (kind, name, URL, API, key,
     models or Engine default), with **Check** (a probe of `/models` or the health path);
   - managed engines link to This Machine (Inventory, Engine store) for the deep edits;
   - `/api/machine/v1` lists the endpoints' models and passes a request for one of them to its
     endpoint, with its key.
   This Machine and Migo Bridge stay as they are. Two choices are kept but not yet acted on,
   and the page says so: Expose on Migo Bridge for an unmanaged engine (Migo Bridge offers
   endpoints with the local Sothoth, phase 6), and Expose as tool (phase 4).

   Shipped in 0.16.37: `machine/endpoints.json`, `tools.json`, `secrets/`, the `endpoints` map
   in `share.json`; `/api/machine/local-engines`, `PUT|DELETE /api/machine/endpoints/<id>`,
   `POST /api/machine/endpoints/check`, `PUT /api/machine/tools`; `machine` in `/api/info`;
   the machine on every `--hostui` host (`--hostui-no-machine` turns it off).
2. **Cloud on the Sothoth Router.** The gateway routes `[upstreams.*]` of kind `openai` and
   `anthropic` as virtual cards (as the Claude engine's virtual card does): a deployment per
   listed model, placement, failover, `/v1/models`, leases and the request log as for any card.
   Anthropic goes through its OpenAI-compatible endpoint (`/v1/chat/completions`). The router
   admin's Inventory gets a **Cloud** group beside Servers & cards and Migo machines: add a
   provider (kind, base URL, key into `[secrets]`, models, prices, limits).
3. **Migration** (on the first load of a version with Local Engines; nothing is deleted):
   - server mode's `lmStudio.baseUrl`, `apiKey`, `models` → an unmanaged `llm` endpoint
     `llm-server` (Engine default when `models` is empty), the default LLM engine;
   - each `providers.<name>` (other than the synthesized `lmstudio`) → an endpoint (`cloud` when
     its URL is not on this PC or the LAN, else `unmanaged`);
   - `imageGen.baseUrl`, `apiKey`, `model` → an unmanaged `image` endpoint `image-server`
     (`openai-images`), exposed as the `generate_image` tool when `imageGen.enabled`;
   - the connection becomes `local` unless it was `router`.
   The old keys stay in config.json, kept as a copy of the default LLM engine (and image
   engine), so an older version still works. Tested against real-shaped placeholder configs.
4. **Tools.** `generate_image`, `speak`, `transcribe`, `make_music`, one per kind, each sent to
   its kind's default engine, or to the engine serving the `model` named in the call. In
   router mode they go to the Sothoth Router's media routes; in Local Engines mode to the
   machine (`/api/machine/v1/images|audio/*`).
5. **Images tab removed.** `imageGen.enabled` → Expose as tool on the image engine;
   `imageGen.timeoutSeconds` → the engine's timeout; `agent.uncensored.imageModel` → the image
   engine's "model while /uncensored is on"; `imageGen.outputDirectory` → Agent & tools,
   *Image tool*. `#settings/images` opens Local Engines.
6. **A local Sothoth.** See 4.

## 4. The local Sothoth (phase 6)

How close it is today:

- `--with-sothoth` (together.rs) already runs the whole router in this process on its own
  thread and runtime, with self-healing restarts. It listens on ports and reads its own
  `router.json` / `router.toml`.
- This Machine already runs the router **in-process for its admin pages** (MACHINE.md 2.3), over
  `machine/router.toml`, with no socket, and places local requests with the router's own
  `inventory::place` (`/api/machine/v1`). Its engines run under Migo Bridge's supervisor, not
  the router's drivers (those are Linux shell templates).
- The router's media routes (speech, transcription, music, images, ComfyUI) already place on
  **bridged** machines through the bridge protocol.

So the local Sothoth is: the in-process router of This Machine serving its gateway too (no
socket: the web host calls it as it calls the admin pages), with the PC's own supervisor
connected to it **as a bridge** over an in-process channel instead of a WebSocket. Every
engine then goes through the code a real router uses: LLM placement and failover, the media
adapters (ComfyUI workflows, Wyoming, Stable Audio), Cloud upstreams (phase 2) and unmanaged
endpoints, leases, `/v1/models`. `/api/machine/v1` becomes a thin pass-through to it, and the
agent's Local Engines connection uses it.

Work: an in-process bridge transport (the bridge client and hub speak messages; a channel pair
replaces the socket), a gateway entry for the in-process app, and the overlay of the PC's
bridge into the local snapshot. That is a release of its own after phases 1-5.

## 5. Risks

- **Two sources for a model list** (the inventory's models and an endpoint's models): an id in
  both is refused by the check, with the place it is already used.
- **Keys**: written once to `secrets/<id>` (mode 600 where the OS has modes), never returned;
  the page shows whether one is set. Remote config (MACHINE.md 8) may not read or write them.
- **Relayed endpoints** (an unmanaged endpoint on another machine exposed through Migo Bridge)
  carry that machine's traffic through this PC: off by default, and labelled.
