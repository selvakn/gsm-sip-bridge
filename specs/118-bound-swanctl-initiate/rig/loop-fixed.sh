# The fixed supervisor's behaviour, on the rig: the exact initiate_argv shape
# (timeout -k 2 25 ... --timeout 20) and at most one live client per line
# (a new one is skipped while the previous is alive), at the old 5s cadence.
export STRONGSWAN_CONF=/rig/strongswan.conf
PID=$(pgrep -f libexec/ipsec/charon | head -1)
N=${1:-22}; CLIENT=""
for i in $(seq 1 $N); do
  if [ -n "$CLIENT" ] && kill -0 "$CLIENT" 2>/dev/null; then
    echo "tick=$i initiate skipped, previous client still running"
  else
    timeout -k 2 25 env STRONGSWAN_CONF=$STRONGSWAN_CONF swanctl --initiate --child ims0 --timeout 20 >/dev/null 2>&1 &
    CLIENT=$!
  fi
  sleep 5
  CL=$(pgrep -fc "swanctl --initiate")
  T=$(ls /proc/$PID/task | wc -l)
  timeout 4 swanctl --list-sas --ike ims0 >/dev/null 2>&1; RC=$?
  echo "tick=$i clients=$CL charon_threads=$T list_sas_rc=$RC log_lines=$(wc -l </tmp/charon.log)"
done
