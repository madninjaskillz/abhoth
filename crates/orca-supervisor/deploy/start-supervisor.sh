#!/bin/sh
# POSTINIT command: (re)creates the transient abhoth-supervisor unit (TrueNAS: /etc does not
# persist). Binary and config next to hostd's: $APPS/abhoth/bin, $APPS/abhoth/etc.
H=$(cd "$(dirname "$0")/.." && pwd)
systemctl stop abhoth-supervisor 2>/dev/null
systemctl reset-failed abhoth-supervisor 2>/dev/null
"$H/bin/abhoth-supervisor" check --config "$H/etc/supervisor.toml" || exit 1
exec systemd-run --unit=abhoth-supervisor --description="abhoth supervisor (telemetry hub, Home Assistant)" \
  -p Restart=always -p RestartSec=5 -p TimeoutStopSec=15 \
  "$H/bin/abhoth-supervisor" run --config "$H/etc/supervisor.toml"
