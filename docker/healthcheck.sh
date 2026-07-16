#!/usr/bin/env bash
# Healthy when the circuit-switched daemon's metrics endpoint responds, and
# — only if [vowifi].enabled and at least one VoWiFi line was resolved —
# *every* line's tunnel interface has an address and its P-CSCF (SIP
# registrar) is reachable over it (specs/013-multi-card-vowifi FR-019: the
# health check must consider every line, not only the first). ICMP is
# commonly filtered by the operator, so this uses a TCP connect to the SIP
# port rather than ping.
#
# A line failing its individual checks is logged and makes the overall
# result unhealthy (so `docker ps`/monitoring surfaces a real per-line
# fault), but this never re-runs USB/AT discovery itself: `discover
# --from-file` only re-reads the line-resolution file `docker/entrypoint.sh`
# already produced once at startup — re-scanning every 30s on this
# `HEALTHCHECK` interval would race the already-running VoWiFi agents
# holding those same serial ports open.
set -uo pipefail

GSM_SIP_BRIDGE_BIN="${GSM_SIP_BRIDGE_BIN:-/usr/local/bin/gsm-sip-bridge}"
GSM_SIP_BRIDGE_CONFIG="${GSM_SIP_BRIDGE_CONFIG:-/etc/gsm-sip-bridge/config.toml}"

# All non-secret configuration lives in config.toml's [vowifi] section (plus
# [metrics].port) — ask the binary for the resolved values instead of
# hand-parsing TOML or reading raw env vars (specs/012-strongswan-epdg
# config consolidation; see docker/entrypoint.sh for the same pattern).
eval "$("$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" config vowifi-shell-env)" || exit 1

wget -qO- "http://localhost:${METRICS_PORT}/metrics" >/dev/null || exit 1

if ! "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" config vowifi-enabled; then
    exit 0
fi

eval "$("$GSM_SIP_BRIDGE_BIN" discover --shell-env --from-file)" || exit 1

if [ "$LINE_COUNT" -eq 0 ]; then
    # The spec's own degrade clarification: zero usable lines is a reported
    # condition, not a container-failing one — the circuit-switched side
    # (already checked above) is what still has to be healthy.
    exit 0
fi

STATUS=0
for i in $(seq 0 $((LINE_COUNT - 1))); do
    netns="${LINE_NETNS[i]}"
    tun_iface="${LINE_STRONGSWAN_TUN_IFACE[i]}"
    # "tun1" for the swu engine (named by the SWu-IKEv2 dialer itself), the
    # strongswan engine's own per-line XFRM interface otherwise. Hardcoding
    # "tun1" unconditionally made every strongswan-engine container report
    # unhealthy regardless of real tunnel state (found by live-testing,
    # specs/012-strongswan-epdg).
    if [ "$TUNNEL_ENGINE" != "strongswan" ]; then
        tun_iface="tun1"
    fi
    pcscf_path="${LINE_PCSCF_SOURCE_PATH[i]}"

    if ! ip netns exec "$netns" ip addr show "$tun_iface" 2>/dev/null | grep -qE 'inet6? '; then
        echo "line $i: tunnel interface $tun_iface has no address"
        STATUS=1
        continue
    fi
    if [ -s "$pcscf_path" ]; then
        pcscf_addr="$(cat "$pcscf_path")"
        if ! ip netns exec "$netns" bash -c "timeout 3 bash -c '>/dev/tcp/$pcscf_addr/5060'" 2>/dev/null; then
            echo "line $i: P-CSCF $pcscf_addr unreachable"
            STATUS=1
        fi
    fi
done

exit "$STATUS"
