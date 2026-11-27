# This machine: local engines in the desktop app (D155)

**Names ([NAMES.md](../NAMES.md)):** Sothoth is the router, Nodens the supervisor and Migo the bridge. The old command and file names (`abhoth router`, `abhoth supervisor`, `abhoth bridge`, `router.toml`, `supervisor.toml`, `bridge.toml`) still work; this design keeps them where it was written with them.

The desktop app gets the router's management for one server: the PC it runs on. The same
pages (Inventory, Engine store, folder browser, review-then-save) edit an app-owned config; the
app runs those engines itself; in Migo mode the same inventory is what the PC shares with a
router. Owner: 🟧E. Builds on ROUTER-MEDIA-PLAN.md (D153) sections 11 and 13.

## 1. What the user asked for (2026-11-27)

- The desktop app should have the router's management in bridge mode. A bridge can't add
  servers; it has one default server, this machine, named after its hostname.
- Bridged: expose engines on this machine, but still send everything through the router.
- Not in router mode: the same management is useful locally.

## 2. Decisions

1. **One server, no "add server".** The server's id is the hostname (lower case, `[a-z0-9-]`),
   its title the hostname as typed, editable. Cards come from detection (`GPU0`, `GPU1`, …, `CPU`,
   the ids the bridge already uses), with local overrides (label, VRAM/RAM budget, reserve,
   shared, allowed engines per kind). No layouts, no switching, no SSH, no admin keys.
2. **The app owns one config folder**, `<home>/machine/`:
   - `router.toml`: the server, its cards, engines and models, in the router's own schema, so
     the router's forms, checks, diffs, backups and Engine store work on it unchanged.
     `[control] enabled = false`: nothing in the router itself starts or switches anything.
   - `router.json`: empty model list (the gateway part is not used).
   - `state/`: the router's state for it (installed packages, downloads, caps cache, backups).
   - `share.json`: which engines and models are shared with a router (2.6).

   The old *Bridge: engines file* is migrated once, the first time the folder is made (2.7). The
   setting stays, under Advanced, as an override for headless-style setups.
3. **The management API is an in-process router, with no socket.** The app builds the
   router's `AppState` from `machine/router.toml` (`control::startup::start`, admin pages only)
   and calls its axum app directly from the web host's `/api/admin/machine/*` handler, as a
   tower service with a loopback peer. No port is opened, so nothing else on the PC can reach
   it; the web host's own sign-in (the desktop's launch token) guards the handler.
   *Why not a router on a loopback port:* any local process could then edit engines, and an
   engine is a command line. *Why not a new API:* the router's form edits, checks, Engine store
   jobs, Hugging Face downloads and folder browsing are already written and tested; a second
   copy would drift.
4. **Engines run under the bridge's supervisor**, in-process, whether or not Migo mode is on.
   The router's own drivers are shell-command templates written for Linux hosts; the bridge's
   supervisor (`orca-bridge`: docker, process, engine packages through 🟥F's `orca_engines`
   runner, on demand with an idle stop and a free-RAM floor) already runs on Windows, Linux and
   macOS. The app translates `machine/router.toml` into a bridge config (3) and gives the
   supervisor the result. One supervisor serves both the app's own requests and the router's.
5. **Local placement is the router's.** A request for a local model uses
   `inventory::place(config, model)`, the function the router itself places with: the model's
   engines (primary first), on a card that allows that engine for the model's kind, has its
   files and fits. The supervisor starts it on demand, then the app proxies the request.
6. **Sharing.** Each engine and model has *Share with router* (`share.json`). An engine added
   while Migo mode is on is shared by default; one added while it is off is not. Unshared
   engines and models are local-only: the bridge's descriptor leaves them out (unshared models'
   files are left out of `files`), so the router never places on them.
7. **Migration.** If *Bridge: engines file* names a readable file when `machine/` is first made,
   its `[cards.*]` overrides and `[engines.*]` become the machine's cards and engines (all
   shared, as they were), its `[bridge]` model folder becomes the server's. The file is not
   changed or deleted. Later edits to it are ignored unless the Advanced override is on.
8. **Router side, unchanged in spirit.** Bridge servers stay read-only in the router admin.
   Router admins can turn a bridge off, set its priority, and now also turn off individual
   shared engines of a bridge (`bridges.json` `engines_off`); an engine turned off is left out of
   the overlay, so it is never placed on.

## 3. Translation: `machine/router.toml` to the bridge config

| router.toml | bridge config |
|---|---|
| `[servers.<id>]` `title`, `model_dir`, `comfy_root` | `[bridge]` `id` (Bridge: id wins when set), `title`, `model_dir`, `comfy_root` |
| `[cards.X]` (`label`, `vram_gib`, `ram_gib`, `reserve_gib`, `engines`) | `[cards.X]` overrides of the detected card with the same id |
| `driver = "package"`, `package = { id, version, params }` | `package = "<id>"`, `params` |
| `driver = "docker"`, `image`, `run.docker`, `port` | `[engines.x.docker]` `image`, `args`, the card's port |
| `driver = "process"`, `process.command`, `run.command` | `[engines.x.process]` `command` (argv), `cwd` |
| `driver = "external"` | an engine that is never started (`external`) |
| `health.path`, `caps`, `api` | `health`, `caps`, `api` |
| `port = { GPU0 = 18188 }` | `ports` |

