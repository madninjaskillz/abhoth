# Abhoth

<img src="assets/abhoth/icons/abhoth-256.png" width="128" align="right" alt="Abhoth mascot">

**Abhoth** is the Rust successor of OpenOrca: the same autonomous coding agent for local models
(OpenAI-compatible servers, Strata/ninfer slots, LiteLLM), rewritten in Rust as one `abhoth`
binary. The C# build (`openorca`) stays the parity reference until it is retired; see
[`CUTOVER.md`](CUTOVER.md).

```sh
cd rust
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
pwsh rust/installer/build.ps1                     # -> rust/target/installer/abhoth-<version>-x64.msi
msiexec /i rust\target\installer\abhoth-0.15.0-x64.msi              # or double-click it
msiexec /i rust\target\installer\abhoth-0.15.0-x64.msi ADDTOPATH=0  # without the PATH entry
msiexec /x rust\target\installer\abhoth-0.15.0-x64.msi              # uninstall (or Settings > Apps)
```

`build.ps1 -NoCargo` packages the binaries already in `target\release`. The workflow
`.github/workflows/abhoth-installer.yml` builds the MSI on a tag `abhoth-v*` or when run by hand
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

Porting notes, decisions and the test setup: [`PORTING.md`](PORTING.md); progress and gaps:
[`STATUS.md`](STATUS.md).
