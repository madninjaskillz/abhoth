# Media engines under the router (D153)

Status (2026-11-26): **decided (section 0); phase 1 built.** It is based on a read-only survey of DeepThought
and the user's PC, made at 22:40-23:00 UK on 2026-11-26.

User request: "the comfyui models and other models - and relevant engines, should be managed by
abhoth". This means images, music, speech (TTS) and transcription (STT). Two follow-ups settled
the main choices:

- **Lifecycle (c):** the router starts and stops these engines on cards **and** routes and
  proxies their requests. "I may add this PC as a ComfyUI box too … abhoth will choose if it
  comes here or to the server."
- **One port:** everything goes on the router's existing port (`:8010`) under different paths.
  Extra ports are added only where a client truly needs root paths, and each one is listed
  with its reason (section 4.6).

This document builds on [ROUTER-INVENTORY.md](ROUTER-INVENTORY.md) (D152: servers, cards,
engines with capabilities, models with kind, format and architecture, placement, Hugging Face).
It also builds on SUPERVISOR.md 7 (media engines: `api`, `lifecycle`, `depends_on`,
`conflicts`). Section 1.4 corrects some facts that SUPERVISOR.md 7 states.

---

## 0. Decisions (2026-11-26, D153)

The user answered the questions in the first draft. Section 9 keeps those questions for the
record.

| Topic | Decision |
|---|---|
| Lifecycle | Option (c). The router starts and stops media engines on cards **and** routes and proxies their requests. Everything goes on the router's own port, under paths: `/v1/images/*`, `/v1/audio/*`, `/v1/audio/music` and `/comfyui/*`. |
| Cards | Fixed. Images and video go on X2, speech on X1, music on DeepThought's **CPU card**, and A and B stay LLM-only. These are the default `engines` lists of those cards. |
| CPU cards | A CPU is a card too (`device = "cpu"`, with `cores` and `ram_gib`), and placement fits against its RAM. DeepThought's CPU hosts SA3 the way X1 hosts speech. Detection reports the CPU and RAM through a new `cpu-info` builtin. Old configs load unchanged. |
| TrueNAS apps | The router runs **its own containers**. The apps (comfyui, image-api, stable-audio, jarvis-tts, jarvis-stt) are retired: stopped and kept, with their images, data and config never deleted. Each is backed up and has its own rollback. |
| Engines | Described by **engine packages** (section 10): a JSON manifest that names one or more install and run methods (compose, docker, pip, uv, conda, binary, git, existing). Manual engines stay first-class, and nothing in placement or routing depends on a package. |
| Store | A package index (default: `stores/` in the repo), an Engine store tab, and a Create package wizard. Image digests and hashes are pinned. Privileged mode, host network, host mounts and host-run methods each need approval. |
| Bridges | A headless `abhoth bridge` runs in a Docker image with no Claude token. It **dials the router** over a token-authenticated WebSocket, with reconnect and backoff. It describes its own server, cards (GPU and CPU) and installed engines in a versioned descriptor, and starts and stops engines on its host for the router. In the router admin, bridge servers are read-only ("managed by bridge X"); the admin can disable one or set its priority. |
| PC | Used only while its Abhoth runs in bridge mode. Never woken over the network. |
| Images | DeepThought first. The PC takes overflow only, when DeepThought is busy or down. |
| ComfyUI page | Served through the router at `/comfyui/` behind a login. `:8188` closes to the LAN once the router's own container binds to loopback. |
| Outputs | Streamed only. Sticky routing sends follow-up requests back to the server that made the output. |
| Kokoro | Becomes a router engine (TTS on X1). The video pipelines move to `/v1/audio/speech`. |
| Sync | Automatic and in the background: checksummed, bandwidth-limited, and never fills a disk. It leaves headroom and asks an admin when space is short. Sync state shows in the admin. |
| Video | Added as a kind now, with no video engine yet. |
| truenas.local | A **speech backup** only: Chatterbox, whisper and Kokoro on its M4000, through a bridge container. That container is a custom app with only the M4000, storage at `<pool>/apps/abhoth-bridge/{models,state}`, and limits of 4 CPUs and 6 GiB. Failover engines start on demand when X1 or the CPU card is busy or down, and stop after 10 minutes idle. Music has no backup: the host has 31 GiB RAM with about 2 GiB free. wyoming-piper stays outside the router. |

### truenas.local (survey, read only)

- Address and OS: truenas.local, TrueNAS 26.0.0-BETA.2.
- CPU: Ryzen 7 5700X, 8 cores / 16 threads, AVX2.
- RAM: 31 GiB, about 2 GiB available. Two VMs, Frigate and Jellyfin use the rest.
- GPU: Quadro M4000 8 GB, driver 570.172. Frigate's ffmpeg uses about 0.5 GiB of it. Docker's
  default runtime is nvidia.
- Disk: `Pool` has 322 GiB free, `Backup` 1.44 TiB free.
- Already running there:
  - wyoming-piper and wyoming-tts-pad (`:10200`);
  - SearXNG (`:8088`), the video pipelines' research search;
  - Frigate;
  - the Home Assistant VM;
  - a stopped `comfyui` app.

## 1. What exists today (survey)

### 1.1 Cards

The table uses the card names in the live `router.onedocker.toml`. SUPERVISOR.md calls X1 and
X2 "D" and "C".

| Card | GPU | Bus | VRAM used now | Router config | Who is on it |
|---|---|---|---|---|---|
| A | V100-SXM3-32GB | 21:00.0 | 32.3 / 32 GiB | managed, on demand, `llm = [ninfer, llamacpp, strata]` | `ninfer-a` (LLM) |
| B | V100-PCIe-32GB | 01:00.0 | 32.3 / 32 GiB | managed, on demand, same | `ninfer-b` (LLM) |
| X1 | Quadro M4000 8 GB (Maxwell, sm_52) | 44:00.0 | 4.7 / 8 GiB | `managed = false`, `llm = []` | jarvis-stt (whisper-torch, 1.2 GiB); jarvis-tts (Chatterbox nano, 3.5 GiB); **the video pipeline's Kokoro TTS and faster-whisper QA**, which run briefly during the daily run (`TTS_GPU=2`) |
| X2 | V100-SXM2-16GB | E1:00.0 | 11.1 / 16 GiB | `managed = false`, `llm = []` | ComfyUI (models stay resident; `--reserve-vram 1.5`) |
| PC (not a server yet) | RTX 3070 Ti 8 GB | — | 5.1 / 8 GiB (desktop) | — | Comfy Desktop v0.34 (offline at survey time) |

### 1.2 Services

