#!/bin/sh
# The single image's health check (D148): every part that is on must answer.
#   web UI      http://127.0.0.1:$PORT/api/auth/state
#   router      $ROUTER_HEALTH_URL (default http://127.0.0.1:8010/health/liveliness; no upstream calls)
#   supervisor  $ABHOTH_SUPERVISOR_URL/api/v1/supervisor/snapshot (default http://127.0.0.1:9890)
# Prints which part failed; never a secret.
ok=0
check() {
  if ! curl -fsS -o /dev/null --max-time 4 "$2"; then
    echo "unhealthy: $1 ($2)"
    ok=1
  fi
}
check web "http://127.0.0.1:${PORT:-8093}/api/auth/state"
if [ -n "${ROUTER_ARGS:-}" ]; then
  check router "${ROUTER_HEALTH_URL:-http://127.0.0.1:8010/health/liveliness}"
fi
if [ -n "${SUPERVISOR_CONFIG:-}" ]; then
  check supervisor "${ABHOTH_SUPERVISOR_URL:-http://127.0.0.1:9890}/api/v1/supervisor/snapshot"
fi
exit $ok
