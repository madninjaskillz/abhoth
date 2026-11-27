# Engine packages

An engine package tells Abhoth how to install and run an inference engine (ComfyUI, Kokoro,
whisper, llama.cpp…) on a server or card. It is **metadata only**: one `abhoth-engine.json`
manifest plus small text templates (a compose file, a shim script). Every artifact it names
(an image, a wheel, a release archive, a git commit) is pinned upstream and fetched from there.

Manual engines are still first-class: anything you can write by hand in `router.toml`
(Inventory → engines) keeps working without a package.

Design notes and the full interface are in [design/ROUTER-MEDIA-PLAN.md](design/ROUTER-MEDIA-PLAN.md)
section 13.3; the JSON Schema is [stores/schema/abhoth-engine.schema.json](../stores/schema/abhoth-engine.schema.json).

## The manifest

```json
{
  "$schema": "../../schema/abhoth-engine.schema.json",
  "schema": 1,
  "id": "kokoro",
  "name": "Kokoro TTS",
  "version": "0.9.4",
  "kinds": ["tts"],
  "caps": { "inherits": "kokoro" },
  "hardware": { "gpu": ["cuda"], "cpu_ok": true, "min_vram_gib": 1.5 },
  "params": {
    "voice": { "type": "string", "default": "af_bella", "description": "The default voice." }
  },
  "api": "openai-speech",
  "probe": "http",
  "models": { "layout": "hf", "env": "HF_HOME", "mount": "/models", "read_only": false },
  "methods": [
    {
      "type": "docker", "label": "Kokoro-FastAPI (GPU)",
      "platforms": ["linux/amd64"], "gpu": ["cuda"],
      "image": "ghcr.io/remsky/kokoro-fastapi-gpu:v0.2.4@sha256:79d81c41…",
      "ports": [{ "id": "http", "container": 8880 }],
      "health": { "http": "/health", "start_seconds": 300 }
    },
    {
      "type": "pip", "label": "pip (host)", "gpu": ["cuda", "cpu"],
      "platforms": ["linux/amd64", "windows/amd64", "macos/arm64"],
      "python": "3.12", "requirements": ["kokoro==0.9.4", "soundfile==0.13.1"],
      "files": ["server.py"],
      "run": ["{{python}}", "{{install}}/server.py", "--host", "{{host}}", "--port", "{{port}}"],
      "ports": [{ "id": "http" }], "health": { "http": "/health" }
    }
  ],
  "licence": "Apache-2.0",
  "source": "https://github.com/hexgrad/kokoro"
}
```

- **methods** are in order of preference. The host takes the first one that fits the card
  (platform, GPU stack, VRAM, docker or a Python present). The router only runs container
  methods (`docker`, `compose`, an `existing` image). The bridge and `abhoth engine` can also
  run host methods (`pip`, `uv`, `binary`, `git`, an `existing` program).
- **Method types:**
  - `compose`: a compose file in the package.
  - `docker`: an image pinned by digest.
  - `pip` / `uv`: a venv under the state dir; versions pinned with `==`, or hash-pinned
    requirements.
  - `binary`: per-platform assets, each with a URL and a sha256.
  - `git`: a 40-character commit.
  - `existing`: detect what is already installed; nothing is fetched.
- **Template variables** are `{{port}}`, `{{host}}`, `{{card}}`, `{{gpu_uuid}}`, `{{model_dir}}`,
  `{{models}}`, `{{state}}`, `{{install}}`, `{{python}}`, `{{package}}`, `{{name}}`,
  `{{param.<id>}}` and `{{port.<id>}}`. An unknown variable is a validation error.
- **security** declares anything beyond a plain container: `privileged`, `host_network`,
  `host_mounts` (each with a `why`), `devices` and `cap_add`. Undeclared, these are never
  added. Declared, each one must be ticked by the admin at install time.

## Commands

```
abhoth engine validate stores/default/kokoro        # store rules: licence, https source, pins
abhoth engine validate ./my-engine --private        # private packages may skip publishing rules
abhoth engine index stores/default --name default   # write index.json for a store folder
abhoth engine index stores/default --check          # CI: is index.json current?
abhoth engine fit stores/default/comfyui --card cpu # which method this machine would use, and why
abhoth engine schema                                # print the JSON Schema
```

`validate` refuses binaries (ELF, PE, Mach-O, wheels, archives, GGUF…), blobs of base64 or hex,
symlinks, and packages over 1 MiB.

## Stores

The **default store** is `stores/default/` in this repo, built into Abhoth. Admins can add
store URLs: a GitHub repo folder, or any https folder with an `index.json`. Added stores are
**untrusted** unless the admin ticks "trusted". Installing from an untrusted store needs its
own approval.

Every file listed in the index carries a sha256, and Abhoth checks each one when it fetches the
package. To publish a store:

1. Put each package in its own folder.
2. Run `abhoth engine index <dir> --name <name>`.
3. Commit the folders and `index.json`.

## Engine store (admin)

The **Engine store** tab is in `/admin/` and in the app's Router config.

1. **Browse.** The list shows packages from every store, filtered by kind or text, with trust,
   installed and update badges.
2. **Details.** Open a package to see its methods, params, the validation report, and the
   manifest and every template, read-only.
