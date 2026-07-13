#!/usr/bin/env bash
# Healthy when the circuit-switched daemon's metrics endpoint responds, and
# — only if [vowifi].enabled — the VoWiFi/ePDG tunnel interface has an
# address and the P-CSCF (SIP registrar) is reachable over it. ICMP is
# commonly filtered by the operator, so this uses a TCP connect to the SIP
# port rather than ping.
set -uo pipefail

GSM_SIP_BRIDGE_BIN="${GSM_SIP_BRIDGE_BIN:-/usr/local/bin/gsm-sip-bridge}"
GSM_SIP_BRIDGE_CONFIG="${GSM_SIP_BRIDGE_CONFIG:-/etc/gsm-sip-bridge/config.toml}"
NETNS="${NETNS:-ims}"
METRICS_PORT="${METRICS_PORT:-9091}"

wget -qO- "http://localhost:${METRICS_PORT}/metrics" >/dev/null || exit 1

if ! "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" config vowifi-enabled; then
    exit 0
fi

ip netns exec "$NETNS" ip addr show tun1 2>/dev/null | grep -qE 'inet6? ' || exit 1

if [ -s /tmp/pcscf ]; then
    PCSCF_ADDR="$(cat /tmp/pcscf)"
    ip netns exec "$NETNS" bash -c "timeout 3 bash -c '>/dev/tcp/$PCSCF_ADDR/5060'" 2>/dev/null || exit 1
fi
