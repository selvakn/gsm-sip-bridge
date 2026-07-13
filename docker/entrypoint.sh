#!/usr/bin/env bash
# Entrypoint for the unified gsm-sip-bridge image. Supervises up to two
# independent subsystems, deciding at startup which apply:
#
#   1. The circuit-switched GSM-to-SIP daemon — always started; it already
#      no-ops gracefully ("no EC20 modules found — waiting for retry") when
#      no supported modem is attached, so there's nothing to gate this on.
#   2. The inbound VoWiFi-to-SIP bridge (specs/011-vowifi-sip-bridge) — an
#      ePDG tunnel, a veth pair, and vowifi-ims-agent/vowifi-sip-agent —
#      started only if [vowifi].enabled = true in the mounted config.toml
#      (checked via `gsm-sip-bridge config vowifi-enabled` rather than
#      hand-parsing TOML in bash). The tunnel engine itself is selectable
#      via TUNNEL_ENGINE (specs/012-strongswan-epdg): "swu" (default during
#      the proving period) is the original SWu-IKEv2 Python dialer; the
#      "strongswan" flag is validated here but not yet implemented — that
#      lands in specs/012-strongswan-epdg Phase 4.
#
# The VoWiFi subsystem's tunnel setup creates network namespace "$NETNS"
# and installs the split-default routes THERE, so the container's own
# routing (used to reach the SIP server / ePDG) is untouched.
set -uo pipefail

TUNNEL_ENGINE="${TUNNEL_ENGINE:-swu}"
case "$TUNNEL_ENGINE" in
    swu | strongswan) ;;
    *)
        echo "[entrypoint] FATAL: invalid TUNNEL_ENGINE '$TUNNEL_ENGINE' (must be 'swu' or 'strongswan')" >&2
        exit 1
        ;;
esac

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
PCSCD_PID=""
CHARON_PID=""
CHARON_LOG_TAIL_PID=""
USIM_BRIDGE_SUPERVISOR_PID=""
STRONGSWAN_SUPERVISOR_PID=""
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
    [ -n "$STRONGSWAN_SUPERVISOR_PID" ] && kill "$STRONGSWAN_SUPERVISOR_PID" 2>/dev/null
    [ -n "$USIM_BRIDGE_SUPERVISOR_PID" ] && kill "$USIM_BRIDGE_SUPERVISOR_PID" 2>/dev/null
    pkill -f vowifi-usim-bridge 2>/dev/null
    [ -n "$CHARON_LOG_TAIL_PID" ] && kill "$CHARON_LOG_TAIL_PID" 2>/dev/null
    [ -n "$CHARON_PID" ] && kill "$CHARON_PID" 2>/dev/null
    [ -n "$PCSCD_PID" ] && kill "$PCSCD_PID" 2>/dev/null
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

log "[vowifi].enabled — starting the VoWiFi/ePDG tunnel and bridge agents (engine: $TUNNEL_ENGINE)"

# --- Preflight (shared by both engines) --------------------------------------
[ -e "$MODEM_PORT" ] || { log "FATAL: modem port $MODEM_PORT not present in container (check devices:)"; exit 1; }
if ! ip netns add __probe 2>/dev/null; then
    log "FATAL: cannot create network namespaces — add cap_add: SYS_ADMIN (and NET_ADMIN)"; exit 1
fi
ip netns del __probe 2>/dev/null || true

# --- Resolve ePDG IP (shared by both engines) --------------------------------
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

