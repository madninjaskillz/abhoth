# Abhoth

<img src="assets/abhoth/icons/abhoth-256.png" width="128" align="right" alt="Abhoth mascot">

**Abhoth** is the Rust successor of OpenOrca: the same autonomous coding agent for local models
(OpenAI-compatible servers, Strata/ninfer slots, LiteLLM), rewritten in Rust as one `abhoth`
binary: an interactive terminal session, one-shot `--print` runs with sub-agents, a web UI
(`--hostui`, React client in `client/`) and a desktop window (`--desktop`).

This repo was split out of `openorca` (the `rust/` folder and the web client, with their
history). The C# build in the `openorca` repo stays the parity reference and the rollback for
about two weeks after the cutover; see [`CUTOVER.md`](CUTOVER.md).

Developed with AI assistance (Claude).

## Build

Needs the toolchain in `rust-toolchain.toml` (rustup installs it). The web client is embedded
from the checked-in `client/dist`; rebuild it with `npm ci && npm run build` in `client/`.

```sh
cargo build --release                # target/release/abhoth(.exe)
cargo build --release --features desktop   # + abhoth-desktop.exe (no console) and --desktop
abhoth                               # interactive session in the current folder
abhoth -p "fix the failing test"     # one prompt, then exit
abhoth --hostui                      # the web UI (prints a URL with a token)
abhoth --desktop                     # the web UI in its own window (feature "desktop";
                                     # one instance, tray Show/Quit, native folder dialog)
abhoth --help                        # the common options
```

Closing the desktop window hides it to the tray and agents keep running; tray Quit exits. Set
`desktop.closeToTray` to false (settings, Desktop section) to quit on close instead.

## Windows installer

A per-user MSI (no admin rights): it installs `abhoth.exe` and `abhoth-desktop.exe` to
`%LOCALAPPDATA%\Programs\Abhoth`, adds an "Abhoth" Start-menu shortcut to the desktop app, and
puts the folder on the user PATH.

```powershell
dotnet tool install --global wix --version 5.0.2   # once; per-user
pwsh installer/build.ps1                          # -> target/installer/abhoth-<version>-x64.msi
msiexec /i target\installer\abhoth-0.15.0-x64.msi              # or double-click it
msiexec /i target\installer\abhoth-0.15.0-x64.msi ADDTOPATH=0  # without the PATH entry
msiexec /x target\installer\abhoth-0.15.0-x64.msi              # uninstall (or Settings > Apps)
```

`build.ps1 -NoCargo` packages the binaries already in `target\release`. The workflow
`.github/workflows/installer.yml` builds the MSI on a tag `abhoth-v*` or when run by hand
and uploads it as an artifact. The state home (`~/.abhoth`) is never touched by install or
uninstall. The MSI is not signed yet, so SmartScreen may warn.

## Name, state and compatibility

- **Binary and product name:** `abhoth` / "Abhoth" (`orca_core::brand`). The TUI header,
  `--version`, `--help`, the web UI (title, favicon, mascot, sign-in and token pages) and the
  desktop window all say Abhoth. The system prompt introduces the agent as Abhoth; prompt files
  on disk keep their shared text.
- **State:** `~/.abhoth` (config.json, sessions, prompts, projects, web UI accounts). On the first
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

## Docker (web UI)

`deploy/docker/` holds the Linux image for the web UI: a headless release build of `abhoth`
on a runtime with the .NET SDK, Node and the Claude Code / Codex CLIs.

```sh
docker build -f deploy/docker/Dockerfile -t local/abhoth-web:<n> .
docker compose -f deploy/docker/compose.yaml up -d   # adapt volumes, ports and secrets first
```

- The entrypoint runs `abhoth --hostui=0.0.0.0:$PORT --hostui-login`; `/app/openorca` is a
  symlink to `/app/abhoth`, so `docker exec <c> /app/openorca --hostui-set-password` still works.
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
