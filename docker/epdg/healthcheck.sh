#!/usr/bin/env bash
# Healthy when the tunnel interface has an IP AND the P-CSCF (SIP registrar)
# is reachable over it. ICMP is commonly filtered by the operator, so this
# uses a TCP connect to the SIP port rather than ping.
set -uo pipefail
NETNS="${NETNS:-ims}"

ip netns exec "$NETNS" ip addr show tun1 2>/dev/null | grep -qE 'inet6? ' || exit 1

if [ -s /tmp/pcscf ]; then
    PCSCF_ADDR="$(cat /tmp/pcscf)"
    ip netns exec "$NETNS" bash -c "timeout 3 bash -c '>/dev/tcp/$PCSCF_ADDR/5060'" 2>/dev/null || exit 1
fi