3. **Install.** Pick a server; each card shows whether the package fits, which method it would
   use, and why the other methods were refused. Then:
   1. pick a card;
   2. confirm you have read the manifest;
   3. tick each approval;
   4. set the engine id and params.

   The install runs as a job with a log: `docker pull` of the pinned image, through the
   server's executor. It then adds `[engines.<id>]` to `router.toml`:

   ```toml
   [engines.kokoro]
   title = "Kokoro TTS"
   driver = "package"
   served_id = "kokoro"
   port = { A = 18100 }
   container = "abhoth-kokoro-{card_lc}"
   package = { id = "kokoro", version = "0.9.4", store = "default" }
   health = { path = "/health" }
   caps = { kinds = ["tts"] }
   ```

   The card's `engines.tts` list gets the engine too. From then on, layouts place it like
   any other engine.
4. **Installed.** This lists each server's packages with their pins and engines.
   - **Update** installs the newer version with the same approvals. It refuses if the new
     version asks for more.
   - **Remove** takes the engine out of `router.toml`. If you tick "also uninstall", it also
     removes the package's files. Images are never removed.

Instance data lives on the host under `<engine_state>/<engine>-<card>`. `engine_state` is a
server var and defaults to `/var/lib/abhoth/engines`.

## Create package wizard

**Engine store → Create package**, or "Package this engine" on a manual engine.

1. **Source.** Start from one of:
   - scratch;
   - a docker image: the router reads its registry manifest and config (ports, env, labels,
     licence, source) and pins the digest, without running anything;
   - a compose file: images that aren't pinned are flagged;
   - a pip package;
   - a GitHub release: its assets, with GitHub's sha256 where it lists one;
   - an existing manual engine.
2. **Probe.** Review what was found. **Test run** is optional, and asks first. It starts the
   image once on the card you choose:
   - no mounts, `--cap-drop ALL`, no-new-privileges;
   - 8 GiB and 4 CPUs;
   - a localhost-only port.

   It probes the usual paths, suggests the API and health path, shows the logs, and removes
   the container.
3. **Form.** Fill in id, name, version, kinds, hardware, params, API, model folder, licence and
   methods.
4. **Review.** The JSON and templates sit side by side and are validated as you type. Choose
   private mode, or store mode (what publishing needs).
5. **Save.** Save to My packages, then install it, or export it as a zip. My packages can point
   at private images and local paths, but they can't be published until they pass store mode.

## Worked examples

### An HTTP service image you already use

Suppose the image is `ghcr.io/acme/tts-server:1.4`.

1. Create package → docker image → `ghcr.io/acme/tts-server:1.4`. The draft has the image
   pinned to `…:1.4@sha256:<digest>`, port 8000 from `EXPOSE`, and the licence from the OCI
   label.
2. Test run on the CPU card. `/health` and `/v1/audio/voices` answer, so apply
   "api openai-speech, health /health".
3. In the form, set kinds to `tts`, add the GPU method (`gpu: ["cuda"]`), and mount the model
   folder: `models.mount = "/models"`.
4. Review shows no errors in private mode. Save, then install on card A.

### A compose stack

A whisper package's `compose.yml` uses the instance variables:

```yaml
services:
  whisper:
    image: docker.io/rhasspy/wyoming-whisper:3.8.1@sha256:ba6fcb60…
    container_name: {{name}}
    command: ["--model", "{{param.model}}", "--language", "{{param.language}}"]
    ports: ["{{host}}:{{port}}:10300"]
    volumes: ["{{model_dir}}:/data"]
```

The manifest's method is `{ "type": "compose", "compose_file": "compose.yml", "health": { "tcp": true } }`.
Bind mounts may only be `{{model_dir}}`, `{{state}}`, the package folder (read-only) or a
declared `host_mounts` entry. `privileged` and `network_mode: host` are refused unless
declared.

### A release binary (llama.cpp)

```json
{
  "type": "binary", "label": "CUDA 12.4 build", "platforms": ["windows/amd64"], "gpu": ["cuda"],
  "assets": [
    { "platform": "windows/amd64", "gpu": "cuda", "url": "https://github.com/ggml-org/llama.cpp/releases/download/b12292/llama-b12292-bin-win-cuda-12.4-x64.zip", "sha256": "078a4392…" }
  ],
  "run": ["{{install}}/llama-server{{exe}}", "--host", "{{host}}", "--port", "{{port}}", "-m", "{{model_dir}}/{{model_file}}"]
}
```

Every asset is checked against its sha256 before it is unpacked, and unpacking refuses paths
that escape the folder.

### Describing an engine you run by hand

The `sa3` package in the default store describes the locally built `local/sa3:1` image. It has
an `existing` method with `detect.image`, and its service, source and cache folders are
declared as `host_mounts` with a reason for each. Nothing is fetched. The install checks that
the image is there, and the admin approves each mount.

## Starter packages

| Package | Kinds | Methods |
|---|---|---|
| comfyui | image, video | docker (yanwk/comfyui-boot, CUDA 12.6), git (pinned commit + torch) |
| kokoro | tts | docker GPU, docker CPU, pip + shim |
| chatterbox | tts | existing image (local/tts-nano), pip + shim |
| whisper | stt | compose (CPU image), pip (wyoming-faster-whisper) |
| sa3 | music | existing image (local/sa3), CPU |
| llamacpp-bin | llm | binary (Windows CUDA, CPU builds), existing `llama-server` |

## Limitations

- The router runs container methods only. Host methods (pip, binary, git, existing programs)
  run under the bridge or `abhoth engine`.
- Conda methods validate but are skipped.
- The fit check on a router server takes `linux/amd64` with docker as given, and uses the
  cards' VRAM from `router.toml`.
- A test run needs `curl` on the host.
