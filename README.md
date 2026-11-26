# Abhoth

<img src="assets/abhoth/icons/abhoth-256.png" width="128" align="right" alt="Abhoth mascot">

**Abhoth** is the Rust successor of OpenOrca: the same autonomous coding agent for local models
(OpenAI-compatible servers, Strata/ninfer slots, LiteLLM), rewritten in Rust as one `abhoth`
binary: an interactive terminal session, one-shot `--print` runs with sub-agents, a web UI
(`--hostui`, React client in `client/`) and a desktop window (`--desktop`).

This repo was split out of `openorca` (the `rust/` folder and the web client, with their
history). The C# build in the `openorca` repo stays the parity reference and the rollback until it
is no longer needed; see [`CUTOVER.md`](CUTOVER.md).

Developed with AI assistance (Claude).

## Build

Needs the toolchain in `rust-toolchain.toml` (rustup installs it). The web client is embedded
from the checked-in `client/dist`; rebuild it with `npm ci && npm run build` in `client/`.

```sh
cargo build --release                # target/release/abhoth(.exe)
cargo build --release --features desktop   # + abhoth-desktop.exe (no console) and --desktop
abhoth                               # interactive session in the current folder
abhoth -p "fix the failing test"     # one prompt, then exit
abhoth --hostui                      # the web UI (prints a URL with a token; first run: setup)
abhoth --desktop                     # the web UI in its own window (feature "desktop";
                                     # one instance, tray Show/Quit, native folder dialog)
abhoth --help                        # the common options
```

Closing the desktop window hides it to the tray and agents keep running; tray Quit exits. Set
`desktop.closeToTray` to false (settings, Desktop tab) to quit on close instead. On Windows the
window has one title row, Chrome-style: the project tabs, the buttons and the window buttons share it
(drag the empty space to move, double-click to maximise, the usual minimise,
maximise and close buttons, Aero Snap); "Use system title bar" (`desktop.systemTitleBar`,
Desktop tab) switches back to the OS one at once. Linux and macOS use the OS title bar.

## Install and updates

