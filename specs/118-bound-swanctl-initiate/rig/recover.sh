# Run after loop.sh "--timeout 20" (do NOT run restart.sh in between): shows the
# daemon is still serving and still negotiating, so an ePDG that comes back
# would be answered without restarting charon (US2).
export STRONGSWAN_CONF=/rig/strongswan.conf
PID=$(pgrep -f libexec/ipsec/charon | head -1)
[ -n "$PID" ] || { echo "recover: charon is not running"; exit 1; }
B=$(grep -c "generating IKE_SA_INIT request" /tmp/charon.log)
R=$(grep -c "retransmit" /tmp/charon.log)
sleep ${1:-90}
A=$(grep -c "generating IKE_SA_INIT request" /tmp/charon.log)
R2=$(grep -c "retransmit" /tmp/charon.log)
timeout 4 swanctl --list-sas --ike ims0 >/dev/null 2>&1; LIST=$?
timeout 25 swanctl --initiate --child ims0 --timeout 5 >/dev/null 2>&1; INIT=$?
timeout 4 swanctl --list-sas --ike ims0 >/dev/null 2>&1; LIST2=$?
echo "charon_pid=$PID init_requests_before=$B after=$A retransmits_before=$R after=$R2 list_sas_rc=$LIST fresh_initiate_rc=$INIT list_sas_after_rc=$LIST2"
[ "$LIST" = 0 ] && [ "$LIST2" = 0 ] && [ "$R2" -gt "$R" -o "$A" -gt "$B" ] && echo "RECOVERY-READY" || echo "NOT-READY"
