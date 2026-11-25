# Cutover: OpenOrca (C#) to Abhoth (Rust)

A plan, not yet executed. The user's decisions (2026-11-25):

- **All at once, once the bench passes.** On one day: Abhoth gets its own repo and becomes the
  product, the DeepThought web container switches to Abhoth, and Abhoth is installed on the
  desktop.
- **Abhoth lives in a new private GitHub repo, `abhoth`,** under the same owner as `openorca`.
  It keeps the port's history: `rust/` with its commits becomes the repo root (the Cargo
  workspace), and the React client comes along as `client/` with its own history.
- **C# stays for about two weeks as the rollback,** in the `openorca` repo, untouched. After
  that the `openorca` repo is archived. Its C# code is not deleted commit by commit; the
  archive is the record.

Every step marked **(user triggers)** waits for the user. Agents prepare and test, but never
create the repo, push to it, or change DeepThought.

## 0. Gate: parity evidence

1. **Tests:** `cargo test --workspace --all-features`, `cargo clippy -D warnings` and
   `cargo fmt --check` are green. The C# suite (`dotnet test`) is green on the same commit.
2. **Cross-check:** every scenario in `rust/crosscheck/scenarios` is identical, in two
   consecutive runs.
3. **Bench parity run** (STATUS.md, "Bench parity run"): same tasks, model and slots, C# vs
   Rust.
   - Rust is no slower in median wall-clock per task (within the noise of 3 runs).
   - The pass rate is equal or higher.
   - Token totals are within about 10%.
4. **Live smoke:** the REPL, `--print` with sub-agents, `--hostui` with the React client, and
   `--desktop`, against LiteLLM `qwen3.8-27b`.

## 1. The `abhoth` repo (user triggers)

The split is scripted and tested locally: `rust/tools/split-abhoth-repo.sh` (D131).

```sh
python -m pip install --user git-filter-repo        # once
rust/tools/split-abhoth-repo.sh <openorca clone> <empty dir> feature/rust-port --build
```

- It clones the branch (never touching the source clone) and runs `git filter-repo`:
  - `rust/` becomes the root;
  - `src/OpenOrca.WebUi/client/` becomes `client/`;
  - everything else, C# included, is dropped from every commit.
- The branch becomes `main`, and no tags are carried over.
- **Checks:**
  - the expected layout;
  - no Anthropic key prefix anywhere in `git log -p --all`;
  - no path points back into the openorca tree.
