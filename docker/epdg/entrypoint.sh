#!/usr/bin/env bash
# Entrypoint for the VoWiFi ePDG tunnel container.
#
#   1. resolve the ePDG IP (env override -> public DNS)
#   2. launch SWu-IKEv2 against the modem SIM (EAP-AKA over AT+CSIM)
#   3. wait for the tunnel + P-CSCF assignment, print the netns state
#   4. keep the tunnel alive (idle SWu tunnels drop after a while)
#   5. tear down the netns on exit
#
# The dialer creates network namespace "$NETNS", opens tun1, moves it into the
# namespace and installs the split-default routes THERE, so the container's own
# routing (used to reach the ePDG) is untouched.
set -uo pipefail

MCC="${MCC:-404}"
MNC="${MNC:-043}"
APN="${APN:-ims}"
MODEM_PORT="${MODEM_PORT:-/dev/ttyUSB6}"
EPDG_FQDN="${EPDG_FQDN:-epdg.epc.mnc${MNC}.mcc${MCC}.pub.3gppnetwork.org}"
NETNS="${NETNS:-ims}"
EPDG_IP="${EPDG_IP:-}"
SRC_ADDR="${SRC_ADDR:-}"
KEEPALIVE_INTERVAL="${KEEPALIVE_INTERVAL:-20}"

log() { echo "[epdg] $*"; }

# --- Preflight -------------------------------------------------------------
[ -c /dev/net/tun ] || { log "FATAL: /dev/net/tun missing (need --device /dev/net/tun + cap NET_ADMIN)"; exit 1; }
[ -e "$MODEM_PORT" ] || { log "FATAL: modem port $MODEM_PORT not present in container (check devices:)"; exit 1; }
if ! ip netns add __probe 2>/dev/null; then
    log "FATAL: cannot create network namespaces — add cap_add: SYS_ADMIN (and NET_ADMIN)"; exit 1
fi
ip netns del __probe 2>/dev/null || true

# --- Resolve ePDG IP -------------------------------------------------------
if [ -n "$EPDG_IP" ]; then
    log "using ePDG IP from EPDG_IP override: $EPDG_IP"
else
    log "resolving $EPDG_FQDN ..."
    EPDG_IP="$(dig +short "$EPDG_FQDN" A | grep -E '^[0-9.]+$' | head -1)"
    if [ -z "$EPDG_IP" ]; then
        log "FATAL: could not resolve an A record for $EPDG_FQDN. Set EPDG_IP explicitly."
        exit 1
    fi
    log "resolved ePDG: $EPDG_IP"
fi

# --- Cleanup on exit -------------------------------------------------------
KEEPALIVE_PID=""
SWU_PID=""
IMS_AGENT_SUPERVISOR_PID=""
SIP_AGENT_SUPERVISOR_PID=""
cleanup() {
    log "shutting down ..."
    [ -n "$KEEPALIVE_PID" ] && kill "$KEEPALIVE_PID" 2>/dev/null
    [ -n "$IMS_AGENT_SUPERVISOR_PID" ] && kill "$IMS_AGENT_SUPERVISOR_PID" 2>/dev/null
    [ -n "$SIP_AGENT_SUPERVISOR_PID" ] && kill "$SIP_AGENT_SUPERVISOR_PID" 2>/dev/null
    pkill -f vowifi-ims-agent 2>/dev/null
    pkill -f vowifi-sip-agent 2>/dev/null
    [ -n "$SWU_PID" ] && kill "$SWU_PID" 2>/dev/null
    ip netns del "$NETNS" 2>/dev/null
    true
}
trap cleanup EXIT INT TERM

# --- Launch the dialer -----------------------------------------------------
SRC_OPT=()
[ -n "$SRC_ADDR" ] && SRC_OPT=(-s "$SRC_ADDR")

LOG=/tmp/swu.log
: > "$LOG"
log "starting SWu-IKEv2: modem=$MODEM_PORT apn=$APN mcc=$MCC mnc=$MNC epdg=$EPDG_IP netns=$NETNS"
# Once connected, the dialer's main loop does select() on stdin alongside the
# IKE sockets to accept interactive q/i/c/r keystrokes. If stdin is closed or
# /dev/null (EOF), select() reports it ready on every iteration and the loop
# busy-spins printing its prompt — millions of lines/sec, no delay. Feed it a
# pipe that stays open and never delivers data/EOF so select() only wakes on
# real IKE traffic. Using process substitution (not a `|` pipeline) keeps
# SWU_PID pointing at swu_emulator.py itself, not at tail/tee.
python3 -u swu_emulator.py -m "$MODEM_PORT" -a "$APN" -M "$MCC" -N "$MNC" \
        -d "$EPDG_IP" -n "$NETNS" "${SRC_OPT[@]}" < <(tail -f /dev/null) \
        > >(tee "$LOG") 2>&1 &
SWU_PID=$!

# --- Wait for tunnel readiness --------------------------------------------
log "waiting for tunnel (P-CSCF assignment) ..."
for _ in $(seq 1 90); do
    if grep -q "P-CSCF IPV[46] ADDRESS" "$LOG" 2>/dev/null; then
        break
    fi
    if ! kill -0 "$SWU_PID" 2>/dev/null; then
        log "FATAL: dialer exited before establishing the tunnel (see log above)."
        exit 1
    fi
    sleep 2
done

