#!/usr/bin/env bash
#
# Runs before every job on an abhoth runner (ACTIONS_RUNNER_HOOK_JOB_STARTED, set by
# entrypoint.sh). First it refuses jobs from forks (below). Then: each runner has its own cache directory (CARGO_HOME, CARGO_TARGET_DIR,
# XWIN_CACHE_DIR, the npm cache, TMPDIR under $RUNNER_CACHE), so pruning one never pulls
# files from under the other runner's build. The Storage pool is shared with everything
# else on the box, so the caches stay under ABHOTH_CACHE_CAP_GB per runner (two runners,
# 14 GB each, ~30 GB in all with the work folders); the dataset's quota is the backstop.
#
# Order of sacrifice: the target dir (rebuilds, minutes), then the cargo registry and npm
# cache (re-downloads, seconds). The xwin SDK cache (~1.5 GB, slow to fetch) is kept.
set -u

# The repo is public, so a pull request from a fork must never run on these runners: its
# workflow file comes from the fork and could drop any `if:` in ci.yml. This hook lives in
# the image, out of a pull request's reach. Allowed: push, workflow_dispatch, and pull requests
# whose head branch is in this repository. Everything else fails before the first step.
event="${GITHUB_EVENT_NAME:-}"
case "$event" in
  push|workflow_dispatch) ;;
  pull_request)
    head=$(jq -r '.pull_request.head.repo.full_name // empty' "${GITHUB_EVENT_PATH:-/dev/null}" 2>/dev/null)
    if [ -z "$head" ] || [ "$head" != "${GITHUB_REPOSITORY:-}" ]; then
      echo "::error::abhoth-ci: refusing a pull request from '${head:-unknown}' (not ${GITHUB_REPOSITORY:-this repo}) on a self-hosted runner"
      exit 1
    fi
    ;;
  *)
    echo "::error::abhoth-ci: refusing event '${event:-unknown}' on a self-hosted runner"
    exit 1
    ;;
esac

cache="${RUNNER_CACHE:?}"
cap="${ABHOTH_CACHE_CAP_GB:-14}"

size_gb() { du -sx --block-size=1G "$1" 2>/dev/null | cut -f1; }

now=$(size_gb "$cache"); now=${now:-0}
echo "abhoth cache: ${now} GB in $cache (cap ${cap} GB)"
if [ "$now" -gt "$cap" ]; then
  echo "abhoth cache: over the cap; removing the target dir $CARGO_TARGET_DIR"
  rm -rf "$CARGO_TARGET_DIR"
  now=$(size_gb "$cache"); now=${now:-0}
fi
if [ "$now" -gt "$cap" ]; then
  echo "abhoth cache: still over; removing the cargo registry and the npm cache"
  rm -rf "$CARGO_HOME/registry" "$CARGO_HOME/git" "$npm_config_cache"
fi
mkdir -p "$CARGO_TARGET_DIR" "$npm_config_cache" "$TMPDIR"
# TMPDIR is per runner and only one job runs at a time on a runner: start each job empty.
find "$TMPDIR" -mindepth 1 -maxdepth 1 -exec rm -rf {} + 2>/dev/null || true
exit 0
