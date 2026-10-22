#!/bin/sh
# usage: dial-until-forwarded.sh <number-with-country-code> <registrar> <bridge-container> [tries]
#   Dials from the baresip-caller (console 5556) until the bridge logs "path=forwarded"
#   for the new call (carrier offers vary per call: a mode-set restriction makes a
#   call transcoded by the rule). Leaves the forwarded call up. The Pi side uses
#   `ssh pi@... docker logs`, so pass BRIDGE_LOG_CMD to override how logs are read.
set -eu
number=$1 registrar=$2 container=$3 tries=${4:-6}
logs() { ${BRIDGE_LOG_CMD:-docker logs $container} 2>&1 | sed 's/\x1b\[[0-9;]*m//g'; }
i=0
while [ $i -lt "$tries" ]; do
  i=$((i + 1))
  before=$(logs | grep -c "media path decided" || true)
  echo "/dial sip:$number@$registrar" | nc -u -w1 127.0.0.1 5556 >/dev/null
  n=0
  while [ $n -lt 40 ]; do
    sleep 1; n=$((n + 1))
    now=$(logs | grep -c "media path decided" || true)
    [ "$now" -gt "$before" ] && break
  done
  line=$(logs | grep "media path decided" | tail -1)
  case $line in
    *path=forwarded*) echo "try $i: $line" | cut -c1-260; exit 0 ;;
    *) echo "try $i: ${line##*decided }" ; echo "/hangup" | nc -u -w1 127.0.0.1 5556 >/dev/null; sleep 6 ;;
  esac
done
echo "no forwarded call in $tries tries" >&2; exit 1
