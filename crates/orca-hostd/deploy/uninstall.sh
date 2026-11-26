#!/bin/sh
# Removes abhoth hostd: stops the unit (cards back to serving, fan backends released), removes
# the POSTINIT entry and the failsafe cron job. Leaves $APPS/abhoth on disk.
#   sudo sh uninstall.sh /mnt/Apps
set -u
APPS=${1:?usage: uninstall.sh APPS_DIR}
H=$APPS/abhoth
systemctl stop abhoth-hostd 2>/dev/null
systemctl reset-failed abhoth-hostd 2>/dev/null
for id in $(midclt call initshutdownscript.query '[["command","=","'"$H"'/bin/start-hostd.sh"]]' | python3 -c 'import json,sys;print(" ".join(str(x["id"]) for x in json.load(sys.stdin)))'); do
  midclt call initshutdownscript.delete "$id" >/dev/null
done
for id in $(midclt call cronjob.query '[["command","^","'"$H"'/bin/abhoth-hostd failsafe"]]' | python3 -c 'import json,sys;print(" ".join(str(x["id"]) for x in json.load(sys.stdin)))'); do
  midclt call cronjob.delete "$id" >/dev/null
done
echo "abhoth hostd removed (files under $H kept)"
