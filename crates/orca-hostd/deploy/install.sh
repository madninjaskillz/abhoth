#!/bin/sh
# Installs abhoth hostd on a TrueNAS host from the apps pool (TrueNAS rewrites /etc, so nothing
# lives there; the unit is a transient systemd unit recreated by a POSTINIT command each boot).
#
#   sudo sh install.sh /mnt/Apps            # $APPS: the apps pool
#
# Layout under $APPS/abhoth:
#   bin/abhoth-hostd                 the binary (copy it there first)
#   bin/start-hostd.sh               POSTINIT command: (re)starts the unit
#   etc/hostd-limits.toml            root-owned limits file (0644 root, dir 0755 root)
#   etc/secrets/<name>               secret:<name> files (0600 root), e.g. esp_fans_password
#   state/hostd/policy.json          last-known-good policy (written by hostd / policy-put)
#
# Steps (each is idempotent):
#   1. check the binary and the limits file (abhoth-hostd limits-check);
#   2. register the POSTINIT command (start-hostd.sh) and the failsafe cron (every minute);
#   3. start the unit now.
# Policy before the supervisor runs it: `abhoth-hostd policy-put --apps $APPS --policy FILE`.
# Uninstall: uninstall.sh (stops the unit, which returns every card to serving and every fan
# backend to the hardware, and removes the POSTINIT entry and the cron job).
set -eu
APPS=${1:?usage: install.sh APPS_DIR}
H=$APPS/abhoth
BIN=$H/bin/abhoth-hostd
[ "$(id -u)" = 0 ] || { echo "run as root"; exit 1; }
[ -x "$BIN" ] || { echo "$BIN missing"; exit 1; }
install -d -o root -g root -m 0755 "$H/etc" "$H/bin" "$H/state"
install -d -o root -g root -m 0700 "$H/etc/secrets" "$H/state/hostd"
chown root:root "$H/etc/hostd-limits.toml" && chmod 0644 "$H/etc/hostd-limits.toml"
"$BIN" limits-check --apps "$APPS"
install -o root -g root -m 0755 "$(dirname "$0")/start-hostd.sh" "$H/bin/start-hostd.sh"

q() { midclt call "$@"; }
if ! q initshutdownscript.query '[["command","=","'"$H"'/bin/start-hostd.sh"]]' | grep -q '"id"'; then
  q initshutdownscript.create '{"type":"COMMAND","command":"'"$H"'/bin/start-hostd.sh","when":"POSTINIT","enabled":true,"timeout":30,"comment":"abhoth hostd"}' >/dev/null
fi
FS="$BIN failsafe --apps $APPS"
if ! q cronjob.query '[["command","=","'"$FS"'"]]' | grep -q '"id"'; then
  q cronjob.create '{"user":"root","command":"'"$FS"'","description":"abhoth hostd failsafe","enabled":true,"stdout":false,"stderr":false,"schedule":{"minute":"*","hour":"*","dom":"*","month":"*","dow":"*"}}' >/dev/null
fi
"$H/bin/start-hostd.sh"
sleep 3
systemctl is-active abhoth-hostd
