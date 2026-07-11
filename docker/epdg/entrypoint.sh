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
cleanup() {
    log "shutting down ..."
    [ -n "$KEEPALIVE_PID" ] && kill "$KEEPALIVE_PID" 2>/dev/null
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
fi

# --- Block on the dialer ---------------------------------------------------
wait "$SWU_PID"