Engines whose command line needs a model (`{model_path}` templates, LLM engines that a layout
fills in) are refused by the translation with the reason shown on the page: in this version an
engine on this machine serves the model(s) its own command line names. Lifecycle is always
on demand here (`idle_stop_seconds` from the engine's `idle_stop_seconds` param, default 600).

## 4. What the app shows

Settings → **This machine** (admins, desktop app only; whether or not Migo mode is on) is its
own page, `machine.html`, in a frame: the router admin forms' transport points at
`/api/admin/machine` there, so it never meets the app's router-mode admin pages.

- **Inventory**: the router's Inventory forms on the one server (no Add server, no Bridged
  machines tab, no guide bar), with **Rename** for the title, Find GPUs, model folder with
  Browse, engines and models (kinds, roles, engines with primary, Hugging Face search and
  download into the model folder).
- **Running**: each engine on each card, its state, Start/Stop, shared or not, and why it cannot
  start (a failed image pull or install, fetched ahead of time).
- **Manual engines**: `engines.toml` as JSON (process, docker, external), saved with a revision
  and mirrored into the inventory as external engines labelled `abhoth.machine = "manual"`.
- **Engine store**: install from a store, and the Create package wizard, on this machine.
- **Sharing & bridge**: Migo mode's status (the router, connected or why not, remote config on
  or off), the share toggles, changes waiting for approval (Approve and apply / Refuse), and the
  change log ("Changed by router admin X …", Undo).

## 5. Interfaces

- `GET|POST|PUT|PATCH|DELETE /api/admin/machine/<rest>` → the in-process router's
  `/admin/api/<rest>` (admins of the web host only). `GET config` reports the revision of the
  file on disk (what a save is checked against).
- `GET /api/machine/status`: `{server, title, hostname, bridgeMode, remoteConfig, bridgeRouter,
  advancedFile, bridge: {connected, server, enabled, router, lastError, connects},
  engines: {engines: [{engine, card, method, lifecycle, port, state, error, active,
  idle_seconds, up_seconds, starts, prepare}]}, problems, share: {engines, models}, rev,
  pending: [{id, at, who, op, reasons}], remoteChanges: [{id, at, who, what, undone}]}`.
- `GET|PUT /api/machine/engines`: `{rev, engines}` (412 on a stale `rev`).
- `PUT /api/machine/share`: `{engines: {id: bool}, models: {id: bool}}`.
- `POST /api/machine/pending/<id>` `{approve}`, `POST /api/machine/changes/<id>/undo`,
  `POST /api/machine/engines/start|stop` `{engine, card}`.
- `/api/machine/v1/*`: OpenAI-compatible (models, chat completions, audio, images), placed as
  in 2.5, holding the engine's lease while the answer streams; `x-abhoth-engine` says where it
  ran.
- The bridge descriptor (ROUTER-MEDIA-PLAN.md 13.2) stays v1: unshared items are simply absent,
  and a new optional `models` list (`[{id, kind, engines}]`) says which shared models the bridge
  has; routers that do not read it ignore it.
- Free RAM for engine starts counts reclaimable ZFS ARC (ROUTER-MEDIA-PLAN.md 13.1; `[limits]
  count_zfs_arc`, `arc_margin_gib`).

## 6. Phases

1. The machine folder, the in-process router, `/api/admin/machine`, the page (Inventory, Engine
   store, Engines), migration. Builtins that shelled out (`nvidia-smi`, `cpu-info`) run without
   a shell, so Find GPUs and the CPU card work on Windows.
2. Translation and the shared supervisor; Migo mode reports the machine's shared inventory;
   share toggles; status. Router: per-bridge engines on/off.
3. Local serving: `/api/machine/v1`, placement, the model picker.

## 7. Risks

- A saved change to the cards or engines rebuilds the supervisor (its engines stop) and
  reconnects the bridge, about a second. A remote change is applied half a second after its
  answer is sent, so the router admin still gets the result.
- **The router's forms assume Linux paths** in places (`/srv/models` placeholders, `clean_path`
  wants an absolute path; `C:/…` is accepted on a Windows host). Paths are shown and stored with
  forward slashes.
- **Docker on Windows** needs Docker Desktop; package and docker engines fail to start without
  it, with docker's own error shown. Process engines need nothing.
