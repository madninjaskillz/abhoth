#!/bin/bash
# Starts the Abhoth web UI in sign-in mode. Secrets come from read-only files under /run/secrets:
#   strata-api-key   the model server key (exported as the env var config.json's slots.apiKeyEnv names)
#   git-deploy-key   optional: an SSH private key for git (a per-repo deploy key)
#   git-credentials  optional: a git credential-store file (https://user:token@host)
#   admin-password   optional: with ADMIN_USER set, creates that admin on the first start
#                    (ABHOTH_ADMIN_USER / ABHOTH_ADMIN_PASSWORD_FILE; ignored once accounts exist)
# Nothing here prints a secret.
set -euo pipefail
umask 077
BIN=${BIN:-/app/abhoth}
mkdir -p "$HOME/.ssh" "$OPENORCA_HOME" "${OPENORCA_WORKDIR:=/projects/workspace}"

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
if [[ "${BENCH:-0}" == 1 ]]; then
  args+=(--hostui-bench=/state/bench --hostui-bench-kit=/state/bench/kit)
  [[ -r /run/secrets/ha.env ]] && args+=(--hostui-bench-power=/run/secrets/ha.env)
  [[ -d /state/bench/kit/openorca.git ]] && export REALTASKS_REPO=/state/bench/kit/openorca.git
fi
exec "$BIN" "${args[@]}" "$@"
