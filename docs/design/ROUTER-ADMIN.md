# The router's own admin pages (D151)

User request (2026-11-26): "it feels like you might run the router without abhoth on the server,
so it should have its own webui — but that should be accessible if you are connected to the
router and have admin." And: editing the router's setup (servers, cards, engines, slots, layouts,
the model list) should be possible with proper forms, not only raw TOML/JSON over SSH.

## 1. Where the pages live

- **On the gateway's port, under `/admin/`** (`http://<router>:8010/admin/`). No second port to
  open, publish or proxy; the gateway already listens where clients reach it, and `/admin/` was
  already the router's admin prefix (`/admin/api/*`, the legacy `/admin/reload`). The page is the
  React client's second bundle (`client/router-admin.html` → `client/dist-router`, built by
  `vite.router.config.ts` from the same `src/`: the app's router panes, forms, components and
  styles), embedded in the router binary with `rust-embed` (`crates/orca-router/build.rs`,
  `ABHOTH_ROUTER_UI_DIST`). A router built without the folder still builds; `/admin/` then
  answers 503 saying so.
- **Every URL in the page is relative** (`base: "./"`: assets, `session`, `login`, `api/...`),
  so the page works at `/admin/`, behind a reverse proxy under any prefix
  (`https://example.org/router/admin/`), and through the Abhoth app's proxy (section 4).
  `GET /admin` redirects to `admin/` (relative, keeping a proxy's prefix).
- **Headers:** `Content-Security-Policy` (`default-src 'self'`, scripts from the page only, styles
  inline allowed for the components, `frame-ancestors 'self'`, `object-src 'none'`), `nosniff`,
  `no-referrer`; hashed assets are cached for a year, the page and every API answer are
  `no-store`.
- **They need `--control router.toml`.** With `control.enabled = true` they sit on the control
  plane (switching, live state, jobs). With `control.enabled = false` the router installs an
  **admin-only plane**: `router.toml` and `router.json` editing, admin keys, sign-in, the audit;
  plans and switches answer 501 and nothing switches. A first-time standalone setup is: a minimal
  `router.toml` (`version = 1`, `[router] state_dir`, `[control] enabled = false`), add servers,
  cards, engines and layouts with the forms, then turn the control plane on. `abhoth router
  init` writes that starting point (section 6). Without `--control`
  the page says how to start the router for it.

## 2. Who is an admin (router-native auth)

The router decides itself; the Abhoth web server is not needed.

- **Router admin keys** (`abk_…`, the same shape as the web server's keys) are kept in the
  router's `router.db` (`router_keys`: name, SHA-256, a 10-character prefix, who made it, last
  use, revoked). Only the hash is stored; a key is shown once. A local key is always an admin
  key. They are checked before the web server's keys (no network) and a revoked one stops at
  once (its sessions end on their next request).
- **First key:** `abhoth router admin-key create <name> --control router.toml [--out FILE]`
  (`--out` writes it to a new file, mode 600, instead of printing it; in Docker,
  `docker exec <c> /app/abhoth router admin-key create …`). `admin-key list` and `admin-key
  revoke <id|name>` too. While no key exists the router says so in its log at start-up. Admins
  can also make and revoke keys on the page (Admin keys).
- **Sign-in** (`POST /admin/login`, JSON only, so a cross-site form can't post it): the router's
  own keys, or an admin's key the linked Abhoth web server vouches for (introspection, as for
  every key). A good key gives a **session cookie** `abhoth_router_session`: HttpOnly,
  SameSite=Strict, `Secure` when the request came over HTTPS (directly or with
  `X-Forwarded-Proto: https`), no `Path` (the browser scopes it to the folder the sign-in was
  posted from: `/admin` or a proxy's prefix), 8 hours at most and 2 hours idle, in memory (a
  restart signs everyone out). State-changing calls with the cookie need the session's CSRF
  token in `x-router-csrf` (`GET /admin/session` gives it to the page).
- **Rate limit:** 10 failed sign-ins per client address in 15 minutes, 100 from everyone, then
  429 with `Retry-After`. Behind a reverse proxy every visitor shares the proxy's address (the
  router never trusts `X-Forwarded-For` for identity), so the per-address limit acts as a global
  one there.
