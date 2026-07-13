#!/usr/bin/env bash
# Healthy when the circuit-switched daemon's metrics endpoint responds, and
# — only if [vowifi].enabled — the VoWiFi/ePDG tunnel interface has an
# address and the P-CSCF (SIP registrar) is reachable over it. ICMP is
# commonly filtered by the operator, so this uses a TCP connect to the SIP
# port rather than ping.
set -uo pipefail

GSM_SIP_BRIDGE_BIN="${GSM_SIP_BRIDGE_BIN:-/usr/local/bin/gsm-sip-bridge}"
GSM_SIP_BRIDGE_CONFIG="${GSM_SIP_BRIDGE_CONFIG:-/etc/gsm-sip-bridge/config.toml}"

# All non-secret configuration lives in config.toml's [vowifi] section (plus
# [metrics].port) — ask the binary for the resolved values instead of
# hand-parsing TOML or reading raw env vars (specs/012-strongswan-epdg
# config consolidation; see docker/entrypoint.sh for the same pattern).
eval "$("$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" config vowifi-shell-env)" || exit 1

# Tunnel interface name depends on TUNNEL_ENGINE (specs/012-strongswan-epdg):
# "tun1" for the swu engine (named by the SWu-IKEv2 dialer itself), the
# strongswan engine's own XFRM interface (STRONGSWAN_TUN_IFACE) otherwise.
# Hardcoding "tun1" here made every strongswan-engine container report
# unhealthy regardless of real tunnel state (found by live-testing).
if [ "$TUNNEL_ENGINE" = "strongswan" ]; then
    TUN_IFACE="$STRONGSWAN_TUN_IFACE"
else
    TUN_IFACE="tun1"
fi

wget -qO- "http://localhost:${METRICS_PORT}/metrics" >/dev/null || exit 1

if ! "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" config vowifi-enabled; then
    exit 0
fi

ip netns exec "$NETNS" ip addr show "$TUN_IFACE" 2>/dev/null | grep -qE 'inet6? ' || exit 1

if [ -s /tmp/pcscf ]; then
    PCSCF_ADDR="$(cat /tmp/pcscf)"
    ip netns exec "$NETNS" bash -c "timeout 3 bash -c '>/dev/tcp/$PCSCF_ADDR/5060'" 2>/dev/null || exit 1
fi
