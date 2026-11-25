# Cutover: OpenOrca (C#) to Abhoth (Rust)

A plan, not yet executed. Every step marked **(OK needed)** waits for the user's go-ahead:
the merge to main, anything on DeepThought, and deleting C# code.

## 0. Gate: parity evidence

Before anything else:

1. **Tests:** `cargo test --workspace`, `cargo clippy -D warnings` and `cargo fmt --check` are
   green. The C# suite (`dotnet test`) is green on the same commit.
2. **Cross-check:** every scenario in `rust/crosscheck/scenarios` is identical, in two
   consecutive runs.
3. **Bench parity run.** This is the one thing tests can't show: same tasks, same model, same
   slots, C# vs Rust. The user schedules it on the bench kit and the DeepThought slots. The exact
   run is in STATUS.md under "Bench parity run".
   - **Pass:** Rust is no slower in median wall-clock per task (within the noise of 3 runs).
   - **Pass:** the pass rate is equal or higher.
   - **Pass:** token totals are within about 10%.
   - These are the "Speed priorities" and "Branch merge rule" criteria.
4. **Live smoke:** the REPL, `--print` with sub-agents, `--hostui` with the React client, and
   `--desktop`, all against LiteLLM `qwen3.8-27b`.

## 1. Merge `feature/rust-port` to main (OK needed)

- Rebase or merge `main` into `feature/rust-port` first, then run gate items 1 and 2 again.
- Merge with `--no-ff`. C# stays in the tree. Both builds ship from main until step 6.
- **Keep `rust/` where it is for now.** Moving it to the root at the same moment as the merge
  would mix a 1,000-file move into the diff that matters. It would also break every
  `../src/OpenOrca.WebUi/client/dist` path, the embed in `orca-webui`, and open branches.
- Move it in step 6 instead, when C# is deleted. The repo root then becomes the Cargo
  workspace, `src/OpenOrca.WebUi/client` moves to `web/`, and the rust-embed path changes
  with it.

## 2. CI

- **Today:** the `rust` job in `.github/workflows/ci.yml` runs test, clippy and fmt on Linux
  and Windows.
- **Add on merge:**
  - **Cross-check:** a job on `windows-latest` that builds the C# Release CLI and runs
    `orca-crosscheck`. It needs no network: the scenarios use fake servers.
  - **Desktop:** done (D114): the `rust-desktop` job runs clippy and the desktop unit tests
    with `--features desktop` on Windows, Linux (WebKitGTK/GTK/dbus dev packages) and macOS.
    No window is opened.
  - **Client:** `npm ci && npm run build && npm test`, failing when `client/dist` differs from
    what is checked in.
- **Release workflow** (tag `abhoth-v*`):
  - Windows: done in part (D115). `abhoth-installer.yml` builds `abhoth.exe` and
    `abhoth-desktop.exe` (`--release --features desktop`) and the MSI, and uploads the MSI and
    its SHA-256 as a workflow artifact. Still to add: attaching it to a GitHub Release.
  - builds `abhoth` (headless) for x86_64-unknown-linux-gnu;
  - attaches them to a GitHub Release with SHA-256 sums.
- **Retire** the .NET jobs in step 6, not before.

## 3. Release and install on Windows

- **Artifact:** `abhoth-<version>-x64.msi` (D115, `rust/installer/`), a per-user MSI built
  with WiX v5 (`rust/installer/build.ps1`; CI: `abhoth-installer.yml`). It holds `abhoth.exe`
  (console; embeds the web client) and `abhoth-desktop.exe` (no console, the shortcut target).
  WebView2 is part of Windows 11, and `--desktop` says so clearly if it is missing.
- **Install:** double-click the MSI, or `msiexec /i abhoth-<version>-x64.msi [ADDTOPATH=0]`.
  - Files go to `%LOCALAPPDATA%\Programs\Abhoth`; the folder is added to the user PATH unless
    `ADDTOPATH=0`; a Start-menu shortcut "Abhoth" runs `abhoth-desktop.exe` with the mascot icon.
  - No admin rights are needed. A newer MSI replaces the older version in place.
  - **Uninstall:** Settings > Apps, or `msiexec /x`. It removes the files, the shortcut and the
    PATH entry, and leaves `~/.abhoth` alone.
  - `winget`/Scoop manifests later.
