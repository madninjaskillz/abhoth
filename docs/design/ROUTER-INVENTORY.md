# The router inventory (D152)

User request (2026-11-26): redesign the router admin around an inventory. A **server** (how the
router reaches it, its model folder) has **cards** (GPU, VRAM); each card says which **engines**
may run each **kind of model** (language, text to speech, speech to text, images, music,
embeddings). There is an **engine catalog**. Models of every kind list the engines that can run
them, with one **primary**, per-engine launch settings, and the servers that have their files.
Setting it up is guided (add a server → find its GPUs → tick engines per kind → add models), in
plain language, with raw editors under Advanced, and works at phone width. Models come from
**Hugging Face** (search, pre-filled details, pick a file, the router downloads it to the
server). The router **picks the card**.

A follow-up asked for **capability-driven engines**: engines say what they can run (kinds,
formats, architectures, quantisations, limits) from a live probe, a built-in catalog and the
user's overrides; engines can inherit from another ("my llama.cpp fork works like llama.cpp");
the engines that can run a model are **computed**, never typed in. A new fork runs every model
it matches with no model edits.

## 1. The data model (`router.toml`)

Everything is additive: a file from before D152 loads unchanged, and section 6 converts it.

```toml
[inventory]
version = 1                      # written by the migration (section 6)

[hf]                             # optional: Hugging Face
token = "secret:hf"              # optional; else <state_dir>/secrets/hf-token (the admin pages)
endpoint = "https://huggingface.co"   # tests point this at a mock

[servers.gpu]
model_dir = "/srv/models"        # downloads go to <model_dir>/<owner>/<name>/

[cards.A]
engines = { llm = ["ninfer", "llamacpp"], tts = ["kokoro"], embedding = ["*"] }

[engines.llamacpp-fork]          # launch settings as before (driver, image, ports, profiles…)
caps = { inherits = "llamacpp", add = { archs = ["qwen3next"] }, max_context = 262144 }

[models.qwen3-8b]
kind = "llm"                     # llm | tts | stt | image | music | embedding (default llm)
format = "gguf"                  # usually worked out from the file name
arch = "qwen3"                   # from the HF card / config.json / GGUF general.architecture
quant = "Q4_K_M"
primary = "llamacpp"             # tried first
prefer = ["llamacpp-fork"]       # then these, in order (optional)
allow = []                       # exceptions: engines allowed although their capabilities say no
deny = []                        # engines never used for this model
servers = ["gpu"]                # servers with the files (unset: every server)
context = 40960
size_gib = 5.2
licence = "apache-2.0"
source = { hf = "example/Qwen3-8B-GGUF", revision = "…", files = ["Qwen3-8B-Q4_K_M.gguf"] }
[models.qwen3-8b.artifacts.llamacpp]          # per-engine launch settings, as before
file = "/srv/models/example/Qwen3-8B-GGUF/Qwen3-8B-Q4_K_M.gguf"
vram_gib = 6.0
```

- **Card `engines`**: per kind, the engines allowed on the card (`"*"` = any engine able to run
  that kind). Unset keeps the old rule: language models on any engine with a port for the card.
- **Artifacts stay** as the per-engine launch settings (file, VRAM, profiles, options, restore).
  An artifact may also say `format`, `arch`, `quant` when its file name does not.
- New validation: `model_dir` must be absolute with no `..` (error); card `engines` keys must be
  kinds (error), unknown engines in them, `prefer`, `allow`, `deny` are warnings; a primary that
  cannot run the model is a warning; an artifact for an unknown engine is now a warning (was an
  error); a route with `layout_from = "auto"` where no card can run the model is a warning.

## 2. Engine capabilities

`control/caps.rs`. What an engine can run is three layers, later ones winning:

1. **The built-in catalog** (`CATALOG`): `llamacpp` (llm + embedding, gguf, any arch), `strata`
   (llm, its own model), `ninfer` (llm, `.ninfer`), `vllm` (llm + embedding, safetensors),
   `exllama` (exl2), `maya` (tts, safetensors), `kokoro` (tts, onnx/pytorch/safetensors),
   `whisper` (stt, ggml/gguf), `sa3` (music), `qwen-image` (image). An engine whose id is a
   catalog kind starts from it; any other starts from nothing until it says `inherits`.