Releases are on [GitHub Releases](https://github.com/madninjaskillz/abhoth/releases) (tags
`abhoth-v<version>`), packaged with [Velopack](https://velopack.io) by CI on Linux:

- **Windows:** `abhoth-<version>-win-x64-setup.exe`. One click, no admin rights, no questions:
  it installs `abhoth.exe` and `abhoth-desktop.exe` to `%LOCALAPPDATA%\Abhoth\current`, adds an
  "Abhoth" Start-menu shortcut to the desktop app, puts that folder on the user PATH and starts
  the app. Uninstall from Settings > Apps (the PATH entry goes too).
- **Linux:** `abhoth-<version>-linux-x64.AppImage` (the desktop app; needs WebKitGTK 4.1).
- `abhoth-<version>-win-x64.zip` holds the two bare exes, for running without installing.

**Auto-update:** the installed desktop app checks the releases in the background (at start, then
every 6 hours). When a newer one is out it is downloaded (a delta package when there is one), and
the tray menu shows "Update available: restart into <version>"; clicking it restarts into the
new version. Otherwise it is applied the next time the app starts. The state home
(`~/.abhoth`) is never touched by install, update or uninstall. Nothing is signed yet, so
SmartScreen may warn on the first download.

On a fresh install with no model configured, the desktop app and `--hostui` still start: open
Settings, LLM Server, and set the Base URL of your model server and its Models; the next prompt
uses them. The executables carry the mascot icon and version info.

## Name, state and compatibility

- **Binary and product name:** `abhoth` / "Abhoth" (`orca_core::brand`). The TUI header,
  `--version`, `--help`, the web UI (title, favicon, mascot, sign-in and token pages) and the
  desktop window all say Abhoth. The system prompt introduces the agent as Abhoth; prompt files
  on disk keep their shared text.
- **State:** `~/.abhoth` (config.json, sessions, prompts, projects, `abhoth.db` with the web UI accounts). On the first
  run without `ABHOTH_HOME`/`OPENORCA_HOME`, Abhoth copies `~/.openorca` there once (logs,
  checkpoints and `config.json.bak*` stay behind) and leaves the old folder untouched, so the C#
  build keeps working until it is retired. The file formats are unchanged.
- **Environment:** `ABHOTH_*` variables are read first, `OPENORCA_*` as the fallback
  (`ABHOTH_HOME`, `ABHOTH_PROJECTS_ROOT`, ...). Wire names the web client and bridges use
  (`X-OpenOrca-Token`, the `openorca` MCP server name of the bridges, log categories
  `OpenOrca.Usage`) keep their names for compatibility.
- **Mascot:** `assets/abhoth/abhoth.svg` (vector original) and `abhoth-reference.webp`; the
  PNGs, the multi-size `abhoth.ico` and the web client's `brand/` files are rendered from the SVG
  (resvg, then Pillow for the .ico and .webp files).

## Security: web UI accounts

The web UI (`--hostui`, `--desktop`, Docker) has accounts, kept in SQLite at
`<home>/abhoth.db` (`ABHOTH_HOME`, else `~/.abhoth`). Details and reasons: PORTING.md, D140.

- **First run.** With no account yet, every page redirects to `/setup`, which creates the
  admin. Setup needs the setup code, which is the launch token in the URL the server prints
  (`http://127.0.0.1:8192/?token=...`). Opening that URL fills the code in, and the desktop
  window does it by itself. Only one setup can succeed, even when two run at once. For headless
  or Docker use, create the admin from the command line or the environment:
  ```sh
  abhoth admin create <name>               # password typed twice, or one line on stdin
  ABHOTH_ADMIN_USER=<name> ABHOTH_ADMIN_PASSWORD_FILE=/run/secrets/admin-password abhoth --hostui ...
  abhoth admin reset-password <name>       # also ends that user's sessions
  abhoth admin reset-2fa <name>            # abhoth admin list shows the users
  ```
- **The desktop app.** The first start makes a local admin named after your OS user, with no
  password, and signs in as it. This account signs in only through the desktop window, and
  password sign-in refuses it until a password is set: on the Account page, or with
  `abhoth admin reset-password <name>`. If the same home is served with `--hostui-login`, the
  setup screen asks for that password first. `--hostui` servers still show the setup (D142).
- **Users.** Admins create, disable and delete users, reset their passwords and 2FA, grant
  admin rights, and read the audit log (Users page). Each user has their own projects and their
  chats in them. The server checks ownership on every endpoint and on the hub websocket:
  someone else's project answers "no such project". Admins see every project. Only admins
  change server and global settings, use the bench and the shared GitHub sign-in, and switch
  the inference server's layout. Users who aren't admins open folders only inside the
  projects locations (`ABHOTH_PROJECT_ROOTS` / `ABHOTH_PROJECTS_ROOT`), never in the state home.
- **Sign-in.** Passwords are hashed with Argon2id. The only rules are 12 to 1024 characters,
  with no composition rules. Repeated failures lock the account name and the client IP with a
  doubling backoff. Wrong usernames and wrong passwords get the same answer after the same work.
- **Sessions.** Sessions live on the server: a random 256-bit cookie, stored only as a hash.
  The cookie is HttpOnly and SameSite=Lax, and `__Host-`/Secure over HTTPS. Sessions end after
  12 hours idle or 7 days. The Account page lists your sessions and can sign out one or all of
  them.
- **Requests.** State-changing browser requests need the session's CSRF header and an Origin
  from this site. Websockets need that Origin as well.
- **2FA.** Optional TOTP per user: enrol with a QR code, then confirm a code. You get ten
  single-use recovery codes, shown once. `--hostui-require-2fa` makes 2FA mandatory.
- **API keys.** Each user can make API keys (`abk_...`) for scripts and the CLI, sent as
  `Authorization: Bearer <key>`. A key is shown once, stored hashed, and revocable.
- **The launch token.** On a local `--hostui`, the printed URL signs in as the first admin, as
  before. The desktop window does the same without showing the token, and its session lasts
  while the app runs. `--hostui-login` marks an exposed server: there the token only unlocks
  the first-run setup, and plain HTTP is refused except from this machine (HTTPS through
  `--hostui-trust-proxy`). Being on loopback grants nothing by itself, because behind a reverse
  proxy every request comes from loopback.
- **Upgrading.** The old single sign-in account (`<home>/webui/account.json`) is imported as
  the first admin, keeping its password and 2FA. The password is re-hashed with Argon2id at the
  next sign-in. The file is left as it was, so a rollback still works. Projects from before
  accounts keep their ids and belong to the admins.
- **Limits.** Accounts separate users in the web UI and its API, not in the operating system.
  Agents' shell tools and the terminal run as the server's OS user, so a user's agent can read
  anything that user can. Background processes, the task list and Pi jobs are kept per session,
  so agents can't list or stop each other's. Give accounts only to people you would trust with
  a shell on the server.

## Docker

The simplest way to run Abhoth on a server is **one image, one container**: the root
`Dockerfile` builds `abhoth` with the web UI, the router (gateway and control plane) and the
supervisor (telemetry hub, Home Assistant, the Glance page's feed). They run in one process, each
on its own thread pool, ports and config; only the web UI is on unless you configure the other
two (D148).

### Getting started

```sh
git clone https://github.com/madninjaskillz/abhoth && cd abhoth
mkdir -p abhoth-data/{web,router,supervisor,projects,secrets} && sudo chown -R 568:568 abhoth-data
docker compose up -d --build                                  # docker-compose.yml
docker compose exec abhoth /app/entrypoint.sh admin create <user>   # the first admin
```

Open `http://<host>:8093` and sign in. The model server is set in the web UI's settings; a key
it needs goes in `abhoth-data/secrets/strata-api-key`.

- **Networking:** on Linux the compose file uses host networking (the router's ports and the
  model servers on your LAN work without a port list). On Docker Desktop, remove
  `network_mode: host` and publish the ports instead (`8093`, and the router's `8010`, `8092`,
  `8098`, `8099` if you use it).
- **Volumes:** `/data/web` (accounts, sessions, Claude/Codex logins), `/data/router`,
  `/data/supervisor`, `/data/projects` (or one volume at `/data`) and read-only `/run/secrets`. Everything runs as uid 568 on a
  read-only root filesystem.
- **The router:** set `ROUTER_ARGS` to its flags, as for `abhoth router`, e.g.
  `--config /data/router/router.json --port 8010 --control /data/router/router.toml`.
  [`ROUTER.md`](ROUTER.md) and [`ROUTER-CONFIG.md`](ROUTER-CONFIG.md) describe the config;
  `abhoth router import` writes a `router.toml` from existing layout files.
- **The supervisor:** set `SUPERVISOR_CONFIG` to its file (example:
  `crates/orca-supervisor/deploy/supervisor.example.toml`). The Glance page then follows its
  feed (`ABHOTH_SUPERVISOR_URL`, default `http://127.0.0.1:9890`; `GLANCE=0` turns it off).
- **Health:** the image's health check asks every part that is on (`deploy/abhoth/healthcheck.sh`).
  A router or supervisor that fails (a startup error such as a missing secret, an error later on,
  or a panic) is restarted inside the container with backoff (1 s doubling to 60 s) while the
  other parts keep serving; the container is unhealthy meanwhile, and `/api/parts` (admins)
  shows each part's state, restarts and last error. Only the web UI stopping exits the container.
- **Fans and clocks (optional):** `abhoth-hostd` needs root, so it is installed on the host, not
  in the container: [`crates/orca-hostd/deploy/install.sh`](crates/orca-hostd/deploy). Mount
  `/run/abhoth` into the container and add the socket's group (`group_add`) so the supervisor can
  reach it.
- **Build options:** `--build-arg FEATURES=claude-engine` adds the router's Claude engine
  ([`ROUTER.md`](ROUTER.md) 12); `CLAUDE_CODE_VERSION` and `CODEX_VERSION` pin the CLIs.

### The separate images (advanced)

The parts also ship alone, for setups that split them across hosts:

- `deploy/docker/` is the web UI image: `docker build -f deploy/docker/Dockerfile -t local/abhoth-web:<n> .`
  and `deploy/docker/compose.yaml`. Its state is under `/state`.
- `deploy/router/` is the router-only image (`abhoth-router`).
- The supervisor runs from the `abhoth-supervisor` binary or as `abhoth supervisor run --config FILE`.
- The entrypoint runs `abhoth --hostui=0.0.0.0:$PORT --hostui-login`; `/app/openorca` is a
  symlink to `/app/abhoth`, so `docker exec <c> /app/openorca --hostui-set-password <user>` still
  works (it now sets that account's password in `abhoth.db`; `abhoth admin ...` is the new form).
- `OPENORCA_HOME=/state/openorca` is still honoured (fallback for `ABHOTH_HOME`), so an
  existing OpenOrca container state is used as is; no migration.
- Rollback: point the app back at the previous OpenOrca image; the state formats are shared.
  The full procedure is in [`CUTOVER.md`](CUTOVER.md), section 4.

## Tests and the cross-check

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace            # add --all-features for the desktop feature
```

`cargo test` needs nothing outside this repo: the C# fixtures it compares against are checked
in (`crates/*/tests/fixtures`, `crates/orca-core/tests/data`). The cross-check
(`crates/orca-crosscheck`, scenarios in `crosscheck/scenarios`) runs the same scenarios
through Abhoth and the C# CLI and diffs them, so it is not part of `cargo test` or CI. It
needs the C# reference, which lives in the `openorca` repo (kept for the rollback period):
build its Release CLI there, then
`cargo run -p orca-crosscheck -- --csharp <openorca>\src\OpenOrca.Cli\bin\Release\net10.0\openorca.exe --rust target\debug\abhoth.exe`.

Porting notes, decisions and the test setup: [`PORTING.md`](PORTING.md); progress and gaps:
[`STATUS.md`](STATUS.md).

## Licence

MIT ([`LICENSE`](LICENSE)). Abhoth is a port of OpenOrca (MIT, Copyright (c) 2025 OpenOrca
Contributors); its notice is kept in `LICENSE`.