- **Not yet:** code signing (SmartScreen warns on an unsigned MSI) and attaching the MSI to a
  GitHub Release.
- **First run:** the exe copies `~/.openorca` to `~/.abhoth` once (D89). The C# install keeps
  working side by side because the binaries have different names and homes.
- **Later:** signing, MSIX, and auto-update, only if they're wanted.

## 4. DeepThought web container (OK needed)

**Today:**
- The TrueNAS custom app `openorca-web` runs container `ix-openorca-web-openorca-1`.
- **Image:** `local/openorca-web:<n>`, built from a source tarball unpacked in
  `$AI_ROOT/orca/build/src<n>`, with
  `docker build -f deploy/docker/Dockerfile -t local/openorca-web:<n> .`
  (logs in `build<n>.log`; the last one is :25).
- **Image contents:** the SDK 10 runtime, Node, and the Claude Code and Codex CLIs.
- **Entrypoint:** `deploy/docker/entrypoint.sh` runs `openorca --hostui=0.0.0.0:8093
  --hostui-login` with bench flags.
- **Environment:** `OPENORCA_HOME=/state/openorca` and `OPENORCA_PROJECTS_ROOT=/projects`.

**Plan:**
1. Add `deploy/docker/Dockerfile.abhoth`:
   - **Build stage:** `rust:<pinned>-bookworm`, then
     `cargo build --release -p orca-cli --no-default-features --features <headless set>`.
   - **Runtime:** the same as today. Keep the SDK image, because the agent still builds .NET
     projects. Keep Node and the CLIs too.
   - Copy `target/release/abhoth` to `/app/abhoth`, and keep `/app/openorca` as a symlink to
     it so that `docker exec … --hostui-set-password` habits keep working.
   - The entrypoint gets `BIN=${BIN:-/app/abhoth}`.
   - The environment keeps `OPENORCA_HOME=/state/openorca`. The fallback reads it, so the
     container needs no migration and keeps the accounts, sessions and bench state.
2. Build the image as `local/abhoth-web:1` next to the old one, the same way as today: a
   tarball into `src<n>` and a log in `build<n>.log`.
3. **Trial:**
   - Run a second app or compose project on another port, for example `8094`, with copies of
     `/state`: `cp -a $AI_ROOT/orca/state $AI_ROOT/orca/state-abhoth`. Never
     the live one.
   - Sign in, open a project, chat, run one bench item.
4. **Switch:**
   - Point the `openorca-web` app at `local/abhoth-web:1` with the real `/state`. Port 8093
     and the npm proxy stay unchanged.
   - Keep the old image tag.

**Rollback:** set the app's image back to `local/openorca-web:25` and restart it. The state
formats are shared, so nothing needs converting.

## 5. Bench and tooling

- The bench kit (`deploy/bench/benchkit.py`, `bench/*.py`) calls the CLI by path. Point it at
  `abhoth` with a variable, or use the symlink above.
- `results.jsonl` is unchanged (cross-checked).

## 6. Retire C# (OK needed, after a period on Abhoth)

- Delete `src/`, `tests/`, `OpenOrca.sln` and the .NET CI jobs.
- Move `rust/*` to the root, and `src/OpenOrca.WebUi/client` to `web/` (step 1 explains why).
- Drop the cross-check crate, or keep it frozen with recorded C# outputs as golden files.
- Remove the dual names that remain only for C#:
  - the `OPENORCA_*` fallbacks: keep them one more release, with a warning;
  - the `X-OpenOrca-*` header names (change client and server together);
  - the bridges' MCP server name (`openorca`, then `abhoth`).
- Rename the GitHub repo or leave a redirect. That is the user's call.
- Optionally, offer to delete `~/.openorca` after confirming that `~/.abhoth` is in use. Never
  delete it automatically.

## Rollback (any stage)

- **Before step 6:** C# is untouched and still builds from main. Users run `openorca` again,
  and its `~/.openorca` is unchanged, because the migration copies.
  - Sessions created under Abhoth stay in `~/.abhoth`. They can be copied back by hand: the
    formats are identical.
- **Container:** see step 4.
- **After step 6:** revert the deletion commit. Keep the last C# release artifacts attached to
  their GitHub Release.