- **Audit:** `admin.sign-in` (who, from where), `admin.sign-in-refused` (the address and why; the
  key never), `admin-key.create` / `admin-key.revoke`, `config.save`, `gateway.save`.
- **Still accepted:** a bearer admin key on any `/admin/api/*` call (scripts, the app's proxy;
  no CSRF needed: a bearer key is not ambient), the web server's signed acting user (the Abhoth
  web app's own Router pages: its admins stay admins), and loopback (`loopback_admin`).
- **Loopback is no longer admin through a proxy:** a loopback request carrying
  `X-Forwarded-For`, `Forwarded` or `X-Real-IP` is treated as the anonymous caller it stands for,
  so a reverse proxy on the router's host does not make every visitor an admin.

## 3. The forms

The page's tabs: Overview (cards, layouts, plan and switch, as in the app), **Servers &
layouts**, **Model list**, Routes, **Advanced** (`router.toml` in the Monaco editor, as before),
Jobs & audit, **Admin keys**.

- **`router.toml`** (Servers & layouts): servers (id, title, host, SSH or local commands, SSH
  user, port, key as `secret:<id>`, pinned host key, variables, locks), their cards (GPU UUID,
  label, memory, reserve, CPUs, managed, on-demand; and what each layout runs on the card with
  its lanes), engines (driver, image, container, served id, context, per-card ports) with their
  **profiles** (lanes = slots per card, context, memory, build), and layouts (per card: not in
  the layout, off, same as the card before, or engine / profile / model). Fields a form does not
  show (hooks, run commands, health checks…) are kept.
  - The page reads the main file as data (`GET /admin/api/config/json`) and sends what it
    changed as **edits** (`POST /admin/api/config/edit` `{edits: [{path, value}]}`: a value at a
    path, `null` removes; objects merge key by key). The router applies them to the file's text
    with `toml_edit` (comments, order and formatting of the untouched parts kept; new sections
    as `[servers.x]`, nested maps dotted or inline) and checks the result with its own loader and
    validator. Nothing is written by an edit: the page shows the diff, problems and consequences
    and saves the returned text with the existing save (`PUT /admin/api/config` with If-Match:
    backup, atomic write, audit, reload). A text save and a form save are the same thing.
  - Only the main file is shown; sections in `include`d files are edited in Advanced.
- **`router.json`** (Model list): the model names (cards they balance over, or the on-demand
  relay; served id; context and output limits; vision, tool calling, reasoning; effort levels
  and what "thinking off" maps to; weight; request defaults; dropped fields; other `modelInfo`
  keys kept), the upstreams (card id, engine URL, engine, served id, lanes, key **file**,
  online), the Claude block (enabled, access, sessions, context, output limit, routes) and slots
  (on, lease TTL), plus the whole file as text.
  - `GET /admin/api/gateway` (text, data, rev = the text's SHA-256 prefix, the router's last load
    error, writable, whether the control plane overlays it), `POST /admin/api/gateway/check`
    (problems from the router's own loader, `Snapshot::build`, plus a refusal of anything that
    looks like a literal key, never echoed; and the diff), `PUT /admin/api/gateway` (If-Match;
    an invalid file is never written: 422; backup to `<dir>/backups/router.json.bak-<time>-<rev>`,
    the last 100 kept; atomic write; audit `gateway.save`; hot reload at once).
  - With the control plane on, the router overlays the cards' live engines and on-demand settings
    on top of the loaded file; the page edits the file itself and says so.
- **No form switches anything.** A saved layout change to the active layout shows as drift with
  a plan on the Overview; switching stays the plan/guard flow (ROUTER-CONFIG.md 5).
- **Validation in the page** (`client/src/features/router/forms/formModel.ts`, tested) catches the
  obvious early (ids, a GPU UUID's shape, ports, lanes ≥ 1, a server's SSH key must be a secret
  reference, an upstream's key must be a file, a layout's engine/profile/model must exist,
  nothing still in use is removed); the router's check is the one that counts.

## 4. In the Abhoth app