| Service | How it runs | Card | API (port) | Models | Users | Starts/stops |
|---|---|---|---|---|---|---|
| **ComfyUI** 0.39.0 | TrueNAS app `comfyui`, container `ix-comfyui-comfyui-1`, `python:3.12-slim`, `sh /scripts/comfyui-start.sh` (on every start: apt-get, git clone if missing, pip installs, rewrites `extra_model_paths.yaml`, so a **cold start takes minutes**); DeviceRequest pinned by UUID | X2 | `:8188` (0.0.0.0, **no auth**): `/prompt`, `/queue`, `/history`, `/view`, `/upload/image`, `/ws`, `/system_stats`, `/object_info` (2.0 MB), `/models`, `/models/<type>` | 22 GiB, see 1.3 | image-api shim; direct browser use of the UI | `midclt app.stop/start comfyui`; `restart: unless-stopped` |
| **image-api shim** (`comfy-openai-shim.py`) | TrueNAS app `image-api`, `ix-image-api-openai-shim-1`, CPU only, stdlib Python | none | `:8189` `POST /v1/images/generations`, `POST /v1/images/edits`, `GET /v1/models`, `GET /health`, `/backends`, `POST /backends/<name>` (drain) | Maps names to workflows: `qwen-image` (Qwen-Image-2.1 Turbo Q4_K_M GGUF, 8 steps) and `qwen-image-uncensored` (step-90000 GGUF, heretic encoder, Turbo LoRA; truenas only). `juggernaut`, `bigasp` and `chroma` are **legacy names redirected to qwen-image**: there is no SDXL or Chroma anywhere. | video pipeline `images.py`; imagegen MCP on the PC (`IMAGEGEN_SHIM_URL=http://deepthought:8189`); OpenOrca image tool; Open WebUI; Jarvis | app stop/start |
| (in the shim) **load balancing** | already spreads jobs over two ComfyUI backends: `truenas` (`deepthought:8188`, pref 1) and `pc` (`the-pc:8188`, pref 0). It checks health with `/system_stats` (cached 10 s), cools off a failed backend for 30 s, retries once on the other, waits for an idle backend (shim in-flight 0 and an empty ComfyUI queue), has a RAM guard, sends edits only to truenas, and `name@pc` pins a request | — | — | — | — | — |
| **Stable Audio 3** | TrueNAS app `stable-audio`, `ix-stable-audio-sa3-1`, `local/sa3:1`, `runtime=nvidia` but **no GPU device or `NVIDIA_VISIBLE_DEVICES`**, so it runs **on the CPU** ("stable-audio-3-medium (tflite, cpu)", 24 threads, `nice 10`) | **none (CPU)** | `:8095` `GET /health`, `POST /generate` (FIFO, max queue 6, max 190 s, one subprocess per job) | `stabilityai/stable-audio-3-optimized`, 2.5 GiB HF cache in `<ai-root>/music/hfcache` | video pipeline `music.py` (`SA3_URL` hard-coded); the sa3-music skill | app stop/start |
| **jarvis-tts** | TrueNAS app `jarvis-tts`, `local/tts-nano:1.3`, Chatterbox Turbo **nano** (`ResembleAI/chatterbox-nano`), voice `jarvis-bettany-a` | X1 | Wyoming `:10203`; HTTP `:10204` `/health`, `/voices`, `/synth`, `/stats`, `/reload` | 2.8 GiB HF cache plus 4.9 MiB of voices in `<ai-root>/tts-nano` | Home Assistant (Wyoming), Jarvis | app stop/start |
| **jarvis-stt** | TrueNAS app `jarvis-stt`, `local/whisper-torch:1.1`, `futo-org/acft-whisper-small.en` fp32 | X1 | Wyoming `:10300` only (no HTTP API) | 3.4 GiB in `<ai-root>/whisper-torch/hf` (acft-small.en, whisper-small.en, whisper-large-v3-turbo) | Home Assistant, Jarvis | app stop/start |
| **Kokoro TTS** (video pipeline) | **not a service**: runs in-process in `run_daily.sh` → `pipeline/tts.py` (`.venv-tts`, `KPipeline("hexgrad/Kokoro-82M")`), `CUDA_VISIBLE_DEVICES=2` (PCI order = M4000) | X1 (briefly) | none | `<ai-root>/vp-tools/hf` (2.2 GiB: Kokoro-82M, faster-whisper small.en and medium.en for TTS QA) | AI News, Game News, Car News (voices am_michael, af_bella, bm_fable) | per run |
| **ai-news-render** | TrueNAS app, Playwright container, `runtime=nvidia` with no device list, 48 GiB memory limit, `render_worker.sh` | none in effect | none (file queue) | — | video pipeline render step | app |
| wyoming-whisper | TrueNAS app, **stopped** (1.6 GiB faster-whisper large-v3-turbo ct2 in `<apps-root>/wyoming-whisper`) | — | — | — | replaced by jarvis-stt | — |
| **Maya TTS** | **not found**: no container, app, process or folder on DeepThought (a depth-3 search of `<ai-root>` for `*maya*` found nothing). It exists only as a catalog entry in `caps.rs`. | — | — | — | — | — |
| **Embeddings** | **none running** (no TEI, embedding container or route) | — | — | — | — | — |
| Other GPU-adjacent | `voca-poc` (29 GiB of training data, checkpoints and HF cache; a research POC, not a service); `<ai-root>/music/models/acepyroot` and build logs for ACE-Step, Levo, MusicGen3 and YuE2 (earlier music experiments, no service) | — | — | — | — | — |

The video pipelines (`<ai-root>/videoPipeline`, 07:00 daily for AI News and Game News,
plus Car News on Sundays) do research, then claims, images, TTS, music, render and upload. They
call `:8010/v1` (router, LLM), `:8189` (images) and `:8095` (music), and run Kokoro and
faster-whisper locally on X1. The router already protects these runs: the calendar entries
`ai-news`, `game-news` and `car-news` (`block_switch` and `block_on_demand` on the server,
detected through the process `run_daily.sh`), the `guards.video` process guard and the
06:45-07:30 quiet window.

### 1.3 Model files

**DeepThought ComfyUI** (`<apps-root>/comfyui-truenas/models` → `/models`, 22 GiB; mapped by
`extra_model_paths.yaml` key `truenas`, which `comfyui-start.sh` rewrites on every start):

| Type folder | Size | Files |
|---|---|---|
| `unet` (`diffusion_models`; GGUF through ComfyUI-GGUF as `unet_gguf`) | 8.1 GiB | `qwen-image-2.1-turbo-Q4_K_M.gguf`, `qwen-image-2.1-step90000-Q4_K_M.gguf` |
| `clip` (text encoders) | 12 GiB | `qwen3vl_8b_w4a8.safetensors`, `qwen3vl_8b_w4a8_heretic.safetensors` |
| `vae` | 645 MiB | `qwen_image_2.1_vae_bf16.safetensors` |
| `loras` | 641 MiB | `chatrlabs-omni-turbo-8step-fp32.safetensors` |
| `upscale_models` | 128 MiB | `4x-UltraSharp.pth`, `RealESRGAN_x4plus.pth` |
| `background_removal` | 421 MiB | `birefnet.safetensors` |
| `checkpoints`, `controlnet`, `ipadapter`, `clip_vision` | empty | — |
| custom nodes | — | ComfyUI-GGUF, ComfyUI_IPAdapter_plus, comfyui_controlnet_aux, Manager |
| `data/` (code and venv) | 6.6 GiB | ComfyUI 1.2 GiB, venv 5.4 GiB |

**PC Comfy Desktop** (`%LOCALAPPDATA%\Comfy-Desktop`). It has **two** model roots:

- install `ComfyUI-Installs\ComfyUI\ComfyUI\models`: `diffusion_models/qwen-image-2.1-turbo-Q4_K_M.gguf`,
  `qwen_image_2.1-Q4_K.gguf`, `text_encoders/qwen3vl_8b_w4a8.safetensors`,
  `vae/qwen_image_2.1_vae_bf16.safetensors`;