2. **A live probe** of a running engine: `GET /props` (llama.cpp: build, architecture, context)
   or `GET /v1/models`, sent with `x-abhoth-no-switch`. Read-only: never a completion, a load or
   a switch. The router asks only ready, idle cards (no request in flight), at most every
   10 minutes, and only when the cache has nothing for this engine's **stamp** (image + build;
   a new binary or version probes again) or the entry is 6 hours old. Results are kept in
   `<state_dir>/engine-probes.json`. A probe adds evidence (architectures seen, version,
   context); it never removes what the catalog says. `caps = { probe = false }` turns it off.
3. **Overrides** (`[engines.x] caps = { … }`): `inherits` (a catalog kind or another engine,
   followed up to 8 deep, cycles stop), `kinds` / `formats` / `archs` / `quants` (replace),
   `add = { … }` and `remove = { … }` (deltas), `max_context`.

The view lists each engine's effective capabilities with their **sources**
(`catalog:llamacpp`, `engine:llamacpp`, `probe`, `override`).

**Matching** (`caps::supported`): an engine runs a model's file when it runs the model's kind,
takes the file's format (from the artifact, the model, or the file name: `.gguf`, `.safetensors`,
`.onnx`, `.ninfer`, `.exl2`…), lists the architecture (or lists none = any) and the quant.
Each engine gets a one-line **reason**: `gguf + qwen3 ✓`, `no exl2 support`,
`does not run text to speech`, `qwen3next not in its architectures`. Then the model's
exceptions: `deny` wins; `allow` forces a yes. One more rule keeps old configs working: an
engine that has its **own artifact** for the model counts as able to run it unless its known
capabilities contradict that (`via = "settings"`; this is how Strata and other legacy entries
stay as they were without `allow` lines).

**Order** (`caps::engine_order`): the primary, then `prefer`, then engines with their own
artifact (file order), then any other capability match. A capability match with no artifact of
its own borrows a matching file from another engine's artifact (the fork case).

**The fork example** (tested): add `[engines.llamacpp-fork]` with `caps = { inherits =
"llamacpp" }` and a port; every GGUF model llama.cpp could run now lists the fork
(`gguf + qwen3 ✓`) and can be placed on it, with no model edits.

## 3. Placement: the router picks the card

`control/inventory.rs` `place(config, model, pin)`. For each engine in the model's order and
each card:

- the card is managed and allowed on demand, and in the route's `cards` if it names any;
- the engine has a port for the card, and a file for the model (its own artifact or a matching
  one);
- **the card's server has the files** (`models.x.servers`), or an archive restore can bring them;
- **the card allows the engine for the model's kind** (card `engines`);
- **it fits**: the artifact's VRAM plus the profile's, against the card's VRAM less its reserve,
  trying the pin's profile, a saved layout's profile (presets), the artifact's default, its
  profiles, then the engine's.

The answer is a list of card options (usable first, then by rank: 0 = the primary engine) with
the reasons a card was refused. On-demand routes with `on_demand = { layout_from = "auto" }`
(or with no `on_demand` block) get a per-card pick from this list; `decide()` takes the best
rank first, then the card idle longest. **Layouts stay** as optional presets, and a route's
`pick` stays a **pin** (its engine and profile win; a refusal becomes a warning). All guards are
unchanged: `x-abhoth-no-switch`, leases, busy cards, min-stay, exclusions, schedule and file
guards. A model with nowhere to run is blocked with a clear reason instead of trying.

## 4. Hugging Face and downloads

- **API calls are made by the router** (`control/hf.rs`): search (`/api/models?search=…`, an
  optional `pipeline_tag` per kind), model details (`/api/models/<repo>?blobs=true` plus its
  `config.json`). The details pre-fill the **kind** (from `pipeline_tag`), **architecture**
  (the GGUF metadata or `config.json` `model_type`), **context**, **vision** (an `mmproj`,
  `vision_config`, or `image-text-to-text`), **licence** and **gated** status (shown clearly; a
  gated model needs its licence accepted on huggingface.co and a token).
- **File choices**: one per GGUF quant (all its shards plus the smallest `mmproj`), one
  safetensors set (shards plus the small side files), else "all files". Each has its size and
  a **VRAM need** (weights × 1.1); the admin API adds **fit hints** per managed card.
- **Downloads run on the target server** through its executor (local, or SSH to `hostctl`):
  the `download` builtin fetches one file to `<model_dir>/<owner>/<name>/<file>` with a
  `.part` file and HTTP `Range`, so a stopped or timed-out download resumes; the SHA-256 from
  the listing is checked when known; `download-status` reports progress, polled every 2 s.
  Files run in order, timeouts and interruptions retry up to 20 times. Cancel keeps the part.
  Jobs are kept in `<state_dir>/downloads.json` (no tokens); a router restart marks running
  ones stopped (Resume carries on).