- With `--build`, it also runs fmt, clippy and the full workspace tests in the split repo.
- **Paths that work in both layouts:**
  - The client embed (`orca-webui/build.rs`) finds `client/dist` in the abhoth layout and
    `src/OpenOrca.WebUi/client/dist` in openorca. `ABHOTH_CLIENT_DIST` overrides it.
  - The bridge test fixtures live in `crates/orca-core/tests/data/engines` (copied from the
    C# tests).
- **What the user runs at cutover**, after the bench passes, on the commit that passed the
  gate:
  1. Run the split from `feature/rust-port` (or from `main`, if it was merged there first).
  2. Create the private repo `abhoth` under the openorca owner, e.g.
     `gh repo create <owner>/abhoth --private`.
  3. In the split dir: `git remote add origin …`, then `git push -u origin main`.
  4. Make the default branch `main` and protect it like openorca's.
- **Carried over by hand** (not in `rust/` today, so not in the split):
  - The bench kit: `bench/*.py` and `deploy/bench/`, without results, into `bench/`.
  - The web container files: `deploy/docker/` adapted to Abhoth (step 3).
  - Both come in as one commit, "Bring the bench kit and container files from openorca".

## 2. CI in the abhoth repo

- `.github/workflows/` moves into the new repo. The `rust` jobs of openorca's `ci.yml` become
  its CI, with paths relative to the root:
  - test, clippy and fmt on Linux and Windows;
  - the desktop feature on Windows, Linux and macOS;
  - the installer workflow `abhoth-installer.yml` (D115).
  `rust/.github/` holds the file so that the split carries it; it is inert in openorca.
- **Client:** `npm ci && npm run build && npm test` in `client/`, failing when `client/dist`
  differs from what is checked in.
- **Cross-check:** it needs the C# build, which is not in the new repo. During the rollback
  weeks, a job (or a local run) checks out `openorca` at its last commit, builds the C#
  Release CLI and runs `orca-crosscheck --csharp <that exe>`. When openorca is archived, the
  scenarios are frozen as golden files from the last C# outputs, or the crate is dropped.
- **Release workflow** (tag `abhoth-v*`):
  - builds `abhoth.exe` and `abhoth-desktop.exe` (with `desktop`) for
    x86_64-pc-windows-msvc, and `abhoth` (headless) for x86_64-unknown-linux-gnu;
  - builds the Windows MSI (today `abhoth-installer.yml` uploads it as a workflow
    artifact only);
  - attaches everything to a GitHub Release with SHA-256 sums.
- openorca's CI stays as it is until the repo is archived.

## 3. Release and install on Windows (user triggers the install)

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
- **Desktop:** closing the window hides it to the tray by default (agents keep running);
  Quit is in the tray menu, and `desktop.closeToTray: false` switches to quit-on-close
  (D113).
- **Later:** signing, MSIX, and auto-update, only if they're wanted.

## 4. DeepThought web container (user triggers)

**Today:**
- The TrueNAS custom app `openorca-web` runs the container `ix-openorca-web-openorca-1`.
- **Image:** `local/openorca-web:<n>`, built from a source tarball unpacked in
  `$AI_ROOT/orca/build/src<n>` with
  `docker build -f deploy/docker/Dockerfile -t local/openorca-web:<n> .`. The logs are in
  `build<n>.log`, and the last image is :25.
- **Contents:** the SDK 10 runtime, Node, and the Claude Code and Codex CLIs.
- **Entrypoint:** `deploy/docker/entrypoint.sh` runs `openorca --hostui=0.0.0.0:8093
  --hostui-login` with bench flags.
- **Environment:** `OPENORCA_HOME=/state/openorca` and `OPENORCA_PROJECTS_ROOT=/projects`.

**Plan:**
1. In the abhoth repo, add `deploy/docker/Dockerfile`:
   - **Build stage:** `rust:<pinned>-bookworm`, then
     `cargo build --release -p orca-cli` (headless; no `desktop` feature).
   - **Runtime:** the same base as today. Keep the SDK image, because the agent still builds
     .NET projects, and keep Node and the CLIs.
   - Copy `target/release/abhoth` to `/app/abhoth`. Keep `/app/openorca` as a symlink to it,
     so the `docker exec … --hostui-set-password` habits keep working.
   - The entrypoint gets `BIN=${BIN:-/app/abhoth}`.
   - Keep `OPENORCA_HOME=/state/openorca`. Abhoth reads it as the fallback for
     `ABHOTH_HOME`, so the container needs no migration and keeps its accounts, sessions and
     bench state.
2. Build `local/abhoth-web:1` next to the old image, the same way as today: a tarball of the
   abhoth repo into `src<n>`, logged to `build<n>.log`.
3. **Short trial** on the cutover day:
   - Run a second app on another port (e.g. 8094) with a COPY of the state:
     `cp -a $AI_ROOT/orca/state $AI_ROOT/orca/state-abhoth`.
   - Sign in, open a project, chat, and run one bench item.
4. **Switch:** point `openorca-web` at `local/abhoth-web:1` with the real `/state`. Port 8093
   and the npm proxy are unchanged. Keep the `:25` image for the rollback period.

## 5. Bench and tooling

- The bench kit calls the CLI by path. Point it at `abhoth` with a variable, or use the
  container symlink.
- `results.jsonl` is unchanged (cross-checked), so old and new results compare directly.

## 6. After about two weeks (user triggers)

- If nothing was rolled back:
  - archive the `openorca` GitHub repo (read-only);
  - delete the `:25` image and the old app config backups;
  - uninstall the C# CLI locally.
- In abhoth:
  - freeze or drop the cross-check (step 2);
  - drop the dual names kept only for C#: the `OPENORCA_*` fallbacks (one more release, with
    a warning), the `X-OpenOrca-*` headers (client and server together), and the bridges'
    MCP server name `openorca` (to `abhoth`).
- Optionally offer to delete `~/.openorca` once `~/.abhoth` is in use. Never delete it
  automatically.

## Rollback (within the two weeks)

- **Desktop:** run `openorca` again. Its `~/.openorca` was only copied, never changed.
  Sessions made under Abhoth stay in `~/.abhoth` and can be copied back by hand, because the
  formats are identical.
- **Container:** set the app's image back to `local/openorca-web:25` and restart it. The
  state formats are shared, so nothing needs converting.
- **Code:** the `openorca` repo is untouched, with C# on its main. If a fix is needed there,
  it is made in openorca and cross-checked as before.
