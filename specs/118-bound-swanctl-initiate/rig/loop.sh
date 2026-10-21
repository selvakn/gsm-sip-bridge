export STRONGSWAN_CONF=/rig/strongswan.conf
PID=$(pgrep -f libexec/ipsec/charon | head -1)
EXTRA="$1"; N=${2:-22}
for i in $(seq 1 $N); do
  swanctl --initiate --child ims0 $EXTRA >/dev/null 2>&1 &
  sleep 5
  CL=$(pgrep -fc "swanctl --initiate")
  T=$(ls /proc/$PID/task | wc -l)
  timeout 4 swanctl --list-sas --ike ims0 >/dev/null 2>&1; RC=$?
  echo "tick=$i clients=$CL charon_threads=$T list_sas_rc=$RC log_lines=$(wc -l </tmp/charon.log)"
done