- **When a download finishes**, the model is marked present on that server
  (`models.x.servers` += server) through the normal audited save, so placement can use it.
- **The token**: `PUT /admin/api/hf/token` writes `<state_dir>/secrets/hf-token` (mode 600);
  `[hf] token = "secret:…"` also works. It is never in `router.toml`, never returned (the API
  says only whether one is set), never logged or audited by value, and reaches the server only
  on the builtin's **stdin** (the executor audit records neither stdin nor args).
- `hostctl` allows `download` only for `https://huggingface.co/` and `https://hf.co/` URLs and
  only into its write paths; `suggested_paths` adds each server's `model_dir`.

## 5. The admin pages

A new **Inventory** tab, first in the in-app Router config pane and second in the standalone
`/admin/` (after Overview). It has four parts:

- **Servers & cards**: each server with its model folder and cards. **Add server** (name, SSH or
  local, host, key reference, host key, model folder), **Find GPUs** (the `telemetry` builtin =
  `nvidia-smi` on the server; new GPUs are added as cards by UUID with the next free letter,
  existing ones show their card), **Engines…** per card (ticks per kind; only engines able to run
  that kind are offered).
- **Engines**: each engine's kinds, formats, cards and probe state; its **capability page**
  (effective capabilities, sources, the probe's findings, models it runs, overrides with
  `inherits` / add / remove / probe on-off) and **Add a fork** (copies the launch settings, sets
  `caps.inherits`).
- **Models**: grouped by kind, with format / architecture / quant, the primary and where the
  router would put it now. A model's page lists **every engine with its reason**, the primary
  (radio), the order of the others (up/down), `allow anyway` / `never`, and the card-by-card
  placement with why not. **Add from Hugging Face**: search (with a kind filter), details, file
  choice with sizes and fit per card, the server to download to, the primary engine (only those
  that can run the file), the model name; **Add and download** saves the catalog entry (review
  → save) and starts the download.
- **Downloads**: progress, Stop, Resume, remove.

A guide bar shows the next setup step (server → GPUs → engine → model) until all four exist.
Every change is the existing **edit → review (diff, problems) → audited save** flow; raw
`router.toml` stays under Advanced. Admin API: `GET inventory`, `GET catalog`,
`POST inventory/detect|gguf|place`, `GET hf/search|model`, `GET|PUT|DELETE hf/token`,
`GET|POST downloads`, `POST downloads/<id>/cancel|resume`, `DELETE downloads/<id>`.

## 6. Migration

At start-up (`control::startup::start`, before anything else) a `router.toml` without
`[inventory]` is converted **in place, losslessly**:

- each card: `engines = { llm = [engines that run LLMs and have a port on the card] }` (the old
  rule made explicit);
- each model: `kind = "llm"`, `primary` = its first artifact's engine, `prefer` = the other
  artifact engines in order (when there are several), `allow` only where an artifact's engine
  is not otherwise supported (none on DeepThought: the settings rule covers Strata),
  `servers` = every server;
- `[inventory] version = 1`.

It is applied with `toml_edit` (comments and layout kept), checked with the router's own loader
and validator (it refuses only errors the migration itself would add), the old file is backed up
to `<state_dir>/backups/router.toml.pre-inventory-<UTC time>` (mode 600), and the new one is
written atomically with the same permissions. Included files are not touched. On any failure
nothing changes and the router runs on the old file the old way. Placement and on-demand picks
are identical before and after (tested on a copy of DeepThought's live file: 28 edits, no new
problems, the same five on-demand models with the same engines and profiles).

**Rolling back to an older router** needs the backed-up file: `router.toml`'s schema denies
unknown fields, so 0.16.13 refuses `[inventory]`, `engines`, `kind` and friends.

## 7. Limits

- New models and on-demand routes need a **router restart** to be switchable on demand (the
  control plane captures its config at start). The Inventory tab says so when it applies.
- A server whose `hostctl` runs in **strict mode** needs its allowlist re-exported (and the new
  `abhoth` on the host) before `download`, `download-status` and `gguf-meta` work there.
- Probes see only running engines; an engine never started is described by its catalog entry
  and overrides alone.
- VRAM fit is an estimate (weights × 1.1 plus the profile's VRAM); the engine's real use wins.
- No live Hugging Face calls in tests: everything runs against mocks.