- **Two supervisors on one PC** (the desktop app's and a separately run `abhoth migo`) would
  fight over ports; the page warns when the bridge state folder is locked by another process.

## 8. Remote config: "Allow router to config" (added 2026-11-27)

A Bridge-mode setting, **off by default** (`desktop.bridgeRemoteConfig`).

- **Off:** the bridge's server is read-only in the router admin, as before (enable, priority,
  engines on/off). The router shows "Read-only: this machine doesn't allow remote config".
- **On:** a router admin edits this machine's inventory through the bridge: cards and their
  engines per kind, engines, Engine store installs and wizard packages, models (Hugging Face
  downloads into the machine's model folder), share toggles.
- **Applied by the bridge.** The router never writes the PC's files. It sends `config.apply`
  with form edits and the revision it read; the app runs them through the same in-process router
  as a local edit (check, diff, backup, atomic write). A stale revision answers 412 with the
  current revision, and nothing is written.
- **Audit on both sides.** The router audits the call with the admin's identity. The app writes
  `machine/remote-changes.jsonl` (who, when, summary, the backup it made) and shows the recent
  ones on the page ("Changed by router admin X at …") with **Undo**, which restores that backup
  through the same save path.
- **Safety:**
  - An edit that turns on anything that needs approval locally (privileged, host network, host
    mounts, a host-process or pip/binary/git package method; the engine store's approval list)
    is not applied: it is parked as **pending approval**, and the page asks the user to approve
    or reject it. The router gets `{"pending": id}`.
  - Edits may not touch the bridge's own router URL, token, id or this setting (they are not in
    `machine/router.toml` at all, so there is nothing to edit), nor `[router]`, `[control]`,
    `[access]` or `[secrets]` of the machine file.
  - Downloads and model folders must stay inside the server's model folder.

### 8.1 Protocol (bridge protocol v1, additive; ROUTER-MEDIA-PLAN.md 13.1-13.2)

- Descriptor: `"remote_config": true` (omitted when false) and `"config_rev"` (the machine
  file's revision, so the router notices a change).
- The router's admin API: `ANY /admin/api/bridges/<id>/machine/<rest>`. `GET config` →
  `config.get`; `PUT config` (`If-Match: "<rev>"`, body `{text, reason}`) → `config.apply`;
  anything else → `machine.api {method, path, query, body, ifMatch, who}`. The bridge answers
  `{status, body}` and the router passes both through: `200`, `202 {pending, reasons}` (waits
  for the user on the PC), `403` (refused, or remote config off: `{readOnly: true}`), `412
  {currentRev}` (stale revision), `422` (the check failed). An offline bridge is `502`. Every
  call that is not a GET is audited on the router as `bridge.config` with the admin's identity,
  and logged on the machine with the same `who`.
- `config.apply` checks the new file against the old one (`remote_config_check`): `[router]`,
  `[control]`, `[access]`, `[secrets]`, upstreams, routes, server ids, `exec`/`host`/`vars` are
  refused; a model folder, ComfyUI root or browse roots change, a non-external non-package
  engine, or a package method other than docker/compose waits for approval.
- `machine.api` allows the inventory's read calls (`inventory`, `config/json`, detect, the
  engine store's reads, `inventory/browse`, `downloads`) and its writes (`machine/engines`,
  `machine/share`, HF downloads, `inventory/mkdir`, engine-store install/update/uninstall).
  Paths given (browse, mkdir, a download's folder) must be inside the model folder. A store
  install that asks for approvals, a new store, and a wizard test run wait for the user.
  Manual engines from a router (`machine/engines`) wait when they add a process, host
  processes, docker flags, publish-all or mounts outside the model folder.

## 9. Router admin: bridged machines, apart from servers

Bridged machines are not servers this router configures, so they are a group of their own
(2026-11-27, the user: "in inventory, bridged machines should be separate from servers"):
Inventory has **Servers & cards** (router.toml's servers only: its count, its empty state and
the guide bar's steps count only these) and, beside it, **Bridged machines** (its own count and
empty state, "No machines bridged yet", with how to bridge one). *Servers & layouts* has a
**Bridged machines** tab too. Each machine shows online/offline and last seen, title and id,
status and last seen, title and id, cards with GPU/CPU badges, engines (struck through when
this router does not use them), priority and enabled, and how many engines cannot start. It
opens a detail view badged "Managed by bridge · remote config allowed" or "· read-only":
enabled, priority, cards and the RAM engines may use, each shared engine with a *use it* toggle
(`engines_off`), its state, Start/Stop and why it cannot start, the models it shares, and, with
remote config, its `router.toml` edited through the bridge (412 asks to reload; 202 says it waits
for approval), else "Read-only: this machine doesn't allow remote config". Their cards appear
wherever cards are listed for placement, labelled with their bridge (`NAS-GPU0 (bridge nas)`),
and install pickers say "(bridged)"; they are never in router.toml, so never in the layout
editor. The separate
**Migo machines** tab (formerly Bridges) stays, for tokens only (mint, rotate, revoke): tokens are a different job from
describing a server, and folding them in would put secrets next to the inventory.
