# The CI runners on deepthought

The Linux jobs of `.github/workflows/ci.yml` run on two self-hosted runners on deepthought
(gpu-host), so they cost no GitHub Actions minutes: the Docker Compose project
**`abhoth-ci`**, runners `abhoth-dt-1` and `abhoth-dt-2`, registered to this repository with
the labels `self-hosted, Linux, X64, deepthought, abhoth`. Jobs select them with
`runs-on: [self-hosted, linux, abhoth]`. Modelled on the `fissure-ci` and `ezmuze-ci` apps on
the same box.

| File | What |
|---|---|
| `Dockerfile` | The image `abhoth-ci-runner`: Ubuntu 24.04, the runner, Rust (rust-toolchain.toml + the `x86_64-pc-windows-msvc` target), cargo-xwin with clang/lld/llvm, the WebKitGTK/GTK/D-Bus/Wayland dev packages, Node 22, .NET 10 + `vpk` (Velopack), zstd, squashfs-tools, gh |
| `entrypoint.sh` | Unpacks and registers the runner on first start, then runs it |
| `job-started.sh` | Runs before every job (`ACTIONS_RUNNER_HOOK_JOB_STARTED`): keeps the runner's caches under 14 GB, empties its TMPDIR |
| `compose.yaml` | The two runners: 16 threads each on NUMA node 1 (`cpuset`), 24 GB of memory each |

The jobs build inside the runner containers, so the image carries everything they need.
The Windows SDK and MSVC CRT that cargo-xwin links against are not baked in: cargo-xwin
downloads them on first use into `XWIN_CACHE_DIR` (~1.5 GB per runner, kept).

## Where things live on deepthought

All of it is in the dataset **`Storage/abhoth-ci`** (quota 40 GB), not on the Apps pool,
which is fragile (its NVMe failed on 2026-11-21). Only the image itself is in Docker's
storage on Apps.

- `$ABHOTH_CI_ROOT/image/` — a copy of this directory; the build context
- `$ABHOTH_CI_ROOT/runners/<n>/` — each runner, its registration and `_work`
- `$ABHOTH_CI_ROOT/cache/<n>/` — each runner's `CARGO_HOME`, `CARGO_TARGET_DIR`,
  `XWIN_CACHE_DIR`, npm cache and `TMPDIR`. Per runner, so pruning one never pulls files from
  under the other's build. The target dir lives here rather than in the workspace, so builds
  are warm while `actions/checkout` still gives every job a clean tree.
- `$ABHOTH_CI_ROOT/builds/` — the last five `package` outputs (Setup.exe, AppImage,
  the zipped exes, SHA256SUMS) from every run. GitHub artifacts are uploaded only on
  workflow_dispatch and tags (~110 MB each; the account's artifact storage quota is small).

The caches are capped at 14 GB per runner (~30 GB in all with the work folders):
`job-started.sh` removes the target dir first, then the cargo registry and npm cache, when a
runner's cache directory is over the cap. The dataset quota (40 GB) is the backstop.

Access: `ssh -i ~/.ssh/id_truenas admin@gpu-host`; `sudo -n docker ...` works.

## Why Docker Compose and not a TrueNAS custom app

The fissure-ci and ezmuze-ci runners are TrueNAS custom apps. Since the Apps pool incident
(2026-11-21) the middleware reports Docker as FAILED (`midclt call docker.status`) although
Docker itself runs, so `app.create` is not trusted to work, and a failure there could restart
Docker under every other app. abhoth-ci is a plain Compose project instead, like the router
shadow; `restart: unless-stopped` brings it back after a reboot. When the middleware is
healthy again it can become a custom app: `midclt call -j app.create` with
`{"app_name": "abhoth-ci", "custom_app": true, "custom_compose_config_string": "<compose.yaml>"}`
(stop the Compose project first; the registrations in `runners/<n>/` survive).

## Changing the image

```sh
scp deploy/ci-runner/* admin@gpu-host:$ABHOTH_CI_ROOT/image/
ssh -i ~/.ssh/id_truenas admin@gpu-host \
  'cd $ABHOTH_CI_ROOT/image && sudo -n docker compose -p abhoth-ci up -d --build'
```

Recreating a container keeps its registration (it is in `runners/<n>/`). A job running at the
time fails; re-run it.

## Adding or re-registering a runner

The first start of a runner waits for a registration token at
`$ABHOTH_CI_ROOT/reg-token`. Tokens last an hour; never print one:

```sh
gh api -X POST repos/madninjaskillz/abhoth/actions/runners/registration-token --jq .token \
  | ssh -i ~/.ssh/id_truenas admin@gpu-host 'umask 077; cat > $ABHOTH_CI_ROOT/reg-token'
```

Delete the file once the runners show as online
(`gh api repos/madninjaskillz/abhoth/actions/runners`). To re-register one, delete its
`runners/<n>/.runner` and recreate the container.

## Stopping it, and moving the jobs back to GitHub-hosted runners

- Stop: `sudo -n docker compose -p abhoth-ci -f $ABHOTH_CI_ROOT/image/compose.yaml stop`.
  Jobs then queue (they do not fail) until a runner is back.
- Back to hosted: in `ci.yml`, change each `runs-on: [self-hosted, linux, abhoth]` to
  `ubuntu-latest`, and give `rust-linux` back the apt step for the desktop feature
  (`sudo apt-get install -y --no-install-recommends pkg-config libwebkit2gtk-4.1-dev
  libgtk-3-dev libdbus-1-dev libwayland-dev`) and `client` back `actions/setup-node` (Node 22).
  The `package` job also needs cargo-xwin, clang/lld/llvm, .NET + vpk, zstd and
  squashfs-tools there; simplest is to drop it to `workflow_dispatch` on hosted runners.
- Remove for good: stop it, then remove the runners on GitHub (Settings → Actions → Runners)
  and `sudo -n docker compose -p abhoth-ci down`. Leave the image unless told otherwise.
