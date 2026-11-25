#!/usr/bin/env bash
#
# One abhoth runner on deepthought (deploy/ci-runner/README.md). Modelled on fissure-ci's.
#
# RUNNER_DIR is on the Storage pool, so the unpacked runner, its registration and its work
# folder survive the container being recreated. The first start registers it with the
# abhoth repository using a one-hour registration token left at $CI_ROOT/reg-token
# (deleted once every runner has registered); every later start just runs it.
set -euo pipefail
: "${RUNNER_NAME:?}" "${RUNNER_DIR:?}" "${RUNNER_LABELS:?}" "${RUNNER_URL:?}" "${CI_ROOT:?}"
: "${CARGO_HOME:?}" "${CARGO_TARGET_DIR:?}" "${XWIN_CACHE_DIR:?}" "${npm_config_cache:?}" "${TMPDIR:?}"

mkdir -p "$RUNNER_DIR" "$CARGO_HOME" "$CARGO_TARGET_DIR" "$XWIN_CACHE_DIR" "$npm_config_cache" "$TMPDIR"
cd "$RUNNER_DIR"
[ -x ./config.sh ] || tar -xzf /opt/runner-dist/runner.tar.gz
export RUNNER_ALLOW_RUNASROOT=1

# Before every job: keep this runner's caches under their cap (job-started.sh).
export ACTIONS_RUNNER_HOOK_JOB_STARTED=/opt/abhoth-ci/job-started.sh

if [ ! -f .runner ]; then
  tok="$CI_ROOT/reg-token"
  until [ -s "$tok" ]; do
    echo "$(date -u '+%F %T') $RUNNER_NAME is not registered; waiting for $tok"
    sleep 60
  done
  ./config.sh --unattended --replace \
    --url "$RUNNER_URL" \
    --token "$(cat "$tok")" \
    --name "$RUNNER_NAME" \
    --labels "$RUNNER_LABELS" \
    --work "$RUNNER_DIR/_work"
fi

exec ./run.sh