- **Router mode, admin key:** Settings → Models → Connection shows **Router admin** when the app
  is in router mode and the router says the key this server keeps for it is an admin's
  (`GET /api/settings/connection/router-admin` asks `/admin/api/whoami` with the key). "Open
  router admin" opens the router's pages in a dialog, through this server:
  `/api/settings/connection/router-admin/ui/*` → `<router>/admin/*` with the key added
  server-side (it never reaches the browser), no cookies either way, SSE streamed, no redirects
  followed, admins only. The page's `session` call is answered with the app's own CSRF header
  and token, so its saves pass the app's CSRF check; sign-in is not needed there. "Open in
  browser" links to the router's own `/admin/` (sign in there with an admin key). Non-admin keys
  and server mode do not see it.
- **The built-in router (single container, D148):** the app's existing admin tabs keep working
  through the signed proxy, and Router config now has the same forms (Servers & layouts, Model
  list) beside the TOML editor: the panes and forms are shared components; only where their
  calls go differs (`setRouterTransport`).

## 5. Tests

- Rust: `access::local_keys` (hash-only storage, revocation, names), `access::sessions` (8 h /
  2 h, rate limits, revocation ends sessions, cookie attributes), `control_api::form` (edits keep
  comments, merge, floats, new tables, removal, refusal), `admin_ui` (paths, literal keys,
  router.json problems), and `tests/admin_pages.rs` over HTTP: the page and its headers, loopback
  vs proxied, sign-in / wrong key / non-JSON / Secure behind HTTPS, CSRF, bearer keys, revocation
  ending a session, audit rows without keys, the rate limit, form edits checked and saved with
  If-Match and 412, router.json check / 422 / literal key / save / backup / hot reload / 412.
  orca-webui: the proxy's target (router mode, Abhoth key only).
- Client: `test/routerForms.test.ts` (form ↔ config, validation, edits, the model list, relative
  URLs) and `test/connection.test.ts` (when Router admin shows).
- CI: `npm run build` builds both bundles; the check that the checked-in `dist` matches a fresh
  build now covers `dist-router` too.

## 6. Standalone router

A router on its own (no Abhoth web server) starts from one command, `abhoth router init [DIR]`
(`crates/orca-router/src/init.rs`; usage in ROUTER.md 17). User request (2026-11-27): one
command, interactive by default with a `--yes` mode, that leaves a router ready to start and sign
in to.

- **What it makes**, in one folder: `router.toml` (`version = 1`; `[router]` id, listen, an
  absolute `state_dir`; `[control] enabled = false`, so the admin-only plane of section 1;
  `[inventory] version = 1`, so start-up has nothing to migrate), `router.json` (empty
  `upstreams` and `deployments`), the state folder, and the first router admin key.
- **This machine as the first server** (optional, default on): `[servers.local]`,
  `exec = "local"`, `host = "127.0.0.1"`; GPUs from `nvidia-smi` with the telemetry query and
  parser the router polls hosts with (`control/telemetry.rs`), as cards `A`, `B`, … by index,
  pinned by UUID, with name and VRAM; the CPU (`control/builtins.rs` `cpu_info`) as a `cpu`
  card. All `managed = false`: making a card managed is a decision for when its engines and
  layouts exist, and the control plane is still off anyway.
- **The key** is made with the same code as `admin-key create` (`access::LocalKeys`, audited as
  `admin-key.create` by `cli`) and written to a new owner-only file (`DIR/secrets/router-admin-key`,
  mode 600). It is shown only with `--print-key`; the summary names the file.
- **Then** the summary prints the exact start command (this binary's path, `--config`,
  `--control`, `--listen`) and the `/admin/` URL (loopback, and the host name when it listens on
  every address). Optional service files: a systemd unit, a compose file for the router-only
  image (the folder mounted at the same path, so `state_dir` holds inside the container), or a
  Windows hint (a start-up scheduled task or a service wrapper).
- **Idempotent:** it refuses, writing nothing, when any file it would write exists; `--force`
  moves each to `DIR/backups/<name>.bak-<time>` first. The state folder and `router.db` are kept.
- **Checked:** after writing, it loads and validates `router.toml` with the control plane's own
  loader and validator and `router.json` with `Snapshot::load`; any error is a bug in init and
  fails the command.
- **Tests:** `init::tests` (the files and their checks, cards from a fake detection, the key's
  file, mode and that it signs in, never printed without `--print-key`, refusal and forced
  backups, quoting, the questions and `--yes`, no terminal without `--yes`) and
  `tests/router_init.rs` (a router started from init's files answers `/admin/` and its key signs
  in).
