#!/usr/bin/env bash
# Entrypoint for the unified gsm-sip-bridge image. Supervises up to two
# independent subsystems, deciding at startup which apply:
#
#   1. The circuit-switched GSM-to-SIP daemon — always started; it already
#      no-ops gracefully ("no EC20 modules found — waiting for retry") when
#      no supported modem is attached, so there's nothing to gate this on.
#   2. The inbound VoWiFi-to-SIP bridge (specs/011-vowifi-sip-bridge) — the
#      SWu-IKEv2 ePDG tunnel, a veth pair, and vowifi-ims-agent/
#      vowifi-sip-agent — started only if [vowifi].enabled = true in the
#      mounted config.toml (checked via `gsm-sip-bridge config
#      vowifi-enabled` rather than hand-parsing TOML in bash).
#
# The VoWiFi subsystem's tunnel setup creates network namespace "$NETNS",
# opens tun1, moves it into the namespace, and installs the split-default
# routes THERE, so the container's own routing (used to reach the SIP
# server / ePDG) is untouched.
set -uo pipefail

GSM_SIP_BRIDGE_BIN="${GSM_SIP_BRIDGE_BIN:-/usr/local/bin/gsm-sip-bridge}"
GSM_SIP_BRIDGE_CONFIG="${GSM_SIP_BRIDGE_CONFIG:-/etc/gsm-sip-bridge/config.toml}"

MCC="${MCC:-404}"
MNC="${MNC:-043}"
APN="${APN:-ims}"
MODEM_PORT="${MODEM_PORT:-/dev/ttyUSB6}"
EPDG_FQDN="${EPDG_FQDN:-epdg.epc.mnc${MNC}.mcc${MCC}.pub.3gppnetwork.org}"
NETNS="${NETNS:-ims}"
EPDG_IP="${EPDG_IP:-}"
SRC_ADDR="${SRC_ADDR:-}"
KEEPALIVE_INTERVAL="${KEEPALIVE_INTERVAL:-20}"
VETH_SIP="${VETH_SIP:-veth-sip}"
VETH_IMS="${VETH_IMS:-veth-ims}"
VETH_IMS_ADDR="${VETH_IMS_ADDR:-10.99.0.1/30}"
VETH_SIP_ADDR="${VETH_SIP_ADDR:-10.99.0.2/30}"

log() { echo "[entrypoint] $*"; }

# --- Cleanup on exit ---------------------------------------------------------
DAEMON_SUPERVISOR_PID=""
KEEPALIVE_PID=""
SWU_PID=""
IMS_AGENT_SUPERVISOR_PID=""
SIP_AGENT_SUPERVISOR_PID=""
cleanup() {
    log "shutting down ..."
    [ -n "$DAEMON_SUPERVISOR_PID" ] && kill "$DAEMON_SUPERVISOR_PID" 2>/dev/null
    pkill -f "$GSM_SIP_BRIDGE_BIN --config" 2>/dev/null
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

# --- 1. Circuit-switched GSM-to-SIP daemon (always attempted) ---------------
if [ ! -x "$GSM_SIP_BRIDGE_BIN" ]; then
    log "FATAL: $GSM_SIP_BRIDGE_BIN not present in this image (build problem)"
    exit 1
fi
if [ ! -f "$GSM_SIP_BRIDGE_CONFIG" ]; then
    log "FATAL: $GSM_SIP_BRIDGE_CONFIG not mounted — see docker-compose.yml's config.toml volume"
    exit 1
fi

log "starting the circuit-switched GSM-to-SIP daemon, supervised..."
(
    while true; do
        "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG"
        log "gsm-sip-bridge daemon exited (status $?); restarting in 5s"
        sleep 5
    done
) &
DAEMON_SUPERVISOR_PID=$!

# --- 2. Inbound VoWiFi-to-SIP bridge (only if [vowifi].enabled) ------------
if ! "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" config vowifi-enabled; then
    log "[vowifi].enabled is not true in $GSM_SIP_BRIDGE_CONFIG — VoWiFi bridge not started"
    wait
    exit 0
fi

log "[vowifi].enabled — starting the VoWiFi/ePDG tunnel and bridge agents"

# --- Preflight ---------------------------------------------------------------
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

# --- Launch the SWu-IKEv2 dialer --------------------------------------------
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
( cd /opt/SWu-IKEv2 && \
  python3 -u swu_emulator.py -m "$MODEM_PORT" -a "$APN" -M "$MCC" -N "$MNC" \
        -d "$EPDG_IP" -n "$NETNS" "${SRC_OPT[@]}" < <(tail -f /dev/null) \
        > >(tee "$LOG") 2>&1 ) &
SWU_PID=$!

# --- Wait for tunnel readiness --------------------------------------------
# Wait for "STATE CONNECTED", not just the P-CSCF address line: the dialer
# prints P-CSCF IPV[46] ADDRESS from inside its IKE_AUTH response handler,
# but only calls set_routes() (which creates network namespace "$NETNS" and
# moves tun1 into it) afterwards, as a later step in its own state machine.
# Proceeding as soon as the address line appears is a race — the namespace
# may not exist yet, and every step below that touches "$NETNS" fails with
# "Cannot open network namespace" / "Invalid netns value" if it loses.
log "waiting for tunnel (P-CSCF assignment + netns/tun1 setup) ..."
for _ in $(seq 1 90); do
    if grep -q "STATE CONNECTED" "$LOG" 2>/dev/null; then
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

    # --- veth pair for the inbound VoWiFi bridge agents ---------------------
    # Agent A (vowifi-ims-agent) runs inside netns "$NETNS" alongside the SWu
    # tunnel/Gm-IPsec state; Agent B (vowifi-sip-agent) runs in this
    # container's default namespace (LAN, reachable to the PBX). Addresses
    # default to VowifiConfig's veth_local_addr (Agent A / ims-netns end) /
    # veth_peer_addr (Agent B / default-netns end) — override both ends
    # together if the config file's [vowifi] section is customized.
    log "creating veth pair ($VETH_SIP <-> $VETH_IMS in netns $NETNS) for the VoWiFi bridge agents..."
    # Both ends must be checked, not just ours. The tunnel dialer deletes and
    # recreates netns "$NETNS" on every reconnect, which destroys the
    # $VETH_IMS end with it while leaving our $VETH_SIP end behind — a
    # half-pair that looks fine from this side but leaves Agent A with no
    # route to Agent B ("Network is unreachable" on the control channel, with
    # the inbound call already answered). Rebuild the pair whenever the far
    # end is missing.
    if ip link show "$VETH_SIP" >/dev/null 2>&1 &&
        ! ip netns exec "$NETNS" ip link show "$VETH_IMS" >/dev/null 2>&1; then
        log "$VETH_IMS is gone from netns $NETNS (tunnel reconnect); rebuilding the veth pair"
        ip link delete "$VETH_SIP"
    fi
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

# --- Block on everything -----------------------------------------------------
wait
