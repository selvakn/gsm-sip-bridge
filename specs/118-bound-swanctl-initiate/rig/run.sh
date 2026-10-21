set -e
mkdir -p /etc/swanctl /var/run
cp /rig/swanctl.conf /etc/swanctl/swanctl.conf
export STRONGSWAN_CONF=/rig/strongswan.conf
/usr/libexec/ipsec/charon >/tmp/charon.out 2>&1 &
sleep 3
swanctl --load-all >/dev/null
echo "threads at start: $(ls /proc/$(pgrep -x charon | head -1)/task | wc -l)"
