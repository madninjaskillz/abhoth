#!/bin/sh
# The single image's health check (D148): every part that is on must answer.
#   web UI      http://127.0.0.1:$PORT/api/auth/state
#   Sothoth     $SOTHOTH_HEALTH_URL or $ROUTER_HEALTH_URL (default
#               http://127.0.0.1:8010/health/liveliness; no upstream calls)
#   Nodens      $ABHOTH_NODENS_URL or $ABHOTH_SUPERVISOR_URL, + /api/v1/supervisor/snapshot
#               (default http://127.0.0.1:9890)
# The new names (docs/NAMES.md) win; the old ones still work. Prints which part failed; never a
# secret.
ok=0
check() {
  if ! curl -fsS -o /dev/null --max-time 4 "$2"; then
    echo "unhealthy: $1 ($2)"
    ok=1
  fi
}
check web "http://127.0.0.1:${PORT:-8093}/api/auth/state"
if [ -n "${SOTHOTH_ARGS:-}${ROUTER_ARGS:-}" ]; then
  check sothoth "${SOTHOTH_HEALTH_URL:-${ROUTER_HEALTH_URL:-http://127.0.0.1:8010/health/liveliness}}"
fi
if [ -n "${NODENS_CONFIG:-}${SUPERVISOR_CONFIG:-}" ]; then
  check nodens "${ABHOTH_NODENS_URL:-${ABHOTH_SUPERVISOR_URL:-http://127.0.0.1:9890}}/api/v1/supervisor/snapshot"
fi
exit $ok