- shared `ComfyUI-Shared\models` (the default in `instance-model-paths/*.yaml`, generated, "do
  not edit"): `diffusion_models/z_image_turbo_bf16.safetensors` (12 GiB),
  `text_encoders/qwen_3_4b.safetensors` (7.5 GiB), `vae/ae.safetensors` (320 MiB).

The PC lacks the uncensored set, which is why the shim restricts that model to truenas.

**Other media files on DeepThought:** SA3 2.5 GiB; Chatterbox nano 2.8 GiB; whisper-torch
3.4 GiB; vp-tools (Kokoro and faster-whisper) 2.2 GiB; wyoming-whisper (stopped) 1.6 GiB.

### 1.4 Corrections to the design docs

- SUPERVISOR.md 7 puts SA3 on card B with `conflicts = ["layout:*@B"]`. In fact **SA3 uses no
  GPU** (tflite on the CPU). `guards.music` (`/run/qfn-musicmode.state`) is now legacy; a
  CPU-heavy SA3 job competes only for CPU and RAM (SA3 itself refuses work below
  `SA3_MIN_AVAIL_GB=12`).
- "SDXL/Chroma" exist only as legacy names; every image request runs Qwen-Image-2.1.
- Maya is not deployed, and nothing on DeepThought serves embeddings.
- Card X1 is the **speech card**. It is shared, without coordination, by two standing TTS/STT
  services and the pipeline's in-process Kokoro. It has 8 GiB, 3.3 GiB of which is free at
  idle.

### 1.5 How the router relates today

- X1 and X2 are discovered, `managed = false`, `on_demand = false`, `engines = { llm = [] }`,
  so on-demand switching **never touches them**, and the media apps never collide with layouts.
- The router knows nothing about media engines, routes or models. It proxies only chat,
  `/v1/models` and similar endpoints. Its `dispatch` **reads the whole body into memory**
  (`axum::body::to_bytes`) and has **no WebSocket or streaming-upload path**, which a ComfyUI
  pass-through needs (section 4.4).
- The executor is local or SSH to `hostctl`. hostd is Linux-only (NVML, AF_PACKET sniff).
  Nothing runs on Windows.
- gpu-power and hostd sniff `:8188` to wake X2 from its idle clock-down. Traffic through the
  router should send a `wake {card}` instead (SUPERVISOR.md 6.3).

---

## 2. Fitting media into the inventory model

### 2.1 Engines

| Engine id | Kind(s) | Formats | Archs (from probe or catalog) | Probe | Driver |
|---|---|---|---|---|---|
| `comfyui` (new catalog entry) | `image` (+ `video` later, + `upscale`) | `safetensors`, `gguf` (with ComfyUI-GGUF), `ckpt`, `pth` | `qwen-image`, `z-image`, `sdxl`, `sd15`, `flux`, `chroma`, `wan` (video), `hunyuan-video`, `ltxv`… probed | `comfyui`: `GET /system_stats` (version, VRAM), `GET /models` and `/models/<type>` (files present), `GET /object_info` (installed loader nodes, so `UnetLoaderGGUF` means gguf is supported) | `compose` (TrueNAS app) on DeepThought; `windows` on the PC (section 5) |
| `sa3` | `music` | `tflite`, `safetensors` | `stable-audio` | `sa3`: `GET /health` (model, queue, running) | `compose`; **runs on the CPU (card none)** |
| `chatterbox` (new) | `tts` | `safetensors` | `chatterbox`, `chatterbox-nano` | `http:/health` + `/voices` | `compose` |
| `kokoro` (exists) | `tts` | onnx / pytorch / safetensors | `kokoro` | none | new small service (section 6.3) |
| `whisper-torch` (new, `inherits = "whisper"` + `formats += safetensors`) | `stt` | `safetensors`, `ct2` | `whisper` | Wyoming `describe` (via a TCP probe) | `compose` |
| `image-api` | none: the CPU shim. **It is retired into the router** (section 4.2) | — | — | — | — |

A ComfyUI engine's **effective capabilities are mostly probe-driven**. `/object_info` lists every
loader node and, for each, the enumerated file names it accepts. That is the router's live view
of which architectures and files a given ComfyUI box can run. It is read-only, cached by stamp
(ComfyUI version plus custom-node list), and taken only while the queue is empty.

### 2.2 Models and components

ComfyUI has no "model" the way an LLM engine does. A run is a **workflow** that loads several
files. The plan therefore adds two things:

1. **Component models**: every file a workflow loads becomes a model entry with `kind` and a
   **`role`** (new field). The role values are `diffusion` (unet/diffusion_models), `checkpoint`
   (all-in-one), `text_encoder` (clip), `vae`, `lora`, `controlnet`, `ipadapter`, `clip_vision`,
   `upscale`, `background_removal`, `embedding_ti`, `audio_encoder` and so on. Each role maps
   to a **ComfyUI folder** (a fixed table that also takes the old names `unet`/`clip`), which
   decides where a download goes. Components carry `arch` (a LoRA's `arch = "qwen-image"`
   means it fits only that base) and `servers`. They are **not routable** by themselves.
2. **Recipes** (routable models, `kind = "image"`): a named, user-facing model such as
   `qwen-image` that lists the components it needs and a workflow **template**. It is what the
   shim's `MODELS` table holds today:

```toml
[models.qwen-image]
kind      = "image"
arch      = "qwen-image"
primary   = "comfyui"
title     = "Qwen-Image 2.1 Turbo"
recipe    = { workflow = "workflows/qwen21-turbo.json", steps = 8, cfg = 1.0,
              sigmas = "1.0, 0.978453, …", sizes = "1mp-buckets", edits = true }
needs     = ["qwen21-turbo-q4", "qwen3vl-8b-w4a8", "qwen21-vae"]   # component model ids
aliases   = ["qwen", "qwen-image-2.1", "qwen-image-turbo", "juggernaut", "bigasp", "chroma"]

[models.qwen21-turbo-q4]
kind = "image"; role = "diffusion"; format = "gguf"; arch = "qwen-image"; quant = "Q4_K_M"
size_gib = 3.9; servers = ["deepthought", "pc"]
source = { hf = "…", files = ["qwen-image-2.1-turbo-Q4_K_M.gguf"] }
```

- **Placement** for a recipe: a server and card qualify when the engine is allowed for `image`
  on that card, the card is up (or can be started), and **every component in `needs` is
  present on that server**. Presence is checked live through the probe's `/models/<type>` and
  falls back to the `servers` lists. The check gives a reason per server, for example
  "pc: missing qwen3vl_8b_w4a8_heretic.safetensors". This replaces the shim's hand-written
  `"backends": ["truenas"]`.
- **VRAM** for a recipe is the sum of its components times 1.1, unless measured. On X2 the
  observed figure is 11 GiB resident.
- Music, TTS and STT models are single files or HF repos and fit the existing `Model` as is
  (`kind = "music" | "tts" | "stt"`). TTS voices are a sub-list (`voices = […]`) on the
  model, filled by the engine's `/voices` probe.

### 2.3 New kinds

- Add **`video`** to `ModelKind` now (Wan, LTXV, Hunyuan through ComfyUI). It costs little and
  avoids a later migration. No video recipe exists yet.
- **Do not** make lora or vae kinds of their own. They are `kind = "image"` (or `video`) with
  a `role`, so a card's `engines.image` covers them.
- Optional later: `upscale` as a recipe (image to image) rather than a kind.

### 2.4 Shared ComfyUI model root and downloads

- Each ComfyUI server gets a `comfy_root` (the server's `model_dir` plus `/comfyui`, or an
  explicit path). On DeepThought this is the existing `<apps-root>/comfyui-truenas/models`.
  On the PC it is `%LOCALAPPDATA%\Comfy-Desktop\ComfyUI-Shared\models`.
- The router **owns `extra_model_paths.yaml`** for the ComfyUI it starts. It writes one
  `abhoth:` section with `base_path = comfy_root` and every role folder. On DeepThought, the
  `cat > … <<YAML` in `comfyui-start.sh` has to stop rewriting it (phase 3). Comfy Desktop
  regenerates its own yaml, so on the PC the router downloads into Desktop's *shared* root
  instead of fighting the generator.
- A Hugging Face download for a component goes to `<comfy_root>/<role folder>/<file>`, not
  `<owner>/<name>/`. ComfyUI lists files flat by name, so the router has to avoid name
  clashes (it refuses, or prefixes `owner--`).
- **Sync between servers**: a component's page shows "present on deepthought ✓, pc ✗ (4.2 GiB)"
  with a **Copy to pc** button. That button is an HF download on the target, or a server-to-
  server copy when the file is not on HF (fine-tunes). Recipes show "runs on: deepthought (pc
  missing 2 files)".

---

## 3. Lifecycle: (c), managed and proxied

The user chose this. This section covers how it behaves.

### 3.1 Engine lifecycle values

These extend SUPERVISOR.md 7.

- `standing`: kept up on its card. Starting point for ComfyUI on X2, jarvis-tts and jarvis-stt
  on X1, and SA3 (CPU).
- `on_request`: started by the first routed request, stopped after `idle_stop`. For the PC's
  ComfyUI, and later for any media engine on A or B.
- `layout`: part of an LLM layout (not used for media at first).

### 3.2 GPU sharing and eviction

- **X2 (16 GB) is the image card**, X1 the speech card. A and B stay LLM-only by default.
  Card engines would read:
  - `X2.engines = { image = ["comfyui"], video = ["comfyui"] }`
  - `X1.engines = { tts = ["chatterbox", "kokoro"], stt = ["whisper-torch"] }`
  X1 and X2 become **`managed = true`, `on_demand = false`**: the router starts and stops
  engines there, but never as part of an LLM layout switch.
- **VRAM accounting per card**: the sum of the standing engines' `vram_gib`, checked at
  placement. X1 holds tts-nano at 3.5 GiB, whisper-small at 1.2 GiB and Kokoro at about
  1 GiB, all within 8 GiB. The router puts Kokoro on X1 only when it fits.
- **Eviction inside ComfyUI**: ComfyUI keeps models resident and swaps them itself. The router
  can call `POST /free {"unload_models": true}` when a different recipe is placed and VRAM is
  short. Because that call never stops the engine, it is safe.
- **Image on A/B (optional, a user choice)**: when both LLM cards are idle, `comfyui` could
  run `on_request` on A or B. It would be subject to every LLM guard: leases, min-stay, the
  calendar, `x-abhoth-no-switch` and the video guards. Off by default (Q2).
- **CPU engines** (SA3) have `card = none` and a CPU or RAM budget. Placement checks
  `mem_available_gb` from the probe against the engine's need, which replaces SA3's own guard.

### 3.3 The daily video pipelines

- The calendar entries already block switches and on-demand work on `server:deepthought`
  during runs. Media routing adds **`block_stop` on media engines** (never stop ComfyUI, SA3
  or X1 engines while a pipeline is active) and a **`prefer` hint**: pipeline requests may go
  to the PC only if the PC is up and has the files. Section 4.1 shows how the scoring handles
  this.
- **Risk**: if the router is down, images and music break, which they would not today. The
  mitigation is the compatibility listeners (section 6.1). Until the final phase, the old
  `:8189` and `:8095` stay up as fallbacks.

---

## 4. Routing and proxying

### 4.1 Choosing a server and card

For each request the router builds the candidates. These are cards (or the CPU) on any server
where:

- the kind is allowed;
- the engine is up, or can start within the request's patience;
- every required file is present (the recipe's `needs`, or a raw workflow's referenced files,
  see 4.4).

The router takes the best candidate by:

1. **Hard filters**:
   - the server is online (health cached 10 s; a failure cools it off for 30 s, as the shim
     does now);
   - files are present;
   - VRAM fits;
   - guards and calendar allow it;
   - a pin (`model@server`, kept from the shim) when one is given.
