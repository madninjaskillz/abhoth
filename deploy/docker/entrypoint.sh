#!/bin/bash
# Starts the Abhoth web UI in sign-in mode. Secrets come from read-only files under /run/secrets:
#   strata-api-key   the model server key (exported as the env var config.json's slots.apiKeyEnv names)
#   git-deploy-key   optional: an SSH private key for git (a per-repo deploy key)
#   git-credentials  optional: a git credential-store file (https://user:token@host)
#   admin-password   optional: with ADMIN_USER set, creates that admin on the first start
#                    (ABHOTH_ADMIN_USER / ABHOTH_ADMIN_PASSWORD_FILE; ignored once accounts exist)
# The single image (deploy/abhoth, D148) also runs Sothoth (the router) and Nodens (the
# supervisor) in the same process when these are set (docs/NAMES.md; the old names in brackets
# still work, the new one wins when both are set):
#   SOTHOTH_ARGS       (ROUTER_ARGS) Sothoth's flags (what follows `abhoth sothoth`), e.g.
#                      "--config /data/router/sothoth.json --port 8010 --control /data/router/sothoth.toml"
#                      (a missing sothoth.* reads the same folder's router.*, and back)
#   NODENS_CONFIG      (SUPERVISOR_CONFIG) Nodens' config file (its [supervisor] listen serves
#                      the feed); nodens.toml or supervisor.toml
#   ABHOTH_NODENS_URL  (ABHOTH_SUPERVISOR_URL) the feed the Glance page follows; defaults to
#                      http://127.0.0.1:9890 when a Nodens config is set (GLANCE=0 turns the page off)
# STATE_DIR (default /state) holds bench/ for BENCH=1.
# Nothing here prints a secret.
set -euo pipefail
umask 077
BIN=${BIN:-/app/abhoth}
mkdir -p "$HOME/.ssh" "${ABHOTH_HOME:-${OPENORCA_HOME:-$HOME/.abhoth}}" "${OPENORCA_WORKDIR:=/projects/workspace}"

# Claude Code / Codex homes (persistent, in $HOME on the state dataset): the progress-bar rule
# and tool, written once (a file the user changed is left alone).
for f in .claude/CLAUDE.md .codex/AGENTS.md .claude/progress.js .bashrc; do
  if [[ -r "/app/home-seed/$f" && ! -e "$HOME/$f" ]]; then
    mkdir -p "$HOME/$(dirname "$f")" && cp "/app/home-seed/$f" "$HOME/$f"
  fi
done

if [[ -r /run/secrets/strata-api-key ]]; then
  export "${STRATA_KEY_ENV:-MAYA_API_KEY}=$(tr -d '\r\n' < /run/secrets/strata-api-key)"
fi
if [[ -r /run/secrets/git-deploy-key ]]; then
  export GIT_SSH_COMMAND="ssh -i /run/secrets/git-deploy-key -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=$HOME/.ssh/known_hosts"
fi
if [[ -r /run/secrets/git-credentials ]]; then
  git config --global credential.helper "store --file=/run/secrets/git-credentials"
fi
# Projects on the share belong to other uids: git refuses those ("dubious ownership") unless
# trusted. The container is single-user and only mounts projects, so trust them all (once).
git config --global --get-all safe.directory 2>/dev/null | grep -qx '\*' || git config --global --add safe.directory '*'
git config --global --get user.name >/dev/null || git config --global user.name "${GIT_USER_NAME:-Abhoth}"
git config --global --get user.email >/dev/null || git config --global user.email "${GIT_USER_EMAIL:-abhoth@localhost}"

if [[ -n "${ADMIN_USER:-}" && -r /run/secrets/admin-password ]]; then
  export ABHOTH_ADMIN_USER="$ADMIN_USER" ABHOTH_ADMIN_PASSWORD_FILE=/run/secrets/admin-password
fi

# Account commands pass straight through (D140):
#   docker exec -it <c> /app/entrypoint.sh admin create|reset-password|reset-2fa <user> | admin list
# The old --hostui-set-password <user> / --hostui-reset-2fa [<user>] still work.
if [[ "${1:-}" == admin || "${1:-}" == --hostui-set-password || "${1:-}" == --hostui-reset-2fa ]]; then
  exec "$BIN" "$@"
fi

cd "$OPENORCA_WORKDIR"
args=(--hostui="${BIND:-0.0.0.0}:${PORT}" --hostui-login)
[[ -n "${TRUSTED_PROXIES:-}" ]] && args+=(--hostui-trust-proxy="$TRUSTED_PROXIES")
[[ "${REQUIRE_2FA:-0}" == 1 ]] && args+=(--hostui-require-2fa)
STATE_DIR=${STATE_DIR:-/state}
if [[ "${BENCH:-0}" == 1 ]]; then
  args+=(--hostui-bench="$STATE_DIR/bench" --hostui-bench-kit="$STATE_DIR/bench/kit")
  [[ -r /run/secrets/ha.env ]] && args+=(--hostui-bench-power=/run/secrets/ha.env)
  [[ -d "$STATE_DIR/bench/kit/openorca.git" ]] && export REALTASKS_REPO="$STATE_DIR/bench/kit/openorca.git"
fi
ROUTER_ARGS=${SOTHOTH_ARGS:-${ROUTER_ARGS:-}}
SUPERVISOR_CONFIG=${NODENS_CONFIG:-${SUPERVISOR_CONFIG:-}}
[[ -n "${ROUTER_ARGS:-}" ]] && args+=(--with-router "$ROUTER_ARGS")
if [[ -n "${SUPERVISOR_CONFIG:-}" ]]; then
  args+=(--with-supervisor "$SUPERVISOR_CONFIG")
  if [[ "${GLANCE:-1}" == 1 && -z "${ABHOTH_NODENS_URL:-}" ]]; then
    export ABHOTH_SUPERVISOR_URL="${ABHOTH_SUPERVISOR_URL:-http://127.0.0.1:9890}"
  fi
fi
[[ "${GLANCE:-1}" == 0 ]] && unset ABHOTH_SUPERVISOR_URL ABHOTH_NODENS_URL
# Sothoth and Nodens logs on stderr (docker logs), like their own images.
if [[ -n "${ROUTER_ARGS:-}${SUPERVISOR_CONFIG:-}" ]]; then
  export RUST_LOG="${RUST_LOG:-info}"
fi
exec "$BIN" "${args[@]}" "$@"
