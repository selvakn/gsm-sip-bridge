for p in $(pgrep -f libexec/ipsec/charon); do kill -9 $p; done
sleep 1; rm -f /var/run/charon.* /tmp/charon.log
sh /rig/run.sh 2>&1 | grep -v "opening directory"