# --- Shared tail: veth pair + both VoWiFi bridge agents -------------------
# Called by either engine once its tunnel is up and $PCSCF_ADDR is known.
# Identical regardless of engine (FR-007/FR-006 — the agents don't know or
# care which engine built the tunnel they're sitting in).
start_shared_tail() {
    local pcscf_addr="$1"

    # Agent A (vowifi-ims-agent) runs inside netns "$NETNS" alongside the
    # tunnel/Gm-IPsec state; Agent B (vowifi-sip-agent) runs in this
    # container's default namespace (LAN, reachable to the PBX). Addresses
    # default to VowifiConfig's veth_local_addr (Agent A / ims-netns end) /
    # veth_peer_addr (Agent B / default-netns end) — override both ends
    # together if the config file's [vowifi] section is customized.
    log "creating veth pair ($VETH_SIP <-> $VETH_IMS in netns $NETNS) for the VoWiFi bridge agents..."
    # Both ends must be checked, not just ours. Under the swu engine the
    # tunnel dialer deletes and recreates netns "$NETNS" on every
    # reconnect, which destroys the $VETH_IMS end with it while leaving our
    # $VETH_SIP end behind — a half-pair that looks fine from this side but
    # leaves Agent A with no route to Agent B ("Network is unreachable" on
    # the control channel, with the inbound call already answered).
    # Rebuild the pair whenever the far end is missing. Under the
    # strongswan engine the namespace itself never gets deleted on
    # reconnect (FR-005) so this should be a no-op there in practice — kept
    # anyway since it's a correct, idempotent safety net regardless of
    # engine.
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

    : "$pcscf_addr" # reserved for future per-tail use; P-CSCF already on disk
}

