#!/bin/sh
# POSTINIT command: (re)creates the transient abhoth-hostd unit. The binary sits next to this
# script; APPS is two levels up ($APPS/abhoth/bin/start-hostd.sh).
H=$(cd "$(dirname "$0")/.." && pwd)
APPS=$(dirname "$H")
systemctl stop abhoth-hostd 2>/dev/null
systemctl reset-failed abhoth-hostd 2>/dev/null
install -d -m 0750 /run/abhoth
if getent group abhoth-exec >/dev/null 2>&1; then chgrp abhoth-exec /run/abhoth && chmod g+s /run/abhoth; fi
exec systemd-run --unit=abhoth-hostd --description="abhoth hostd (GPU clocks, fans, telemetry)" \
  -p Type=notify -p NotifyAccess=main -p WatchdogSec=10 \
  -p Restart=always -p RestartSec=2 -p TimeoutStopSec=30 -p KillMode=mixed \
  "$H/bin/abhoth-hostd" run --apps "$APPS" --clocks "${HOSTD_CLOCKS:-on}"