# Extract first P-CSCF (prefer IPv4).
PCSCF="$(grep 'P-CSCF IPV4 ADDRESS' "$LOG" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
PCSCF6=""
if [ -z "$PCSCF" ]; then
    PCSCF6="$(grep 'P-CSCF IPV6 ADDRESS' "$LOG" | grep -oE '([0-9a-fA-F]{0,4}:){2,}[0-9a-fA-F:]+' | head -1)"
fi

if [ -z "$PCSCF" ] && [ -z "$PCSCF6" ]; then
    log "WARNING: could not confirm P-CSCF assignment; leaving dialer running (inspect log)."
else
    PCSCF_ADDR="${PCSCF:-$PCSCF6}"
    log "tunnel UP. P-CSCF: $PCSCF_ADDR"
    echo "$PCSCF_ADDR" > /tmp/pcscf
    ip netns exec "$NETNS" ip addr show 2>/dev/null | sed 's/^/[epdg][netns] /'
    # Keepalive — idle SWu tunnels drop after a while. Use a TCP connect to the
    # P-CSCF's SIP port rather than ping: operators commonly filter ICMP over
    # the tunnel (confirmed on Vodafone India) while the SIP port stays open.
    (
        while true; do
            ip netns exec "$NETNS" bash -c "timeout 3 bash -c '>/dev/tcp/$PCSCF_ADDR/5060'" >/dev/null 2>&1
            sleep "$KEEPALIVE_INTERVAL"
        done
    ) &
    KEEPALIVE_PID=$!

    # --- veth pair for the inbound VoWiFi bridge agents (specs/011-vowifi-sip-bridge) ---
    # Agent A (vowifi-ims-agent) runs inside netns "$NETNS" alongside the SWu
    # tunnel/Gm-IPsec state; Agent B (vowifi-sip-agent) runs in this
    # container's default namespace (epdg-net -> LAN, reachable to the PBX).
    # Addresses default to VowifiConfig's veth_local_addr (Agent A / ims-netns
    # end) / veth_peer_addr (Agent B / default-netns end) — override both ends
    # together if the config file's [vowifi] section is customized.
    VETH_SIP="${VETH_SIP:-veth-sip}"
    VETH_IMS="${VETH_IMS:-veth-ims}"
    VETH_IMS_ADDR="${VETH_IMS_ADDR:-10.99.0.1/30}"
    VETH_SIP_ADDR="${VETH_SIP_ADDR:-10.99.0.2/30}"
    log "creating veth pair ($VETH_SIP <-> $VETH_IMS in netns $NETNS) for the VoWiFi bridge agents..."
    if ! ip link show "$VETH_SIP" >/dev/null 2>&1; then
        ip link add "$VETH_SIP" type veth peer name "$VETH_IMS" netns "$NETNS"
    else
        log "veth pair already exists, reusing"
    fi
    ip addr replace "$VETH_SIP_ADDR" dev "$VETH_SIP"
    ip link set "$VETH_SIP" up
    ip netns exec "$NETNS" ip addr replace "$VETH_IMS_ADDR" dev "$VETH_IMS"
    ip netns exec "$NETNS" ip link set "$VETH_IMS" up
    log "veth ready: $VETH_SIP=$VETH_SIP_ADDR (default netns), $VETH_IMS=$VETH_IMS_ADDR (netns $NETNS)"

    # --- launch + supervise the two VoWiFi bridge agents ---------------------
    # Replaces the old manual `docker exec ... ims-call` one-shot flow
    # (still documented in README.md as a diagnostic fallback) with an
    # always-on pair of agents, per specs/011-vowifi-sip-bridge. The binary
    # isn't baked into this image (see README.md's build-and-`docker cp`
    # instructions) — skip supervision with a clear message if it isn't
    # there yet, rather than crash-looping.
    GSM_SIP_BRIDGE_BIN="${GSM_SIP_BRIDGE_BIN:-/usr/local/bin/gsm-sip-bridge}"
    GSM_SIP_BRIDGE_CONFIG="${GSM_SIP_BRIDGE_CONFIG:-/etc/gsm-sip-bridge/config.toml}"
    if [ ! -x "$GSM_SIP_BRIDGE_BIN" ]; then
        log "NOTE: $GSM_SIP_BRIDGE_BIN not present — skipping vowifi-*-agent launch." \
            "Build+copy it in (see README.md), set GSM_SIP_BRIDGE_BIN, and restart the container to enable it."
    elif [ ! -f "$GSM_SIP_BRIDGE_CONFIG" ]; then
        log "NOTE: $GSM_SIP_BRIDGE_CONFIG not present — skipping vowifi-*-agent launch." \
            "Mount a config.toml with [vowifi] enabled = true (see config.toml.example) to enable it."
    else
        log "starting vowifi-ims-agent (netns $NETNS) and vowifi-sip-agent (default netns), supervised..."
        (
            while true; do
                ip netns exec "$NETNS" "$GSM_SIP_BRIDGE_BIN" -v --config "$GSM_SIP_BRIDGE_CONFIG" vowifi-ims-agent
                log "vowifi-ims-agent exited (status $?); restarting in 5s"
                sleep 5
            done
        ) &
        IMS_AGENT_SUPERVISOR_PID=$!
        (
            while true; do
                "$GSM_SIP_BRIDGE_BIN" -v --config "$GSM_SIP_BRIDGE_CONFIG" vowifi-sip-agent
                log "vowifi-sip-agent exited (status $?); restarting in 5s"
                sleep 5
            done
        ) &
        SIP_AGENT_SUPERVISOR_PID=$!
    fi
fi

# --- Block on the dialer ---------------------------------------------------
wait "$SWU_PID"