if [ "$TUNNEL_ENGINE" = "strongswan" ]; then
    # --- strongSwan engine (specs/012-strongswan-epdg) -----------------------
    STRONGSWAN_TUN_IFACE="${STRONGSWAN_TUN_IFACE:-tun23}"
    STRONGSWAN_IF_ID="${STRONGSWAN_IF_ID:-23}"
    VPCD_HOST="${VPCD_HOST:-127.0.0.1}"
    VPCD_PORT="${VPCD_PORT:-35963}"

    # --- Idempotent netns + XFRM interface (T020, FR-005/FR-011) -----------
    # Pre-created once, here, by the entrypoint — not by charon or the
    # updown script — so it survives every future rekey/reconnect: only
    # ims.updown's address install/remove touches it after this point.
    # File-existence check, not `ip netns list | grep -x`: once an
    # interface lives in the namespace, iproute2 annotates the list output
    # with an id ("ims (id: 1)"), breaking an exact-name match — confirmed
    # by testing (a second entrypoint run against an already-populated
    # namespace mismatched and tried `ip netns add` again).
    if [ ! -e "/var/run/netns/$NETNS" ]; then
        ip netns add "$NETNS"
        log "created netns $NETNS"
    else
        log "netns $NETNS already exists, reusing"
    fi
    ip netns exec "$NETNS" ip link set lo up

    if ! ip netns exec "$NETNS" ip link show "$STRONGSWAN_TUN_IFACE" >/dev/null 2>&1; then
        if ip link show "$STRONGSWAN_TUN_IFACE" >/dev/null 2>&1; then
            # Leftover in the default netns from a previous run that didn't
            # get moved — absorb rather than fail (idempotent startup).
            ip link set "$STRONGSWAN_TUN_IFACE" netns "$NETNS"
        else
            ip link add "$STRONGSWAN_TUN_IFACE" type xfrm if_id "$STRONGSWAN_IF_ID"
            ip link set "$STRONGSWAN_TUN_IFACE" netns "$NETNS"
        fi
        log "created XFRM interface $STRONGSWAN_TUN_IFACE (if_id=$STRONGSWAN_IF_ID) in netns $NETNS"
    else
        log "XFRM interface $STRONGSWAN_TUN_IFACE already in netns $NETNS, reusing"
    fi
    ip netns exec "$NETNS" ip link set "$STRONGSWAN_TUN_IFACE" up
    ip netns exec "$NETNS" ip route replace default dev "$STRONGSWAN_TUN_IFACE" 2>/dev/null || true
    ip netns exec "$NETNS" ip -6 route replace default dev "$STRONGSWAN_TUN_IFACE" 2>/dev/null || true
    # Received IPsec traffic gets dropped if IPsec policy isn't disabled on
    # the interface itself (osmocom wiki's Option 2 walkthrough — "Very
    # important", reason unknown/FIXME upstream too).
    ip netns exec "$NETNS" sh -c "echo 1 > /proc/sys/net/ipv6/conf/$STRONGSWAN_TUN_IFACE/disable_policy" 2>/dev/null || true

    # --- Render the swanctl connection (T021) -------------------------------
    if [ -n "${IMSI:-}" ]; then
        log "using IMSI override from IMSI env var"
    else
        IMSI="$("$GSM_SIP_BRIDGE_BIN" vowifi-imsi --modem "$MODEM_PORT")"
        if [ -z "$IMSI" ]; then
            log "FATAL: failed to read IMSI from $MODEM_PORT (AT+CIMI) — see vowifi-imsi's own error above"
            exit 1
        fi
        log "read IMSI from SIM"
    fi

    SED_ARGS=(-e "s/@IMSI@/$IMSI/" -e "s/@MCC@/$MCC/" -e "s/@MNC@/$MNC/" -e "s/@EPDG_IP@/$EPDG_IP/")
    if [ -n "$SRC_ADDR" ]; then
        SED_ARGS+=(-e "s/@SRC_ADDR@/$SRC_ADDR/")
    else
        # No override: drop the local_addrs line entirely so charon
        # auto-selects a source address based on routing to $EPDG_IP
        # (mirrors the legacy engine's optional -s/SRC_ADDR flag).
        SED_ARGS+=(-e "/local_addrs.*@SRC_ADDR@/d")
    fi
    sed "${SED_ARGS[@]}" /etc/strongswan.d/swanctl-epdg.conf.template >/etc/swanctl/conf.d/epdg.conf
    log "rendered swanctl connection for mcc=$MCC mnc=$MNC epdg=$EPDG_IP"

    # --- pcscd + vowifi-usim-bridge (the virtual PC/SC reader) --------------
    mkdir -p /run/pcscd
    pcscd --foreground >/tmp/pcscd.log 2>&1 &
    PCSCD_PID=$!
    log "started pcscd (pid $PCSCD_PID)"

    log "starting vowifi-usim-bridge (default netns, talks to the modem + pcscd's vpcd), supervised..."
    (
        while true; do
            "$GSM_SIP_BRIDGE_BIN" -v vowifi-usim-bridge --modem "$MODEM_PORT" \
                --vpcd-host "$VPCD_HOST" --vpcd-port "$VPCD_PORT"
            log "vowifi-usim-bridge exited (status $?); restarting in 5s"
            sleep 5
        done
    ) &
    USIM_BRIDGE_SUPERVISOR_PID=$!

    # --- Start charon (T021) -------------------------------------------------
    mkdir -p /run
    : >/tmp/charon.log
    /usr/libexec/ipsec/charon &
    CHARON_PID=$!
    log "started charon (pid $CHARON_PID)"
    # Surface charon's filelog on docker logs, same purpose as the swu
    # engine's `tee` of /tmp/swu.log — FR-010's observability requirement.
    tail -f /tmp/charon.log 2>/dev/null | sed 's/^/[charon] /' &
    CHARON_LOG_TAIL_PID=$!

    sleep 2 # let the vici socket come up before swanctl talks to it
    if ! swanctl --load-all >/tmp/swanctl-load.log 2>&1; then
        log "WARNING: swanctl --load-all reported problems (see /tmp/swanctl-load.log)"
    fi

    # keyingtries=0 means charon retries forever internally, so
    # `swanctl --initiate` can block indefinitely waiting for a terminal
    # event that may never come — run it in the background and poll
    # readiness ourselves instead of waiting on its exit.
    swanctl --initiate --child ims >/tmp/swanctl-initiate.log 2>&1 &

    # --- Wait for tunnel readiness, indefinitely (T022/T023 merged) --------
    # A single foreground loop, not a "wait once, then give up and hand off
    # to a separate background supervisor" split: that split was tried and
    # is a real bug (found by live-testing against a real carrier, not by
    # reading the code) — an EAP-AKA round can be flatly rejected (a
    # definitive per-protocol outcome, not a timeout `keyingtries`/
    # `retry_initiate_interval` retry), and if the first attempt fails
    # before `start_shared_tail`/the keepalive/the ongoing supervisor ever
    # run, NOTHING re-checks even though charon (and a fresh
    # `swanctl --initiate`) may well succeed a few attempts later — the
    # veth pair and both agents would simply never start, indefinitely,
    # which directly defeats FR-004/FR-005's "no permanent give-up".
    # Logs progress periodically rather than hanging silently for however
    # long the network takes.
    log "waiting for the strongSwan tunnel (CHILD_SA + P-CSCF assignment) ..."
    ATTEMPT=0
    while true; do
        if grep -q "CHILD_SA.*established" /tmp/charon.log 2>/dev/null; then
            # Extract only the text AFTER "received P-CSCF server IP" before
            # applying the address regex — charon's filelog prefixes every
            # line with a "HH:MM:SS" timestamp, and the IPv6 regex below
            # (hex groups separated by colons) matches that timestamp too;
            # applying it to the whole line silently picked up "14:00:45"
            # as the "P-CSCF address" instead of the real one (confirmed
            # live: /tmp/pcscf ended up containing a timestamp, and
            # vowifi-ims-agent then refused to start with "invalid P-CSCF
            # address ... invalid IP address syntax").
            PCSCF_LINES="$(grep -oE 'received P-CSCF server IP .*' /tmp/charon.log | sed 's/^received P-CSCF server IP //')"
            PCSCF="$(echo "$PCSCF_LINES" | grep -oE '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$' | head -1)"
            PCSCF6=""
            if [ -z "$PCSCF" ]; then
                PCSCF6="$(echo "$PCSCF_LINES" | grep -oE '^([0-9a-fA-F]{0,4}:){2,}[0-9a-fA-F:]+$' | head -1)"
            fi
            if [ -n "$PCSCF" ] || [ -n "$PCSCF6" ]; then
                break
            fi
            log "CHILD_SA established but no P-CSCF line found yet; still waiting"
        fi
        if ! kill -0 "$CHARON_PID" 2>/dev/null; then
            log "FATAL: charon exited before establishing the tunnel (see /tmp/charon.log)."
            exit 1
        fi
        ATTEMPT=$((ATTEMPT + 1))
        if [ $((ATTEMPT % 15)) -eq 0 ]; then
            # ~30s of no CHILD_SA since the last (re-)initiate — a plain
            # timeout, or a rejected EAP round, both look the same from out
            # here: fire another attempt. keyingtries=0 keeps charon's own
            # internal retry going regardless; this covers the case where
            # the whole IKE_SA was torn down instead of merely retried.
            log "still waiting after ${ATTEMPT}x2s; re-initiating"
            swanctl --initiate --child ims >>/tmp/swanctl-initiate.log 2>&1 &
        fi
        sleep 2
    done

    PCSCF_ADDR="${PCSCF:-$PCSCF6}"
    log "tunnel UP. P-CSCF: $PCSCF_ADDR"
    echo "$PCSCF_ADDR" >/tmp/pcscf
    ip netns exec "$NETNS" ip addr show 2>/dev/null | sed 's/^/[epdg][netns] /'

    # --- Reliability supervision (T023) -----------------------------------
    # From here on the tunnel is up at least once; this loop's only job is
    # noticing if the CHILD_SA later disappears entirely (e.g. after an
    # unrecoverable rekey failure) and re-triggering --initiate — the
    # capability the swu engine never had at all (FR-004). Started only
    # now (not earlier) because it has nothing to check before the first
    # success; the loop above already covers "not up yet".
    (
        while true; do
            sleep 30
            if ! swanctl --list-sas 2>/dev/null | grep -q '^ims:'; then
                log "ims CHILD_SA missing; re-initiating"
                swanctl --initiate --child ims >>/tmp/swanctl-initiate.log 2>&1 &
            fi
        done
    ) &
    STRONGSWAN_SUPERVISOR_PID=$!

    # Same idle-tunnel keepalive rationale as the swu engine (TCP
    # connect, not ICMP — operators filter ICMP over the tunnel).
    (
        while true; do
            ip netns exec "$NETNS" bash -c "timeout 3 bash -c '>/dev/tcp/$PCSCF_ADDR/5060'" >/dev/null 2>&1
            sleep "$KEEPALIVE_INTERVAL"
        done
    ) &
    KEEPALIVE_PID=$!

    start_shared_tail "$PCSCF_ADDR"
else
    # --- swu engine: SWu-IKEv2 Python dialer (specs/011-vowifi-sip-bridge) --
    [ -c /dev/net/tun ] || { log "FATAL: /dev/net/tun missing (need --device /dev/net/tun + cap NET_ADMIN)"; exit 1; }

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

        start_shared_tail "$PCSCF_ADDR"
    fi
fi

# --- Block on everything -----------------------------------------------------
wait