2. **Score**:
   - user priority per server and kind (`[servers.pc] priority = { image = 0 }`; lower is
     preferred, like the shim's `pref`);
   - idle first (router in-flight 0 and backend queue empty);
   - shortest backend queue, from `/queue` and `queue_remaining` in `/prompt` responses;
   - warm before cold: the recipe's diffusion model already loaded, using the last recipe
     each backend ran;
   - start cost for `on_request` engines.
3. **Wait or fall back**: with no idle candidate, the request queues in the router per kind,
   up to the shim's limits (wait 3600 s, job 900 s). When a job fails on one backend
   (connection loss, OOM or an execution error), it is retried once on another, as now.

**The PC off or asleep**: it fails the health check and drops out. No request waits on it
unless it is pinned. Wake-on-LAN (optional, Q6) can send a magic packet when the PC's priority
says "prefer" and its estimated start time is shorter than the queue wait.

### 4.2 One stable API per kind, on `:8010`

| Path | Kind | Backend handling |
|---|---|---|
| `POST /v1/images/generations` | image | recipe → ComfyUI workflow (the shim's logic, ported into the router: buckets, sigmas, the LoRA chain, b64 or URL responses) |
| `POST /v1/images/edits` (multipart) | image | uploads through `/upload/image` on the chosen backend, then runs the edit workflow; only recipes with `edits = true` |
| `GET /v1/models` | all | adds image, music, tts and stt models with `kind`; `?kind=image` filters |
| `POST /v1/audio/speech` (OpenAI TTS) | tts | Chatterbox `/synth`, Kokoro service; `voice` is mapped; formats wav, mp3, opus |
| `POST /v1/audio/transcriptions` (OpenAI STT, multipart) | stt | whisper-torch through a Wyoming client in the router (it has no HTTP API), or an added HTTP endpoint on it (preferred, phase 2) |
| `POST /v1/audio/music` (Abhoth extension; body = SA3's `{prompt, seconds, bpm, seed, negative}`) | music | SA3 `/generate`; long-running, so it also offers `?async=1` → `202 {id}` and `GET /v1/audio/music/<id>` |
| `GET /v1/jobs/<id>` | all | job status for async media requests (queue position, backend, progress) |

Access, budgets, request logs and leases apply as they do for chat. Routes gain `kind`
(SUPERVISOR.md 7). Wyoming stays as is for Home Assistant: HA keeps talking to `:10203` and
`:10300` directly (section 4.6).

### 4.3 The image-api shim folds into the router

The shim already holds the multi-backend logic the user wants. The router absorbs it:

- `MODELS`, `ALIASES` and `LEGACY_NAMES` become recipes;
- `BACKENDS` become servers with a `comfyui` engine;
- `pref` becomes priority;
- the RAM guard becomes a CPU or RAM fit check;
- the edit backends become recipes whose `needs` include a file only DeepThought has.

### 4.4 Raw ComfyUI pass-through under `/comfyui/`

For tools that send their own workflows, such as ComfyUI API scripts, Krita-AI-Diffusion and
the UI itself:

- `POST /comfyui/prompt`: the router parses the workflow (API format). For every loader node
  (`CheckpointLoaderSimple.ckpt_name`, `UNETLoader.unet_name`, `UnetLoaderGGUF.unet_name`,
  `CLIPLoader*.clip_name*`, `VAELoader.vae_name`, `LoraLoader*.lora_name`,
  `ControlNetLoader`, `UpscaleModelLoader`…) it reads the **file it references**. The fields
  come from `/object_info`: any input whose allowed values are a file list from a model
  folder counts, so new node packs work without code. It also lists the **node classes** used.
  Candidates must have every file and every class. The answer explains a refusal, for example
  "no server has lora X". The router then forwards to the chosen backend and records
  `prompt_id → server`.
- **Affinity**: `/comfyui/history/<id>`, `/comfyui/view?filename=…`, `/comfyui/queue`
  (merged or per server) and `/comfyui/interrupt` go to the server that owns the prompt.
  `client_id → server` is recorded so that `/ws?clientId=` reaches the same backend.
  `/comfyui/upload/image` uploads go to the server chosen for that client's next prompt; until
  one is chosen, the router keeps the upload and replays it to the server it picks.
- **Outputs**: `/view` is streamed through. Optionally (Q5) the router copies finished outputs
  to `<state_dir>/media/<job>/` so that results stay available after a PC goes to sleep, and
  returns `/v1/files/<id>` URLs.
- **Needed router work**:
  - a streaming proxy path (no `to_bytes` for `/comfyui/*`, `/v1/images/edits` or
    `/v1/audio/*`);
  - a WebSocket upgrade passthrough (axum `WebSocketUpgrade` and a client connection to the
    backend `/ws`), with frame relaying that rewrites nothing;
  - per-route body limits (uploads of 50 MB or more).

### 4.5 The ComfyUI web UI

- `GET /comfyui/` serves the stock frontend from the chosen backend (a pinned server, or the
  one that is standing). Recent ComfyUI frontends build API URLs relative to the page path
  (`api/…`, `ws` under the same base), so a subpath **should** work. This must be checked in
  phase 4 against 0.39 and Desktop 0.34. The UI behind the router is per server
  (`/comfyui/@deepthought/`, `/comfyui/@pc/`): a single UI cannot meaningfully span two
  queues.
- The UI needs authentication through the router (an access key or a session cookie from the
  web UI login). Today `:8188` is open on the LAN.

### 4.6 Extra ports (only if needed)

| Port | Why | Can a rewrite avoid it? |
|---|---|---|
| none for OpenAI image, audio or music | all go on `:8010` paths | — |
| **`:8188` compat (optional)** | tools that assume ComfyUI at `/` (some plugins hard-code `/prompt`, `/ws` at root; Krita's plugin takes a base URL, so it works with a subpath) | Mostly yes: if the frontend works under `/comfyui/@x/`, nothing needs it. Keep the backend's own `:8188` for direct access until phase 5, then rebind it to `127.0.0.1` |
| `:8189` and `:8095` compat (temporary) | old clients: pipeline `images.py` and `music.py`, imagegen MCP, Open WebUI, Jarvis | yes: re-point the clients to `:8010` (section 6). Keep the listeners only until then |
| Wyoming `:10203` and `:10300` (unchanged) | Home Assistant speaks Wyoming (raw TCP, not HTTP) | No: it is not HTTP. The engines keep their own ports. The router could add a Wyoming TCP proxy later, but that brings nothing at first |

---

## 5. The Windows PC as a server

> **Superseded (D153):** the PC joins only as a **bridge** (section 11). It uses no Windows
> SSH and is never woken over the network. The notes below on a busy signal and the firewall
> still apply.

- **Registration**: `[servers.pc]` with `exec = "ssh"` to Windows OpenSSH (built in; key auth).
  The executor's builtins today are Linux shell and hostctl. Windows needs a small
  **`abhoth-hostctl.exe`**, the same allowlisted builtin set built for Windows:
  - `telemetry` through `nvidia-smi.exe` (same CSV, so GPU detection works unchanged and the
    3070 Ti becomes card `P1` by UUID);
  - `download` (HF only, into `comfy_root`);
  - `start` and `stop` of the ComfyUI process;
  - `models-list`.
  hostd (fans, clocks, sniff) is **not needed** on the PC.
- **Start and stop**: driver `process` (a new `windows` flavour) runs
  `…\Comfy-Desktop\…\standalone-env\python.exe ComfyUI\main.py --listen 0.0.0.0 --port 8188
  --extra-model-paths-config <router yaml>` as a **scheduled task in the user's session**
  (never with focus theft; per the GUI-testing rule, headless only). The alternative is
  "standing, user-run": the router never starts it and only routes when it is up. The second
  is safer while the PC is a desktop (Q4).
- **Auth**: ComfyUI has no auth of its own. The PC listens on the LAN, and the Windows
  firewall allows `:8188` only from DeepThought (deepthought). The router holds the only
  route.
- **Sleep and gaming**: a "busy" signal (the GPU used by another process above N GiB, from
  telemetry) drops the PC out of placement, so a game never gets a ComfyUI job.

---

## 6. Keeping the pipelines and MCP working (migration)

### 6.1 Compatibility

Phases 1-4 change nothing for existing clients. In phase 4 the router also serves:

- `:8010/v1/images/*`, so clients can move one at a time;
- `GET /health` in the shim's format at `:8010/compat/image-api/health`, because pipeline
  `images.py` waits on the shim's health.

### 6.2 Re-pointing clients, one at a time

Each move is reversible by changing a URL back:

1. imagegen MCP (PC): `IMAGEGEN_SHIM_URL=http://deepthought:8010` plus a router key.
2. OpenOrca image tool, Open WebUI, Jarvis.
3. Video pipeline `images.py` (`IMAGEGEN_SHIM_URL` env) and `music.py` (`SA3_URL` is
   hard-coded: it becomes an env var with the old default). The move happens **outside**
   06:45-07:30, with a manual `run_daily.sh --from images` dry run on a past date.
4. Only then: stop the `image-api` app (rollback: start it again).

### 6.3 Kokoro as a service (optional)

The pipeline's in-process Kokoro on X1 is invisible to the router. As phase 6 it could become a
`kokoro` engine (standing or on request on X1) behind `/v1/audio/speech`. `tts.py` already has
a backend table, so this means adding an `openai` backend to it. Until then, X1's VRAM budget
reserves about 1.5 GiB for it during pipeline runs (a calendar effect).

---

## 7. Phased build plan (revised for D153)

Effort is in agent-days (one builder plus review). Each phase ships on its own and can be rolled
back. Shipping a phase means:

- tests, clippy, fmt and the secret scan;
- a tagged release and a `local/abhoth:N` image;
- a no-network check of the image;
- backups, plus prep and rollback scripts;
- a watched switch.

| Phase | What | Effort |
|---|---|---|
| **1. Inventory and CPU cards** | **Model types:** the `video` kind; a component `role` (with the ComfyUI folder table), `needs` and `aliases`; a server `comfy_root`. **CPU cards:** `device`, `cores`, `ram_gib`, fit against RAM, kept out of the GPU code paths; a `cpu-info` builtin and CPU detection. **Catalog:** `comfyui` and `chatterbox` added; more formats for whisper and sa3. **Probes and presence:** read-only media probes (ComfyUI's files per folder, SA3's health); presence per server and "needs met". **Downloads:** components go into their ComfyUI folders. **Admin:** a CPU badge, the video kind, roles and presence. **Live config:** the default card engines and a CPU card. This phase **starts and routes nothing**. | 4-5 |
| **2. Engine packages** | A manifest JSON Schema with methods (section 10) and `abhoth engine validate <path>`. Method selection per host. Templating for compose, docker and the run command. A supervisor for host processes (restart with backoff, captured logs, kill by exact PID, limits). The approval rules. A package-driven driver beside the manual drivers. Packages in `stores/` for ComfyUI (docker), Chatterbox (docker), whisper-torch (docker), Kokoro (pip and docker), SA3 (docker) and llama.cpp (binary, as the example). | 7-9 |
| **3. Proxy core** | A streaming proxy path; WebSocket passthrough; per-route body limits; route `kind`; async jobs. The `/v1/audio/music`, `/v1/audio/speech` and `/v1/audio/transcriptions` endpoints, and media models in `/v1/models`. Placement across servers and cards (GPU and CPU) by priority, idle state and queue depth, with failover. | 6-7 |
| **4. Images in the router** | The image API shim ported into the router: recipes to workflows, size buckets, edits, retries, the queue. `/comfyui/*` with workflow dependency checks and prompt and client affinity. The ComfyUI page behind the login. | 6-8 |
| **5. Migration on DeepThought** | Router-run engines from packages on X2, X1 and the CPU card. The TrueNAS apps retired one at a time, each with a backup and its own rollback. Dry runs of the pipelines' TTS, image and music calls against the router. Pipelines moved outside their run windows. Old ports kept until every client has moved. | 5-6 |
| **6. Bridge** | **Bridge side:** `abhoth bridge`, headless (env or config file, logs to stdout); an outbound WebSocket with a token, reconnect and backoff; a versioned descriptor; start and stop through packages or manual engines; probes and file lists over the bridge. **Router side:** bridge tokens as secret files; bridge-owned servers (read-only, can be disabled, with a priority); descriptor updates on reconnect. A Docker image without the Claude engine. | 7-9 |
| **7. Backup servers and sync** | The bridge container on truenas.local, deployed after the bridge passes its tests. The PC in bridge mode (we ask the user to switch theirs). Speech failover on demand, with a stop after 10 minutes idle. Automatic model sync: checksums, background runs, a bandwidth limit, disk headroom, an admin prompt when space is short, and sync state in the admin. | 5-7 |
| **8. Store and wizard** | The Engine store tab: browse, install onto a server or card with fit checks, update, remove; extra store URLs; third-party packages marked untrusted; the manifest and the rendered method shown before install. The Create package wizard, usable for every method. | 6-8 |

Total: about 46-59 agent-days.

Dependencies:
- phases 1 and 2 can overlap;
- 3 needs 2;
- 4 needs 3;
- 5 needs 2, 3 and 4;
- 6 needs 2;
- 7 needs 3 and 6;
- 8 needs 2.

## 8. Risks

- **The router becomes a single point of failure for media.** Keep the old ports until phase 6,
  and the router's restart is quick. The video pipeline's images and music retry for a while
  already (`wait_healthy`).
- **ComfyUI cold start takes minutes** (pip and apt on every start). `on_request` on DeepThought
  is impractical until the start script stops reinstalling. Keep X2 `standing`.
- **The workflow dependency parser** misses custom loader nodes. Because the fields come from
  `/object_info` and not from a hard-coded list, the gap is limited to nodes that take free
  text paths. Unknown file fields mean "any server with the node class".
- **WebSocket and long uploads through the router** are new code paths in a hot proxy. Keep
  them in a separate handler that never touches chat.
- **X1 VRAM races** with the pipeline's in-process Kokoro (unaccounted today). Reserve the
  headroom (6.3) until it becomes a service.
- **The PC's two model roots and the generated yaml in Comfy Desktop.** The router writes
  only into the shared root and reads presence from the probe, which sees both roots.

---

## 9. Questions for the user (answered: section 0)

The recommended option is listed first in each question.

1. **Cards per kind.** Which cards may run image, music and speech engines?
   - (a) X2 image/video, X1 TTS/STT, SA3 on the CPU, A and B LLM only (today's de facto split).
   - (b) As (a), plus image `on_request` on A or B when they are idle and no guard blocks it.
   - (c) Let placement use any card with room, for any kind.
2. **TrueNAS apps under router control.** Should the router start and stop the apps?
   - (a) Yes, for comfyui, stable-audio, jarvis-tts and jarvis-stt, through `midclt`; they stay
     TrueNAS apps; the router may start them but stops them only on an admin action or during
     maintenance.
   - (b) Router-native containers: the router runs them with its docker or compose driver and
     the TrueNAS apps are retired.
   - (c) Inventory and routing only: the router never starts or stops the media apps.
3. **The ComfyUI web UI.**
   - (a) Through the router at `/comfyui/@<server>/` with router auth, and `:8188` rebound to
     localhost once that works.
   - (b) Both: the router path, with `:8188` also left open on the LAN.
   - (c) No UI through the router: keep `:8188` direct only.
4. **The PC's ComfyUI lifecycle.**
   - (a) User-run: the router routes to it when it is up and never starts it.
   - (b) The router starts it on demand (a scheduled task, headless) and stops it after idle.
   - (c) The router starts it and keeps it standing whenever the PC is on.
5. **Outputs.**
   - (a) The router keeps a copy of each finished output for N days (default 7), so results
     survive the PC sleeping.
   - (b) Stream from the backend only.
6. **PC priority for images.**
   - (a) Prefer DeepThought for pipeline and unattended work, and the PC for interactive
     requests when it is up and idle.
   - (b) Always prefer the PC when it is up (today's shim `pref`).
   - (c) DeepThought only, with the PC as overflow.
   - Add-on: Wake-on-LAN for the PC: (i) no, (ii) yes.
7. **Kokoro for the pipelines.**
   - (a) Leave it in-process for now; reserve X1 headroom during runs.
   - (b) Make it a router engine behind `/v1/audio/speech` and move `tts.py` to it.
8. **Model sync.**
   - (a) Manual "Copy to server" per file or recipe, with "missing on X" shown on the recipe.
   - (b) Automatic: keep every recipe's files on every image server.
9. **The `video` kind.**
   - (a) Add it now (schema only).
   - (b) Add it when the first video workflow exists.

## 10. Engine packages, the store and the wizard

**The manifest is the package** (`abhoth-engine.json`, which may sit in a folder with the files
it references). It says what the engine is and lists one or more **methods** for installing and
running it, in order of preference. The router or bridge uses the **first method that fits the
host**: its OS and architecture, whether docker is present, and what the GPU offers (cuda, rocm,
metal or cpu). For example, the PC bridge might take `pip` or `binary` while DeepThought takes
`docker`.

| Method | What it holds |
|---|---|
| `compose` | An inline or referenced `docker-compose.yml`, templated (`{{model_dir}}`, `{{port}}`, `{{gpu_uuid}}`, `{{cpus}}`, `{{mem_limit}}`, `{{env.*}}`). |
| `docker` | One image (pinned by digest) plus args, env, mounts and ports. |
| `pip` | A package spec: a pinned version or a hash-pinned requirements list. Also the Python version and the run command. Installs into a venv the router or bridge manages under its state dir. |
| `uv` / `conda` | As `pip`, through uv or conda. They have a place in the schema; support comes later. |
| `binary` | A URL per OS and architecture, each with its sha256, unpacked into the state dir, plus the run command. llama.cpp releases are the typical case. |
| `git` | A repo, a pinned commit, build steps and the run command. For advanced use; always untrusted. |
| `existing` | Already installed. Found on PATH or at a given path, and checked with `--version` against a version regex. This is how a manual engine is described as a package. |

**Fields common to every method:**
- `platforms`: OS and architecture;
- `gpu`: cuda, rocm, metal or cpu, with device selection mapped through env, for example
  `CUDA_VISIBLE_DEVICES={{gpu_index}}` or `NVIDIA_VISIBLE_DEVICES={{gpu_uuid}}`;
- `run`: the templated command and args;
- `env`;
- `workdir`;
- `ports`;
- `health`;
- `update` and `uninstall` steps.

**Package-level fields:** `id`, `name`, `version`, `kinds`, `caps` (formats, architectures and
`inherits`, as in the inventory), `hardware` (GPU vendor, minimum VRAM, whether CPU-only is fine,
SIMD needs), `params` (type, default, description, `per_model`), `api` (openai, comfyui, wyoming,
sa3…), `probe`, `models` (the mount and the folder layout, for example `comfyui`), `security`,
`licence` and `source`.

Examples (from `stores/default/`):

```json
{ "schema": 1, "id": "comfyui", "name": "ComfyUI", "version": "0.39.0", "kinds": ["image", "video"],
  "caps": { "inherits": "comfyui" }, "api": "comfyui", "probe": "comfyui",
  "hardware": { "gpu": ["cuda"], "min_vram_gib": 8, "cpu_ok": false },
  "models": { "mount": "/models", "layout": "comfyui" },
  "methods": [ { "type": "docker", "platforms": ["linux/amd64"], "gpu": "cuda",
      "image": "ghcr.io/example/comfyui@sha256:…",
      "run": ["python", "main.py", "--listen", "0.0.0.0", "--port", "{{port}}",
              "--extra-model-paths-config", "/data/extra_model_paths.yaml"],
      "ports": [ { "id": "http", "container": 8188 } ], "health": { "http": "/system_stats" } } ],
  "security": { "privileged": false, "host_network": false, "host_mounts": [] },
  "licence": "GPL-3.0", "source": "https://github.com/comfyanonymous/ComfyUI" }
```

```json
{ "schema": 1, "id": "kokoro", "name": "Kokoro TTS", "version": "0.9.4", "kinds": ["tts"],
  "caps": { "inherits": "kokoro" }, "api": "openai-speech",
  "methods": [ { "type": "pip", "platforms": ["linux/amd64", "windows/amd64"], "gpu": "cuda",
      "python": "3.12", "requirements": ["kokoro==0.9.4 --hash=sha256:…", "…"],
      "env": { "CUDA_VISIBLE_DEVICES": "{{gpu_index}}" },
      "run": ["{{venv}}/bin/python", "-m", "abhoth_kokoro_server", "--port", "{{port}}"],
      "ports": [ { "id": "http", "host": "{{port}}" } ], "health": { "http": "/health" } } ] }
```

```json
{ "schema": 1, "id": "llamacpp-bin", "name": "llama.cpp (release build)", "version": "b12620",
  "kinds": ["llm", "embedding"], "caps": { "inherits": "llamacpp" }, "api": "openai-chat",
  "methods": [ { "type": "binary", "platforms": ["windows/amd64"], "gpu": "cuda",
      "url": "https://github.com/ggml-org/llama.cpp/releases/download/b12620/llama-b12620-bin-win-cuda-x64.zip",
      "sha256": "…", "run": ["{{install}}/llama-server.exe", "-m", "{{model_file}}", "--port", "{{port}}"],
      "ports": [ { "id": "http", "host": "{{port}}" } ], "health": { "http": "/health" } } ] }
```

**Validation and security:**
- A JSON Schema at `stores/schema/abhoth-engine.schema.json` and the command
  `abhoth engine validate <path>` check a manifest. The router refuses to install one that fails.
- Privileged mode, host network and host mounts are refused unless the manifest declares them
  **and** the admin approves each one.
- `pip`, `git` and `binary` run code on the host. They need explicit approval and hash or commit
  pinning, and the admin shows them with a "runs on the host, not sandboxed" warning.
- Third-party store packages that use these methods are marked untrusted.

**Running engines that are not containers:** the router or bridge supervises them as child
processes, with restart and backoff, captured logs, kill by exact PID, and a resource limit where
the OS allows one (cgroups on Linux, job objects on Windows).

**Manual engines stay.** An engine can be a binary, a command, a container, or an endpoint that
is already running. Placement and routing read the inventory, never where an engine came from.

**The store** is an index of manifests: a git repo or a static `index.json`. The default is the
repo's `stores/` folder, and admins can add store URLs. The admin shows the manifest and the
rendered method before every install.

**The wizard** works in four steps:
1. **Source:** an image, a compose file, a pip package, a GitHub release, or an existing manual
   engine (which pre-fills the rest).
2. **Probe:** image metadata first. A throwaway test container on a chosen card runs only after
   asking.
3. **Form:** kinds, capabilities with `inherits`, hardware, params, ports, model mounts, licence.
4. **Review:** the manifest and the rendered method side by side, checked against the schema.
   Then save to My packages (`<state_dir>/packages/`), install now, or export.

## 11. Bridges

- **Running it:** `abhoth bridge --config bridge.toml` (or env). It runs headless and logs to
  stdout.
- **Connecting:** it **dials** the router (`wss://<router>/bridge/ws`) with a bridge token. A
  router admin mints the token, which is stored as a secret file on both sides and never printed.
  The bridge reconnects with backoff.
- **The descriptor:** on connect it sends `{"v": 1, "server": …, "cards": […], "engines": […],
  "files": {…}}`:
  - `cards`: GPU and CPU, as detected, plus local overrides;
  - `engines`: installed packages and manual engines, with their capabilities and ports;
  - `files`: the model files it has, per folder.

  A changed descriptor sent on reconnect replaces the old one.
- **In the router:** the descriptor becomes a bridge-owned server, read-only in the admin
  ("managed by bridge X"). The admin can disable it or set its priority. The bridge decides what
  it can run; the router decides what to send.
- **Requests:** requests to the bridge's engines travel over the same WebSocket as multiplexed
  streams, so the bridge needs no open port.
- **Commands:** `start`, `stop`, `status`, `probe`, `detect`, `files`, `download`,
  `sync-receive`.

**The store holds metadata only:** manifests (JSON) and small text templates (compose files,
config templates). No binaries, wheels, images, model files or archives. Every artifact is
referenced at its upstream and pinned (image digest, PyPI version and hash, GitHub release URL
and sha256, git commit), fetched by the router or bridge at install time and checked against its
pin. The validator refuses a store package that embeds or vendors binaries or is over about
1 MB. A private package (My packages, a wizard export) may reference a local path or URL with no
upstream; such a package cannot be published to a store.

## 12. Owners and status

| Area | Owner | Status |
|---|---|---|
| Core: CPU cards, media and video kinds, roles and components, media routes on `:8010` (`/v1/images`, `/v1/audio/*`, music, `/comfyui/` with WebSocket and streaming), placement and failover for media, model sync, the DeepThought migration, retiring the TrueNAS apps, closing `:8188`, moving the pipelines | 🟩B | Phase 1 shipped in 0.16.17 (`local/abhoth:16`, live): inventory only |
| Headless bridge: `abhoth bridge`, the protocol (13.1), the descriptor (13.2), its Docker image, PC bridge mode in the desktop app, the truenas.local deploy, supervising engines on the bridge's host; router side: `/bridge/ws`, tokens, the overlay and tunnel (13.4), the Bridges admin page | 🟧E | Shipped: bridge, protocol, image, PC Bridge mode, router side (tokens, overlay, tunnel, admin page); client routing waits on the media routes calling `upstream_for` |
| Engine packages: the manifest schema and methods, `abhoth engine validate`, the shared runner (13.3), the metadata-only `stores/`, the Engine store tab and the wizard, manual engines first-class, packages for ComfyUI, Chatterbox, whisper, Kokoro, sa3 and llama.cpp | 🟥F | Shipped in abhoth-v0.16.20: crate `orca-engines`, `driver = "package"`, `/admin/api/engine-store/`, the Engine store tab and wizard, docs/ENGINE-PACKAGES.md. The router runs container methods only; host methods run under the bridge. |

Rules: section 13 is the source of truth for the seams between the three. Change it in small
commits on main, before the code that depends on the change. Take the next free D-number and
version from main at release time.

## 13. Interfaces

### 13.1 Bridge protocol (v1)

- **Transport:** the bridge dials `GET /bridge/ws` on the router's `:8010` (wss behind TLS) with
  `Authorization: Bearer <bridge token>`. No port opens on the bridge's host.
- **Tokens:** a router admin mints one per bridge (`abb_` prefix, shown once). The router keeps
  only its SHA-256 in `<state_dir>/secrets/bridges/<bridge-id>.sha256`. The bridge reads its token
  from `token_file` (mode 600). Neither side logs or prints it.
- **Frames:** JSON text frames for control messages; binary frames for stream data. Every control
  message is `{"t": "<type>", ...}`.
- **Keepalive and reconnect:** a ping every 20 s; the router marks the server offline after 45 s
  of silence. The bridge reconnects with backoff from 1 s to 60 s, with jitter.
- **Versions:** `v` is the protocol major. The router closes with code `4400` on a major it does
  not speak, and `4401` on a bad token.

| Direction | `t` | Fields | Meaning |
|---|---|---|---|
| bridge → router | `hello` | `v`, `bridge` (id), `version` (abhoth version), `descriptor` (13.2) | The first frame on every connection. |
| router → bridge | `welcome` | `v`, `server` (the id the router gives it), `settings` (`enabled`, `priority`) | The connection is accepted. |
| bridge → router | `descriptor` | `descriptor` | The full descriptor again after a local change; it replaces the old one. |
| router → bridge | `cmd` | `id`, `op`, `args` | `op`: `start`, `stop`, `status`, `logs`, `probe`, `detect`, `files`, `download`, `sync-receive`, `install`, `uninstall`. |
| bridge → router | `result` | `id`, `ok`, `data` or `error` | The answer to a `cmd`. |
| bridge → router | `event` | `kind` (`engine-state`, `download`, `sync`, `log`), `data` | Unprompted state changes. |
| router → bridge | `open` | `sid`, `engine`, `card`, `method`, `path`, `headers`, `ws` (bool) | Open a proxied HTTP request or WebSocket to a local engine. |
| bridge → router | `head` | `sid`, `status`, `headers` | The engine's response head (or the WebSocket upgrade result). |
| either | binary frame | 4-byte big-endian `sid`, then the payload | Body bytes, or one WebSocket message. |
| either | `credit` | `sid`, `bytes` | Flow control: each side may send at most 1 MiB of unacknowledged body bytes per stream. |
| either | `end` / `reset` | `sid` (`reset` adds `error`) | A clean end, or an abort. |
| either | `ping` / `pong` | `at` (ms) | Application keepalive, beside WebSocket pings (some proxies drop those). |

- **Stream ids:** the router picks `sid`s (odd numbers, from 1). A stream's request body follows
  `open` as binary frames, then `end`. A response body follows `head`, then `end`.
- **WebSocket streams** (`ws: true`): each binary frame's payload is one message, with its first
  byte the opcode (`1` text, `2` binary) and the rest the message. `end` is a close.
- **Starting on demand:** an `open` for an engine with `lifecycle = "on_demand"` that is not
  running makes the bridge start it, wait for its health check, then proxy. The bridge stops it
  after `idle_stop_seconds` with no stream open. The router never has to send `start` first.
- **Library:** crate `orca-bridge` holds the frames, the descriptor types, the stream multiplexer
  (both ends), the bridge's dial loop, detection (GPUs through `nvidia-smi`, CPU and RAM) and the
  engine supervisor seam (13.3; a minimal docker/process runner until `orca-engines` lands).

### 13.2 Bridge descriptor (v1)

```json
{ "v": 1,
  "server": { "id": "truenas", "title": "truenas.local", "os": "linux", "arch": "amd64",
              "model_dir": "/models", "comfy_root": "/models/comfyui" },
  "cards": [
    { "id": "GPU0", "device": "gpu", "gpu_uuid": "GPU-...", "label": "Quadro M4000", "vram_gib": 8.0,
      "engines": { "tts": ["chatterbox", "kokoro"], "stt": ["whisper"] }, "shared": true },
    { "id": "CPU", "device": "cpu", "cores": 4, "ram_gib": 6.0, "simd": ["avx2"],
      "engines": { "tts": ["kokoro"] } } ],
  "engines": [
    { "id": "chatterbox", "package": "chatterbox@1.0.0", "method": "docker", "kinds": ["tts"],
      "caps": { "inherits": "chatterbox" }, "api": "openai-speech", "probe": "none",
      "ports": { "GPU0": 10204 }, "lifecycle": "on_demand", "idle_stop_seconds": 600 } ],
  "files": { "comfyui/diffusion_models": ["..."], "hf": ["owner/name"] },
  "limits": { "max_concurrent": 2 } }
```

- `cards[].engines` uses the same shape as `cards.x.engines` in `router.toml`. `shared: true`
  means something outside the bridge also uses the card (Frigate on truenas.local), so the router
  never assumes it holds all of the VRAM.
- `engines[].package` is null for a manual engine. Placement never reads it.
- The router gives bridge cards global ids `<SERVER>-<card>` (for example `TRUENAS-GPU0`). An id
  that clashes with `router.toml` is refused, with the reason sent back in `result`.

### 13.3 Engine manifest and runner (🟥F, used by 🟩B and 🟧E)

Revised by 🟥F on 2026-11-27; this is what the code implements. Changes go here first.

**Manifest** (section 10): `abhoth-engine.json`, `"schema": 1`. JSON Schema at
`stores/schema/abhoth-engine.schema.json`; the Rust types are the authority and a test checks the
schema against them. Unknown top-level and method keys are errors, so typos fail loudly. Worked
examples: `docs/ENGINE-PACKAGES.md` and `stores/default/*/abhoth-engine.json`.

**Crate `orca-engines`** (no dependency on `orca-router`; the router and the bridge both use it):

```rust
// manifest.rs
pub struct Manifest { schema, id, name, version, description, kinds: Vec<String>, caps: Caps,
    hardware: Hardware, params: IndexMap<String, Param>, api: String, probe: Option<String>,
    models: Option<ModelsLayout>, methods: Vec<Method>, security: Security, licence, source }
pub struct Caps { inherits, kinds, formats, archs, quants, add, remove }   // = router EngineCaps
pub struct Method { r#type: MethodType, platforms, gpu: Vec<GpuKind>, requires: Requires,
    run: Vec<String>, env: IndexMap<String, String>, workdir, ports: Vec<PortDecl>,
    health: Health, mounts: Vec<Mount>, update: Vec<Step>, uninstall: Vec<Step>,
    /* per type: */ image, compose, compose_file, python, requirements, package,
    assets: Vec<Asset>, repo, commit, build: Vec<Step>, detect: Option<Detect> }
pub enum MethodType { Compose, Docker, Pip, Uv, Conda, Binary, Git, Existing }
pub enum GpuKind { Cuda, Rocm, Metal, Cpu }
impl Manifest { pub fn parse(text: &str) -> Result<Manifest, Vec<Issue>>; }

// validate.rs: `abhoth engine validate <dir|file> [--private]`
pub fn validate_path(path: &Path, mode: Mode /* Store | Private */) -> Report;
pub struct Report { ok: bool, manifest: Option<Manifest>, issues: Vec<Issue> } // Issue {level, at, message}

// host.rs: what a host is (bridge: from detect; router: the server's telemetry + cpu-info)
pub struct HostFacts { os: String /* linux|windows|macos */, arch: String /* amd64|arm64 */,
    docker: bool, compose: bool, python: Option<String>, gpus: Vec<Gpu>, cpu: Cpu,
    path_tools: Vec<String> /* found on PATH, for `existing` */ }
pub struct Gpu { index: u32, uuid: String, vendor: GpuKind, name: String, vram_gib: f64 }
pub struct Cpu { cores: u32, ram_gib: f64, simd: Vec<String> /* avx, avx2, avx512f, neon */ }
pub enum CardSel { Gpu { card: String, index: u32, uuid: String, vendor: GpuKind, vram_gib: f64 },
                   Cpu { card: String, cores: u32, ram_gib: f64 } }
pub struct MethodFilter { host_processes: bool /* false: only docker/compose */, only: Option<MethodType> }
pub fn choose_method<'a>(m: &'a Manifest, h: &HostFacts, card: &CardSel, f: &MethodFilter)
    -> Result<(usize, &'a Method), Vec<Rejection>>;   // Rejection { method: usize, reasons: Vec<String> }
pub fn fit(m: &Manifest, h: &HostFacts, card: &CardSel, f: &MethodFilter) -> Fit; // store tab check

// render.rs: templates are {{name}}; an unknown name is an error, never left in place
pub struct RunSpec { engine: String, card: CardSel, port: u16, host: String /* bind, 127.0.0.1 */,
    model: Option<ModelRef>, params: BTreeMap<String, Value>, limits: Limits,
    model_dir: Option<String>, name: String /* container/process name: "abhoth-comfyui-x2" */ }
pub struct Limits { cpus: Option<f64>, mem_gib: Option<f64> }
pub fn plan_start(m: &Manifest, i: &Installed, s: &RunSpec, ok: &Approvals) -> Result<StartPlan, String>;
pub enum StartPlan {
    Docker { name: String, argv: Vec<String> },      // the full `docker run -d --name … image args`
    Compose { project: String, yaml: String, up: Vec<String>, down: Vec<String> },
    Process { name: String, program: String, args: Vec<String>, env: Vec<(String, String)>,
              cwd: Option<String>, limits: Limits },
    External { name: String, url: String },          // `existing` with `detect.url`: nothing to run
}
```

`RunSpec` also has `ports` (extra ports by id), `state` (the instance's data folder **on the
host**) and `package_dir` (the installed package folder on the host, mountable read-only), so a
router whose state dir is inside its own container passes host paths. In a compose plan, `up`
and `down` contain the literal `{{compose_file}}`, which the caller replaces with wherever it
wrote `yaml`.

Template names: `{{port}}`, `{{host}}`, `{{gpu_index}}`, `{{gpu_uuid}}`, `{{cpus}}`,
`{{mem_limit}}` (e.g. `6g`), `{{model_dir}}`, `{{model_file}}`, `{{model_id}}`, `{{state}}` (the
engine's own state dir), `{{install}}` (the unpacked binary or git checkout), `{{venv}}`,
`{{python}}` (the venv's interpreter, per OS), `{{exe}}` (`.exe` on Windows, else empty),
`{{name}}`, `{{bin}}` (an `existing` program), `{{models}}` (the model folder as the engine sees
it: the container mount, or the host path), `{{package}}`, `{{port.<id>}}`, `{{param.<id>}}`.
Device selection: a `cuda` method on a GPU card gets
`CUDA_VISIBLE_DEVICES={{gpu_index}}` (host processes) or `--gpus device=<uuid>` (docker) unless
it sets them itself; on a CPU card GPUs are hidden (`CUDA_VISIBLE_DEVICES=""`, no `--gpus`).

```rust
// pins.rs
pub fn check_pins(m: &Method, mode: Mode) -> Vec<Issue>;  // image@sha256:<64 hex>; pip `==` + --hash;
                                                         // asset sha256; git 40-hex commit
pub fn verify_file(path: &Path, sha256: &str) -> Result<(), String>;

// runner.rs
#[async_trait] pub trait HostExec: Send + Sync {          // how a command runs on the host: locally
    async fn run(&self, argv: &[String], o: &ExecOpts) -> Result<ExecResult, String>; // or through
}                                                         // the router's hostctl/ssh executor
#[async_trait] pub trait Runner: Send + Sync {
    // Package = the manifest plus its text files (templates, shims), its store and trust.
    // The card picks the build to fetch (a binary method's CUDA or CPU asset).
    async fn install(&self, p: &Package, method: usize, card: &CardSel, ok: &Approvals) -> Result<Installed, RunError>;
    async fn update(&self, old: &Installed, p: &Package, method: usize, card: &CardSel, ok: &Approvals) -> Result<Installed, RunError>;
    async fn uninstall(&self, m: &Manifest, i: &Installed) -> Result<(), RunError>;
    async fn start(&self, m: &Manifest, i: &Installed, s: &RunSpec) -> Result<Handle, RunError>;
    async fn stop(&self, h: &Handle, grace: Duration) -> Result<(), RunError>;
    async fn status(&self, h: &Handle) -> Result<State, RunError>;   // Running/Exited/Backoff/Failed…
    async fn logs(&self, h: &Handle, lines: u32) -> Result<String, RunError>; // secrets scrubbed
    async fn adopt(&self) -> Vec<Handle>;                             // after a restart
}
pub struct LocalRunner { /* state_dir, Arc<dyn HostExec>, Arc<dyn Fetch>, supervisor */ }
impl LocalRunner { pub fn new(state_dir: PathBuf, exec: Arc<dyn HostExec>, fetch: Arc<dyn Fetch>,
    host_processes: bool /* false in the router container: pip/uv/binary/git/existing refused */) -> Self; }
pub struct Approvals { host_run: bool, privileged: bool, host_network: bool,
    host_mounts: Vec<String>, devices: Vec<String>, untrusted: bool } // each an explicit admin click
pub struct Installed { id, version, method: usize, method_type, dir: PathBuf, venv: Option<PathBuf>,
    install: Option<PathBuf>, image: Option<String>, pins: Vec<String>, at: i64, approvals }
pub enum Handle { Container { name: String }, Compose { project: String, file: PathBuf },
    Process { pid: u32, started: u64 /* OS start time: guards PID reuse */, name: String } }
```

- **State dir layout** (router and bridge alike): `<state>/engines/<id>/<version>/installed.json`
  (+ `package/` (the manifest and templates as installed), `venv/`, `bin/`, `src/`, `app/`), `<state>/engines/run/<name>.json` (the `Handle`),
  `<state>/engines/logs/<name>.log` (rotated at 8 MiB, 2 kept), `<state>/packages/` (My packages).
- **Supervision** (host processes): restart on exit with backoff 1 s, 2 s, 4 s … 60 s, reset
  after 10 min up; at most 5 restarts in 10 min, then `Failed`. Stop is SIGTERM (Windows:
  `taskkill /PID <pid> /T`), then a kill after `grace`, always by exact PID checked against
  `started`. Limits: on Linux, `prlimit --as` and `nice` when `mem_gib` is set; on Windows none
  yet (reported in `State.notes`).
- **Docker/compose** run through `HostExec`, so the router uses its hostctl executor on
  DeepThought and the bridge runs `docker` locally. The runner never adds `--privileged`,
  `--network host`, devices, or bind mounts outside `{{model_dir}}` and `{{state}}` unless
  `security` declares them **and** `Approvals` approves each one.

**Store** (`orca_engines::store`):
- A store is a folder or URL with `index.json`: `{"schema": 1, "name", "packages": [{"id",
  "version", "path", "sha256", "name", "kinds", "methods", "summary"}]}`; `sha256` is the
  manifest file's, checked when fetched. `abhoth engine index <store-dir>` rebuilds it.
- The default store is `stores/default/`, embedded in the router at build time (works offline)
  and **trusted**. Store URLs the admin adds (a static `index.json` over https, or a GitHub repo
  URL, read as `raw.githubusercontent.com/<owner>/<repo>/<ref>/index.json`) are **untrusted**
  unless the admin marks them trusted. The list is `<state_dir>/engine-stores.json`.

**Router API** (🟥F; `/admin/api/engine-store/…`, admins only):

| Call | Does |
|---|---|
| `GET stores`, `POST stores {url, trusted}`, `DELETE stores/<id>` | The store list. |
| `GET packages` | Every package from every store, plus My packages, with trust and methods. |
| `GET package?store=&id=` | The manifest, its templates and the validation report. |
| `POST fit {store, id, server, card}` | Hardware fit, the method that would be chosen, and why each other one was refused. |
| `POST validate {manifest, files}` | The wizard's live check. |
| `POST install {store, id, server, card, approvals, engine?}` | Installs, then adds `[engines.<id>]` with `driver = "package"`. 202 + a job. |
| `POST update {engine}`, `POST remove {engine}` | Update to the store's newer version; uninstall and remove the engine. |
| `GET installed` | Installed packages per server, with versions and available updates. |
| `POST wizard/probe-image {image}` | Registry metadata only (digest, ports, env, labels, licence); runs nothing. |
| `POST wizard/test-run {image, server, card, confirm: true}` | A throwaway `--rm` container with no mounts, probed (`/v1/models`, `/object_info`, `/health`, `/system_stats`), then removed. Refused without `confirm`. |
| `GET my`, `PUT my/<id>`, `DELETE my/<id>`, `GET my/<id>/export` | My packages (`<state_dir>/packages/<id>/`); export is a zip. |

**In `router.toml`** (🟥F adds the schema; 🟩B uses it for the migration):

```toml
[engines.comfyui]
driver  = "package"
port    = { X2 = 8188 }
# installed: <router state>/engines/<server>/comfyui/0.45.0/installed.json
package = { id = "comfyui", version = "0.45.0", method = "docker", params = { preview = "auto" } }
```

`method` (optional) pins a method type, else the installed one is used; `params` are the
manifest's params (the engine's existing `params` table is the request params, so they live
here).

`driver = "package"` reads the installed manifest and asks `plan_start` for the card. `Docker`
and `Compose` plans run as the same hostctl commands the `docker` and `compose` drivers use.
`Process` plans run only where the runner is local (a bridge, or `exec = "local"`). Kinds,
caps, api and health come from the manifest unless the engine entry overrides them. Nothing else
in placement or routing reads `package`: manual engines (`docker`, `compose`, `process`,
`external`, …) keep working with no package.

### 13.4 Inventory hooks (🟩B places and proxies; 🟧E builds the bridge side of them)

Split: 🟧E writes `orca-router/src/bridges/` (the `/bridge/ws` endpoint, tokens, the registry of
connected bridges, the overlay below, `upstream_for` and the tunnel client, the bridges admin API
and page). 🟩B's placement and media proxy call `bridges::effective(&config)` for the merged
config and `bridges::upstream_for(...)` / `bridges::forward(...)` to send a request. Bridge
servers carry `origin: Some("bridge:<id>")` (a `#[serde(skip)]` field: never read from or
written to `router.toml`) and `exec = "bridge"` (refused in `router.toml`). A bridge engine whose
id is already an engine in `router.toml` gets the bridge's cards added to its `port` map; an
unknown id becomes a new `driver = "external"` engine.

- **Bridge-owned servers are an overlay** on the `router.toml` config: `servers`, `cards` and
  `engines` with `origin = bridge:<id>`. They are never written to `router.toml`. The same
  validation rules apply, and id clashes are refused.
- **Admin settings** for each bridge (`enabled`, `priority`) live in `<state_dir>/bridges.json`.
  The admin shows a bridge server read-only ("managed by bridge X"), with those two controls.
- **Calls:**
  - `control::bridges::upsert(bridge, Descriptor) -> Result<(), Vec<Issue>>`;
  - `control::bridges::offline(bridge)`;
  - `control::bridges::remove(bridge)`.

  Each one rebuilds the effective config that placement and the admin read. An offline bridge's
  cards stay listed but are never placed on.
- **Execution:** `ExecKind::Bridge` (set by the overlay, never in `router.toml`). `exec_for(c,
  server)` returns an executor that sends `cmd` frames and waits for the matching `result`.
- **Files and probes:** the descriptor's `files`, and later `probe` results, feed
  `caps::record_media_probe(engine, server, Probe)`. Presence and "needs met" then work for
  bridge servers unchanged.
- **Routing:** the proxy asks `upstream_for(server, engine, card)`. That gives `Http(url)` for
  servers the router reaches directly, or `Bridge { bridge, engine, card }`, which opens a stream
  over the bridge connection (13.1).
- **Placement order:** enabled and online servers only; DeepThought first, the PC for overflow
  only (D153); then the server's `priority`, idle state and queue depth.
