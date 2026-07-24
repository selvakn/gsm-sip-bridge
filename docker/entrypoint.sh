#!/usr/bin/env bash
# Entrypoint for the unified gsm-sip-bridge image. Supervises up to two
# independent subsystems, deciding at startup which apply:
#
#   1. The circuit-switched GSM-to-SIP daemon — always started; it already
#      no-ops gracefully ("no EC20 modules found — waiting for retry") when
#      no supported modem is attached, so there's nothing to gate this on.
#   2. The inbound VoWiFi-to-SIP bridge — now potentially **multiple
#      lines** (specs/013-multi-card-vowifi): one ePDG tunnel + veth pair +
#      vowifi-ims-agent per discovered VoWiFi SIM, plus one shared
#      vowifi-sip-agent presenting a single SIP identity to the PBX for all
#      of them. Whether VoWiFi runs at all, and how many lines, is decided
#      by `gsm-sip-bridge discover` (below) — never more than one process
#      probes a modem's serial ports at a time (specs/013-multi-card-vowifi
#      research.md item 3: running `discover` once, up front, before either
#      subsystem opens a single serial port, is what prevents the
#      circuit-switched daemon's own USB scan and VoWiFi's line discovery
#      from racing each other over the same candidate modem).
#      The tunnel engine is selectable via [vowifi].tunnel_engine
#      (specs/012-strongswan-epdg): "strongswan" (the default) has proper
#      IKE rekeying/re-auth/DPD/MOBIKE and a network namespace that
#      survives reconnects; "swu" is the original SWu-IKEv2 Python dialer,
#      kept as an explicit fallback.
#
# Each VoWiFi line's tunnel setup creates its own network namespace and
# installs the split-default routes THERE, so the container's own routing
# (used to reach the SIP server / ePDG) is untouched.
#
# All non-secret configuration (MCC/MNC/APN/tunnel engine/interface names/
# etc.) lives in config.toml's [vowifi] section, not env vars. This script
# bootstraps how to find the binary and its config file, asks the binary for
# global settings via `config vowifi-shell-env`, and asks it for the
# per-line line table via `discover --shell-env`
# (specs/013-multi-card-vowifi config/discovery consolidation).
set -uo pipefail

GSM_SIP_BRIDGE_BIN="${GSM_SIP_BRIDGE_BIN:-/usr/local/bin/gsm-sip-bridge}"
GSM_SIP_BRIDGE_CONFIG="${GSM_SIP_BRIDGE_CONFIG:-/etc/gsm-sip-bridge/config.toml}"

log() { echo "[entrypoint] $*"; }

if [ ! -x "$GSM_SIP_BRIDGE_BIN" ]; then
    log "FATAL: $GSM_SIP_BRIDGE_BIN not present in this image (build problem)"
    exit 1
fi
if [ ! -f "$GSM_SIP_BRIDGE_CONFIG" ]; then
    log "FATAL: $GSM_SIP_BRIDGE_CONFIG not mounted — see docker-compose.yml's config.toml volume"
    exit 1
fi

SHELL_ENV="$("$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" config vowifi-shell-env)" || {
    log "FATAL: 'config vowifi-shell-env' failed — see error above (bad config.toml?)"
    exit 1
}
eval "$SHELL_ENV"

# --- Cleanup on exit ---------------------------------------------------------
DAEMON_SUPERVISOR_PID=""
SIP_AGENT_SUPERVISOR_PID=""
# One shared pcscd for every strongswan-engine line — pcsc-lite's socket
# path is compiled in and NOT overridable at runtime (there is no
# PCSCLITE_CSOCK_NAME env override in modern pcsc-lite), so a second pcscd
# could never coexist anyway. It doesn't need to: charon's eap-sim-pcsc
# selects each line's SIM by IMSI across all of the one pcscd's vpcd slots
# (specs/013-multi-card-vowifi; vpcd built with --enable-vpcdslots, see
# docker/Dockerfile).
PCSCD_PID=""
PCSCD_LOG_TAIL_PID=""
declare -a CHARON_PIDS=()
declare -a CHARON_LOG_TAIL_PIDS=()
declare -a USIM_BRIDGE_SUPERVISOR_PIDS=()
declare -a LINE_SUPERVISOR_PIDS=()
declare -a SWU_PIDS=()
declare -a KEEPALIVE_PIDS=()
declare -a IMS_AGENT_SUPERVISOR_PIDS=()
declare -a STARTED_NETNS=()
# specs/020-volte-line-netns: the auto-discovered, multi-line VoLTE path's
# per-line carrier-agent supervisors, and the (netns, line-index) pairs that
# actually started — appended only after a line's namespace/veth/process
# setup succeeds, mirroring STARTED_NETNS's own append-on-success discipline.
# Distinct from VOLTE_SUPERVISOR_PID below, which belongs to the older
# single-`--modem`/registration-only paths (research.md R7 — out of scope for
# namespace isolation) and can be active at the same time as neither of these.
declare -a VOLTE_CARRIER_AGENT_SUPERVISOR_PIDS=()
declare -a VOLTE_STARTED_LINE_NETNS=()
declare -a VOLTE_STARTED_LINE_INDEX=()
VOLTE_BRIDGE_SUPERVISOR_PID=""

cleanup() {
    log "shutting down ..."
    [ -n "$DAEMON_SUPERVISOR_PID" ] && kill "$DAEMON_SUPERVISOR_PID" 2>/dev/null
    pkill -f "$GSM_SIP_BRIDGE_BIN --config" 2>/dev/null
    [ -n "$SIP_AGENT_SUPERVISOR_PID" ] && kill "$SIP_AGENT_SUPERVISOR_PID" 2>/dev/null
    pkill -f vowifi-sip-agent 2>/dev/null
    pkill -f vowifi-ims-agent 2>/dev/null
    pkill -f vowifi-usim-bridge 2>/dev/null
    for pid in "${LINE_SUPERVISOR_PIDS[@]:-}" "${KEEPALIVE_PIDS[@]:-}" \
        "${IMS_AGENT_SUPERVISOR_PIDS[@]:-}" "${USIM_BRIDGE_SUPERVISOR_PIDS[@]:-}" \
        "${SWU_PIDS[@]:-}" "${CHARON_LOG_TAIL_PIDS[@]:-}" "${CHARON_PIDS[@]:-}"; do
        [ -n "$pid" ] && kill "$pid" 2>/dev/null
    done
    if [ -n "${VOLTE_SUPERVISOR_PID:-}" ]; then
        # SIGKILL, not SIGTERM: the child may be blocked mid-AT-transaction on
        # the modem's serial port, and only an unblockable kill guarantees the
        # kernel closes that fd *now* — releasing the port before we open it for
        # `volte-pdn down`. A graceful signal could leave the child holding the
        # port past any timeout and race the teardown (interleaved reads / open
        # failure), leaving the displaced binding unrestored. The child has no
        # cleanup of its own to lose — this trap owns the PDN teardown.
        kill -KILL "$VOLTE_SUPERVISOR_PID" 2>/dev/null
        pkill -KILL -f "volte-register" 2>/dev/null
        pkill -KILL -f "volte-bridge" 2>/dev/null
        # Confirm the process table (and thus the serial fd) is clear before
        # touching the modem. Bounded so a zombie cannot hang shutdown; after
        # SIGKILL this returns almost immediately.
        for _ in $(seq 1 20); do
            pgrep -f "volte-register|volte-bridge" >/dev/null 2>&1 || break
            sleep 0.25
        done
        # Release the IMS PDN(s) so each modem's single host data path goes back
        # to whatever it was bound to before (FR-005). The inbound bridge
        # recorded each displaced context at attach; teardown passes it as
        # --restore-cid so tear_down rebinds it instead of leaving the data path
        # unbound. Best-effort: a failure here must not stop the rest of cleanup.
        #
        # A multi-modem bridge (empty modem_port) wrote a line manifest listing
        # every line's modem/cid/restore-cid — `volte-cleanup` tears them all
        # down from it. A single pinned modem (or the `volte-register` path,
        # which writes no manifest) is torn down directly here. `volte-cleanup`
        # is a no-op when no manifest exists, so running it unconditionally is
        # safe and covers the discovery case.
        "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" volte-cleanup \
            >/dev/null 2>&1
        if [ -n "$VOLTE_MODEM_PORT" ]; then
            volte_restore_cid=""
            if [ -f "${VOLTE_RESTORE_CID_PATH:-}" ]; then
                volte_restore_cid="$(cat "$VOLTE_RESTORE_CID_PATH" 2>/dev/null)"
            fi
            "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" volte-pdn \
                --action down --modem "$VOLTE_MODEM_PORT" \
                ${VOLTE_IFACE:+--iface "$VOLTE_IFACE"} --cid "${VOLTE_CID:-3}" \
                ${volte_restore_cid:+--restore-cid "$volte_restore_cid"} \
                >/dev/null 2>&1
        fi
    fi
    # specs/020-volte-line-netns: the auto-discovered, multi-line VoLTE path.
    # Kill every carrier-agent process first (SIGKILL, same reasoning as
    # above — one may be blocked mid-AT-transaction), then run each started
    # line's teardown *inside its own namespace* before that namespace is
    # deleted (research.md R6): `volte-pdn down`'s `netcfg::teardown` issues
    # namespace-scoped `ip`/sysctl commands that silently find nothing if run
    # from the wrong namespace, which would leave the modem's displaced data
    # context unrestored — the same failure class `e50ddca` already fixed
    # once for the single-namespace case.
    if [ "${#VOLTE_STARTED_LINE_NETNS[@]}" -gt 0 ]; then
        [ -n "$VOLTE_BRIDGE_SUPERVISOR_PID" ] && kill -KILL "$VOLTE_BRIDGE_SUPERVISOR_PID" 2>/dev/null
        pkill -KILL -f "volte-bridge" 2>/dev/null
        for pid in "${VOLTE_CARRIER_AGENT_SUPERVISOR_PIDS[@]:-}"; do
            [ -n "$pid" ] && kill -KILL "$pid" 2>/dev/null
        done
        pkill -KILL -f "volte-carrier-agent" 2>/dev/null
        for _ in $(seq 1 20); do
            pgrep -f "volte-bridge|volte-carrier-agent" >/dev/null 2>&1 || break
            sleep 0.25
        done
        for i in "${!VOLTE_STARTED_LINE_NETNS[@]}"; do
            netns="${VOLTE_STARTED_LINE_NETNS[$i]}"
            idx="${VOLTE_STARTED_LINE_INDEX[$i]}"
            ip netns exec "$netns" "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" \
                volte-cleanup --line "$idx" >/dev/null 2>&1
        done
    fi
    [ -n "$PCSCD_LOG_TAIL_PID" ] && kill "$PCSCD_LOG_TAIL_PID" 2>/dev/null
    [ -n "$PCSCD_PID" ] && kill "$PCSCD_PID" 2>/dev/null
    pkill -x pcscd 2>/dev/null
    for netns in "${STARTED_NETNS[@]:-}"; do
        [ -n "$netns" ] && ip netns del "$netns" 2>/dev/null
    done
    true
}
trap cleanup EXIT INT TERM

# --- 1. Discover once, up front (specs/013-multi-card-vowifi) ---------------
# Resolves the VoWiFi line table (auto-discovered SIMs, or the single
# explicitly-configured [vowifi].modem_port line) — deliberately BEFORE the
# circuit-switched daemon supervisor starts below, not after: the daemon
# does its own USB scan as soon as it starts, and if that scan ran
# concurrently with this one (as it would if `discover` ran after
# backgrounding the daemon), both processes could probe the same candidate
# modem's serial port at the same time and corrupt each other's AT
# exchange. Running `discover` to completion first — and skipping it
# entirely when VoWiFi is disabled, since nothing needs the excluded-ports
# file in that case — is what research.md item 3 calls out as the fix.
#
# Modem IMS mode (AT+QCFG="ims", see gsm-sip-bridge/src/vowifi/ims_mode.rs)
# is reconciled per line, not here: neither VoWiFi nor the native-LTE VoLTE
# path (specs/020-volte-line-netns) can coexist with the modem's own VoLTE
# stack (all three would share the SIM's IMPU and the modem's IMEI-derived
# +sip.instance, so the network tears one binding down as a re-registration
# of another — observed against Airtel on both host-driven paths), so every
# resolved line's modem needs its IMS forced off before that line's
# tunnel/registration starts, regardless of which subsystem the line belongs
# to. See the shared `reconcile_line_ims_mode` (defined below, called from
# both `start_line_strongswan`/`start_line_swu` and the VoLTE per-line loop).
#
# Deliberately narrower than this project's single-line-era behavior in one
# respect: that version reconciled bidirectionally regardless of
# [vowifi].enabled, so a modem could also be put back into IMS_ENABLED when
# VoWiFi was off. Here, nothing runs at all when neither host-driven path is
# enabled (no `discover`, no per-line loop) — a modem previously forced into
# IMS_DISABLED by an earlier run stays that way until one is re-enabled.
# Accepted as harmless: the circuit-switched bridge never relies on the
# modem's own IMS/VoLTE registration (it drives calls via AT+ATA/ATD
# directly), so the only thing left idle is a capability this project
# doesn't otherwise use for that modem.
VOWIFI_ENABLED=0
if "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" config vowifi-enabled; then
    VOWIFI_ENABLED=1
    DISCOVER_ENV="$("$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" discover --shell-env)" || {
        log "FATAL: 'discover' failed — see error above"
        exit 1
    }
    eval "$DISCOVER_ENV"
    log "discover: LINE_COUNT=$LINE_COUNT"
fi

# specs/020-volte-line-netns: the same "discover once, up front" fix applies
# to VoLTE's auto-discovered multi-line path — resolving (and persisting) the
# line table here, before the circuit-switched daemon's own scan starts
# below, is what lets that scan's *ongoing* re-scans exclude every VoLTE
# line's modem by its actually-resolved card id (not just a config-pinned
# port, which a modem exposing several `ttyUSB` interfaces can defeat — found
# live: the circuit-switched daemon's rescan settled on a different port than
# the one `[[volte.line]]` pinned, and both subsystems' AT traffic
# interleaved on the same SIM, surfacing as intermittent `AT+CIMI` failures
# on the VoLTE side). `config volte-shell-env` is read regardless of
# bridge_inbound/modem_port so the mutual-exclusion check against VoWiFi
# below can fail fast, before the circuit-switched daemon even starts.
VOLTE_ENABLED=0
if "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" config volte-enabled; then
    VOLTE_ENABLED=1
    VOLTE_ENV="$("$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" config volte-shell-env)" || {
        log "FATAL: 'config volte-shell-env' failed — see error above (bad config.toml?)"
        exit 1
    }
    eval "$VOLTE_ENV"

    if [ "$VOWIFI_ENABLED" -eq 1 ]; then
        log "FATAL: [volte].enabled and [vowifi].enabled are both true. They register the"
        log "       same IMPU with the same instance-id, so each would tear the other's"
        log "       binding down. Enable exactly one."
        exit 1
    fi

    # Where the inbound bridge records the context its IMS PDN displaced, so
    # `cleanup` can `--restore-cid` it on teardown (the bridge never runs its
    # own detach — its accept loop does not return).
    VOLTE_RESTORE_CID_PATH="/run/volte-restore-cid"

    if [ "${VOLTE_BRIDGE_INBOUND:-0}" -eq 1 ] && [ -z "$VOLTE_MODEM_PORT" ]; then
        DISCOVER_LINES_ENV="$("$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" \
            volte-discover-lines --shell-env --restore-cid-path "$VOLTE_RESTORE_CID_PATH")" || {
            log "FATAL: 'volte-discover-lines' failed — see error above"
            exit 1
        }
        eval "$DISCOVER_LINES_ENV"
        log "volte-discover-lines: VOLTE_LINE_COUNT=${VOLTE_LINE_COUNT:-0}"
    fi
fi

# Reconciles this line's modem's own IMS/VoLTE stack with whether *this
# host* wants to register it itself — either subsystem
# (gsm-sip-bridge/src/vowifi/ims_mode.rs): VoWiFi and the modem's own VoLTE
# registration cannot coexist — they share the SIM's IMPU and the modem's
# IMEI-derived +sip.instance, so the network treats one as a
# re-registration of the other and tears the older binding down (observed
# against Airtel: our binding torn down ~0.7s after being granted). Must
# run before anything else on this line touches the modem: reconciling can
# reboot the module (~30s), which would yank the port out from under a
# concurrent AT+CIMI/PLMN-derivation call.
#
# Shared (not scoped inside either subsystem's `if` block, unlike originally
# — specs/020-volte-line-netns found live that a VoLTE-only deployment never
# called this at all, leaving the modem's own IMS stack enabled and fighting
# our registration for the same IMPU) so both the VoWiFi and VoLTE per-line
# loops below can call it regardless of which one is active. The Rust side
# already computes the right answer for whichever is enabled
# (`config.vowifi.enabled || config.volte.enabled` — never both, `volte::guard`
# refuses that combination at the registration level).
#
# A failure here is scoped to *this line only* (return 1, caller skips it
# and continues with the rest, matching every other per-line FATAL check in
# this script) — unlike gsm-sip-bridge's own single-line-era `modem-ims`
# entrypoint step, which exited the whole container, appropriate when there
# was only ever one line to lose.
reconcile_line_ims_mode() {
    local idx="$1" modem="$2"
    log "line $idx: reconciling the modem's IMS mode ..."
    if ! "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" modem-ims --modem "$modem"; then
        log "line $idx: FATAL: could not put the modem's IMS stack in the mode this deployment needs (see the error above); skipping this line"
        return 1
    fi
}

# --- 2. Circuit-switched GSM-to-SIP daemon (always attempted) ---------------
log "starting the circuit-switched GSM-to-SIP daemon, supervised..."
(
    while true; do
        "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG"
        log "gsm-sip-bridge daemon exited (status $?); restarting in 5s"
        sleep 5
    done
) &
DAEMON_SUPERVISOR_PID=$!

# --- 3. Inbound VoWiFi-to-SIP bridge (only if [vowifi].enabled) ------------
# NOTE: this used to `wait; exit 0` when VoWiFi was disabled, which made the
# host-side LTE block far below UNREACHABLE — and since the two are mutually
# exclusive (enabling both is fatal), that meant [volte] could never start from
# this script at all. The VoWiFi stack is now *skipped* rather than terminal,
# so execution reaches the LTE block either way and one `wait` at the end
# covers whichever supervisors were started.
if [ "$VOWIFI_ENABLED" -eq 1 ]; then

if [ "$LINE_COUNT" -eq 0 ]; then
    log "PROMINENT ERROR: [vowifi].enabled is true but no usable VoWiFi line was discovered \
(no AT-capable modem with a ready SIM found, or all candidates are already \
serving the circuit-switched bridge) — the VoWiFi subsystem will NOT start \
this run. The circuit-switched daemon is unaffected and keeps running."
    wait
    exit 0
fi

log "[vowifi].enabled — starting $LINE_COUNT VoWiFi line(s) (engine: $TUNNEL_ENGINE)"

if ! ip netns add __probe 2>/dev/null; then
    log "FATAL: cannot create network namespaces — add cap_add: SYS_ADMIN (and NET_ADMIN)"
    exit 1
fi
ip netns del __probe 2>/dev/null || true

# --- Shared helpers (parametrized per line; no global state) -----------------

# charon.log accumulates every "received P-CSCF server IP" line for the life
# of the container — including ones from a later re-auth/rekey that assigned
# a *different* P-CSCF than the first. Picks the chronologically last
# matching line overall (`tail -1` after filtering to valid v4/v6
# addresses), not the last of one family checked first (Greptile PR #2,
# specs/012-strongswan-epdg).
extract_latest_pcscf() {
    local log_file="$1"
    local lines
    lines="$(grep -oE 'received P-CSCF server IP .*' "$log_file" 2>/dev/null | sed 's/^received P-CSCF server IP //')"
    echo "$lines" | grep -E '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$|^([0-9a-fA-F]{0,4}:){2,}[0-9a-fA-F:]+$' | tail -1
}

# Idempotently ensures netns "$1" and its pre-created XFRM interface "$2"
# (if_id "$3") exist (specs/012-strongswan-epdg T020, FR-005/FR-011), pinned
# per line since specs/013-multi-card-vowifi replicates this recipe once per
# line rather than sharing one namespace/interface across lines.
ensure_epdg_interface() {
    local netns="$1" tun_iface="$2" if_id="$3"
    if [ ! -e "/var/run/netns/$netns" ]; then
        ip netns add "$netns"
        log "created netns $netns"
    else
        log "netns $netns already exists, reusing"
    fi
    ip netns exec "$netns" ip link set lo up

    if ! ip netns exec "$netns" ip link show "$tun_iface" >/dev/null 2>&1; then
        if ip link show "$tun_iface" >/dev/null 2>&1; then
            # Leftover in the default netns from a previous run that didn't
            # get moved — absorb rather than fail (idempotent startup).
            ip link set "$tun_iface" netns "$netns"
        else
            ip link add "$tun_iface" type xfrm if_id "$if_id"
            ip link set "$tun_iface" netns "$netns"
        fi
        log "created XFRM interface $tun_iface (if_id=$if_id) in netns $netns"
    else
        log "XFRM interface $tun_iface already in netns $netns, reusing"
    fi
    ip netns exec "$netns" ip link set "$tun_iface" up
    ip netns exec "$netns" ip route replace default dev "$tun_iface" 2>/dev/null || true
    ip netns exec "$netns" ip -6 route replace default dev "$tun_iface" 2>/dev/null || true
    # Received IPsec traffic gets dropped if IPsec policy isn't disabled on
    # the interface itself (osmocom wiki's Option 2 walkthrough).
    ip netns exec "$netns" sh -c "echo 1 > /proc/sys/net/ipv6/conf/$tun_iface/disable_policy" 2>/dev/null || true
}

# reconcile_line_ims_mode is now defined earlier, shared with the VoLTE
# per-line loop below (specs/020-volte-line-netns) — see its definition
# right after "1. Discover once, up front".

# Veth pair + this line's vowifi-ims-agent, supervised. Agent B
# (vowifi-sip-agent) is started once, after every line's veth pair exists —
# see the bottom of this script.
start_line_tail() {
    local idx="$1" netns="$2" veth_sip="$3" veth_ims="$4" veth_sip_addr="$5" veth_ims_addr="$6" card_id="$7"

    log "line $idx ($card_id): creating veth pair ($veth_sip <-> $veth_ims in netns $netns)..."
    # Both ends must be checked, not just ours: under the swu engine the
    # tunnel dialer deletes and recreates the netns on every reconnect,
    # destroying the ims-side end while leaving the sip-side end behind — a
    # half-pair that looks fine from this side but leaves Agent A with no
    # route to Agent B. Rebuild whenever the far end is missing (a no-op
    # under strongswan, whose netns never gets deleted on reconnect).
    if ip link show "$veth_sip" >/dev/null 2>&1 &&
        ! ip netns exec "$netns" ip link show "$veth_ims" >/dev/null 2>&1; then
        log "line $idx: $veth_ims is gone from netns $netns (tunnel reconnect); rebuilding the veth pair"
        ip link delete "$veth_sip"
    fi
    if ! ip link show "$veth_sip" >/dev/null 2>&1; then
        ip link add "$veth_sip" type veth peer name "$veth_ims" netns "$netns"
    else
        log "line $idx: veth pair already exists, reusing"
    fi
    ip addr replace "$veth_sip_addr" dev "$veth_sip"
    ip link set "$veth_sip" up
    ip netns exec "$netns" ip addr replace "$veth_ims_addr" dev "$veth_ims"
    ip netns exec "$netns" ip link set "$veth_ims" up
    log "line $idx: veth ready: $veth_sip=$veth_sip_addr (default netns), $veth_ims=$veth_ims_addr (netns $netns)"

    log "line $idx: starting vowifi-ims-agent (netns $netns), supervised..."
    (
        while true; do
            ip netns exec "$netns" "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" vowifi-ims-agent --line "$idx"
            log "line $idx: vowifi-ims-agent exited (status $?); restarting in 5s"
            sleep 5
        done
    ) &
    IMS_AGENT_SUPERVISOR_PIDS+=("$!")
}

# --- strongSwan engine, one full independent stack per line -----------------
# Deliberately one charon + one pcscd + one vpcd port per line
# (specs/013-multi-card-vowifi research.md item 4), not one shared charon
# with N swanctl connections: strongSwan's eap-sim-pcsc plugin has no
# documented way to pick among several PC/SC readers, so giving every line
# its own pcscd means its charon always sees exactly one reader — identical
# to the proven single-line arrangement, just replicated N times — and a
# crashed charon/pcscd on one line cannot touch any other line's process.

# Renders a per-line strongswan.conf: its own vici socket and filelog path —
# so this line's charon instance is fully independent of every other line's,
# never sharing a vici socket or log file with them. Launched via
# `STRONGSWAN_CONF="$conf" charon`/`STRONGSWAN_CONF="$conf" swanctl ...`
# (both charon and swanctl load their settings — including the vici socket
# — through this same file/env var; verified against the actual pinned
# source, src/swanctl/command.c's `command_dispatch()`: it reads
# `swanctl.socket`/`swanctl.plugins.vici.socket` from `lib->settings`
# *before* parsing any CLI flags, which is why the `swanctl { socket = ... }`
# block below exists — swanctl does NOT read `charon.plugins.vici.socket`).
#
# Deliberately does NOT set `charon.pidfile` here to get per-line pidfiles:
# tried that first, and it does not work — the raw `charon` binary's
# "already running" startup guard checks the unqualified `/var/run/charon.pid`
# regardless of this directive (verified live), and there is no `--pid-file`
# CLI flag either. `start_line_strongswan`'s `rm -f /var/run/charon.pid`
# immediately before each launch is what actually fixes the second-line-onward
# refusal (specs/013-multi-card-vowifi, found live-testing a genuine 2-line
# strongswan deployment for the first time).
render_line_strongswan_conf() {
    local idx="$1" vici_socket="$2" charon_log="$3"
    local conf="/etc/strongswan-line-$idx.conf"
    cat >"$conf" <<EOF
charon {
    plugins {
        include /etc/strongswan.d/charon/*.conf
        vici {
            socket = unix://$vici_socket
        }
    }
    filelog {
        line$idx {
            path = $charon_log
            default = 1
            ike = 1
            cfg = 1
            append = no
            flush_line = yes
            ike_name = yes
            time_format = %Y-%m-%d %H:%M:%S
        }
    }
}
swanctl {
    socket = unix://$vici_socket
}
include /etc/strongswan.d/charon-extra.conf
EOF
    echo "$conf"
}

# Renders a per-line swanctl.conf pointing at this line's own conf.d
# directory (never the shared /etc/swanctl/conf.d/) so `swanctl --load-all
# --file <this>` only ever loads *this* line's "ims" connection into *this*
# line's charon — sharing one directory across lines would load every
# line's same-named "ims" connection into every charon instance. `--file`
# is `--load-all`'s own option (src/swanctl/commands/load_all.c: `-f,
# --file "custom path to swanctl.conf"`) — verified against the pinned
# source, and must come *after* `--load-all` on the command line (swanctl's
# top-level `getopt_long` pass only recognizes registered command names
# until one matches; a global/per-command flag given before the command
# name comes back "unrecognized option" — found live-testing).
render_line_swanctl_conf() {
    local idx="$1"
    local conf_dir="/etc/swanctl/conf.d-$idx"
    local conf="/etc/swanctl-line-$idx.conf"
    mkdir -p "$conf_dir"
    echo "include $conf_dir/*.conf" >"$conf"
    echo "$conf"
}

# Renders this line's updown wrapper: sets NETNS/STRONGSWAN_TUN_IFACE (which
# the shared /etc/strongswan.d/ims.updown reads to know which netns/interface
# to install the carrier-assigned address on — see that script's own
# comments) to this line's own values, then execs the shared script
# unchanged, so the verb-handling logic itself still lives in exactly one
# place.
#
# A wrapper, not an env-var export on the `charon` launch line itself: tried
# that first, and it does not work — charon does not propagate its own
# launch environment down into the updown program it execs on CHILD_SA
# up/down (verified live), so every line fell through to the script's
# defaults ("ims"/"tun23", i.e. line 0's values) regardless. A script that
# sets the vars and execs the next program *is* that program's environment,
# no propagation required.
render_line_updown_script() {
    local idx="$1" netns="$2" tun_iface="$3"
    local script="/etc/strongswan.d/ims-updown-$idx.sh"
    cat >"$script" <<EOF
#!/bin/sh
NETNS="$netns" STRONGSWAN_TUN_IFACE="$tun_iface" exec /etc/strongswan.d/ims.updown "\$@"
EOF
    chmod +x "$script"
    echo "$script"
}

start_line_strongswan() {
    local idx="$1"
    local card_id="${LINE_CARD_ID[idx]}"
    local modem="${LINE_MODEM_PORT[idx]}"
    local netns="${LINE_NETNS[idx]}"
    local tun_iface="${LINE_STRONGSWAN_TUN_IFACE[idx]}"
    local if_id="${LINE_STRONGSWAN_IF_ID[idx]}"
    local mcc="${LINE_MCC[idx]}"
    local mnc="${LINE_MNC[idx]}"
    local vpcd_port="${LINE_VPCD_PORT[idx]}"
    local pcscf_path="${LINE_PCSCF_SOURCE_PATH[idx]}"
    local veth_local="${LINE_VETH_LOCAL_ADDR[idx]}"
    local veth_peer="${LINE_VETH_PEER_ADDR[idx]}"
    local veth_sip="${LINE_VETH_SIP_IFACE[idx]}"
    local veth_ims="${LINE_VETH_IMS_IFACE[idx]}"
    local charon_log="/tmp/charon-$idx.log"
    local vici_socket="/var/run/charon-$idx.vici"
    local strongswan_conf
    strongswan_conf="$(render_line_strongswan_conf "$idx" "$vici_socket" "$charon_log")"
    local swanctl_top_conf
    swanctl_top_conf="$(render_line_swanctl_conf "$idx")"
    local swanctl_conf="/etc/swanctl/conf.d-$idx/epdg.conf"

    log "line $idx ($card_id): modem=$modem netns=$netns mcc=$mcc mnc=$mnc"

    if [ ! -e "$modem" ]; then
        log "line $idx: FATAL: modem port $modem not present in container (check devices:); skipping this line"
        return 1
    fi

    reconcile_line_ims_mode "$idx" "$modem" || return 1

    if [ -z "$mcc" ] || [ -z "$mnc" ]; then
        log "line $idx: mcc/mnc not set — deriving the home PLMN from the SIM ..."
        local plmn
        plmn="$("$GSM_SIP_BRIDGE_BIN" vowifi-plmn --modem "$modem")" || plmn=""
        read -r mcc mnc <<<"$plmn"
        if [ -z "${mcc:-}" ] || [ -z "${mnc:-}" ]; then
            log "line $idx: FATAL: could not derive MCC/MNC from $modem; skipping this line"
            return 1
        fi
        log "line $idx: derived home PLMN: mcc=$mcc mnc=$mnc"
    fi

    local epdg_fqdn="${EPDG_FQDN:-}"
    if [ -z "$epdg_fqdn" ]; then
        epdg_fqdn="epdg.epc.mnc${mnc}.mcc${mcc}.pub.3gppnetwork.org"
    fi

    local epdg_ip="${EPDG_IP:-}"
    if [ -n "$epdg_ip" ]; then
        log "line $idx: using ePDG IP override: $epdg_ip"
    else
        log "line $idx: resolving $epdg_fqdn ..."
        epdg_ip="$(dig +short "$epdg_fqdn" A | grep -E '^[0-9.]+$' | head -1)"
        if [ -z "$epdg_ip" ]; then
            log "line $idx: FATAL: could not resolve $epdg_fqdn; skipping this line"
            return 1
        fi
        log "line $idx: resolved ePDG: $epdg_ip"
    fi

    ensure_epdg_interface "$netns" "$tun_iface" "$if_id"
    STARTED_NETNS+=("$netns")

    local imsi="${IMSI:-}"
    if [ -n "$imsi" ]; then
        log "line $idx: using IMSI override from vowifi.imsi_override"
    else
        imsi="$("$GSM_SIP_BRIDGE_BIN" vowifi-imsi --modem "$modem")"
        if [ -z "$imsi" ]; then
            log "line $idx: FATAL: failed to read IMSI from $modem (AT+CIMI); skipping this line"
            return 1
        fi
        log "line $idx: read IMSI from SIM"
    fi

    local updown_script
    updown_script="$(render_line_updown_script "$idx" "$netns" "$tun_iface")"

    # `|` delimiter for @UPDOWN@ specifically: its replacement is a filesystem
    # path containing `/`, which would break a plain `s/.../.../ ` substitution.
    local sed_args=(
        -e "s/@IMSI@/$imsi/" -e "s/@MCC@/$mcc/" -e "s/@MNC@/$mnc/" -e "s/@EPDG_IP@/$epdg_ip/"
        -e "s/@IF_ID@/$if_id/" -e "s|@UPDOWN@|$updown_script|"
    )
    if [ -n "${SRC_ADDR:-}" ]; then
        sed_args+=(-e "s/@SRC_ADDR@/$SRC_ADDR/")
    else
        sed_args+=(-e "/local_addrs.*@SRC_ADDR@/d")
    fi
    sed "${sed_args[@]}" /etc/strongswan.d/swanctl-epdg.conf.template >"$swanctl_conf"
    log "line $idx: rendered swanctl connection ($swanctl_conf) for mcc=$mcc mnc=$mnc epdg=$epdg_ip"

    # pcscd is the single shared instance started before this loop — this
    # line connects its vowifi-usim-bridge to its own vpcd slot's port
    # ($vpcd_port = base + idx), and its charon's eap-sim-pcsc finds this
    # line's SIM among all the shared reader's slots by IMSI.
    log "line $idx: starting vowifi-usim-bridge (talks to modem $modem + vpcd slot on port $vpcd_port), supervised..."
    (
        while true; do
            "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" vowifi-usim-bridge --modem "$modem" \
                --vpcd-host "$VPCD_HOST" --vpcd-port "$vpcd_port"
            log "line $idx: vowifi-usim-bridge exited (status $?); restarting in 5s"
            sleep 5
        done
    ) &
    USIM_BRIDGE_SUPERVISOR_PIDS+=("$!")

    mkdir -p /run
    : >"$charon_log"
    # charon's own "already running" startup guard checks the unqualified
    # /var/run/charon.pid regardless of this line's `pidfile =` setting in
    # strongswan-line-$idx.conf (verified live: the directive is accepted but
    # not honored by the raw charon binary, no --pid-file CLI flag exists
    # either) — so with two or more lines, every line after the first refuses
    # to start at all, finding the prior line's leftover pidfile. Removing it
    # right before each launch is safe: lines start sequentially (this loop),
    # never concurrently, and nothing reads the pidfile's *content* afterward
    # (swanctl/vici use the per-line socket, not this file).
    rm -f /var/run/charon.pid
    # ims.updown (invoked by charon on CHILD_SA up/down) reads NETNS/
    # STRONGSWAN_TUN_IFACE from ITS OWN environment, defaulting to "ims"/
    # "tun23" when unset — exactly line 0's (unindexed) values. Without these
    # exports every line's charon falls through to that same default,
    # installing its carrier-assigned address on *line 0's* interface instead
    # of its own — found live-testing a genuine second line for the first
    # time (specs/013-multi-card-vowifi): line 1's address landed on netns
    # "ims"/tun23, leaving its own tun23-1 with no global address and every
    # subsequent connection attempt "Host unreachable".
    STRONGSWAN_CONF="$strongswan_conf" /usr/libexec/ipsec/charon &
    local charon_pid=$!
    CHARON_PIDS+=("$charon_pid")
    log "line $idx: started charon (pid $charon_pid, vici $vici_socket)"
    tail -f "$charon_log" 2>/dev/null | sed "s/^/[charon][line $idx] /" &
    CHARON_LOG_TAIL_PIDS+=("$!")

    sleep 2 # let the vici socket come up before swanctl talks to it
    if ! STRONGSWAN_CONF="$strongswan_conf" swanctl --load-all --file "$swanctl_top_conf" >"/tmp/swanctl-load-$idx.log" 2>&1; then
        log "line $idx: WARNING: swanctl --load-all reported problems (see /tmp/swanctl-load-$idx.log)"
    fi
    STRONGSWAN_CONF="$strongswan_conf" swanctl --initiate --child ims >"/tmp/swanctl-initiate-$idx.log" 2>&1 &

    log "line $idx: waiting for the strongSwan tunnel (CHILD_SA + P-CSCF assignment) ..."
    local attempt=0 stuck_without_pcscf=0 pcscf_addr=""
    while true; do
        if grep -q "CHILD_SA.*established" "$charon_log" 2>/dev/null; then
            pcscf_addr="$(extract_latest_pcscf "$charon_log")"
            if [ -n "$pcscf_addr" ]; then
                break
            fi
            log "line $idx: CHILD_SA established but no P-CSCF line found yet; still waiting"
            stuck_without_pcscf=1
        else
            stuck_without_pcscf=0
        fi
        if ! kill -0 "$charon_pid" 2>/dev/null; then
            log "line $idx: FATAL: charon exited before establishing the tunnel (see $charon_log); skipping this line"
            return 1
        fi
        attempt=$((attempt + 1))
        if [ $((attempt % 15)) -eq 0 ]; then
            if [ "$stuck_without_pcscf" -eq 1 ]; then
                log "line $idx: CHILD_SA established but no P-CSCF after ${attempt}x2s; terminating and re-initiating fresh"
                STRONGSWAN_CONF="$strongswan_conf" swanctl --terminate --ike ims >/dev/null 2>&1 || true
            else
                log "line $idx: still waiting after ${attempt}x2s; re-initiating"
            fi
            STRONGSWAN_CONF="$strongswan_conf" swanctl --initiate --child ims >>"/tmp/swanctl-initiate-$idx.log" 2>&1 &
        fi
        sleep 2
    done

    log "line $idx: tunnel UP. P-CSCF: $pcscf_addr"
    echo "$pcscf_addr" >"$pcscf_path"
    ip netns exec "$netns" ip addr show 2>/dev/null | sed "s/^/[epdg][line $idx][netns] /"

    # --- Reliability supervision, scoped to this one line (FR-013) ---------
    # A dead charon or a broken vici connection is recovered by restarting
    # *this line's* charon process only — never a container-wide restart,
    # which would tear down every other (possibly healthy) line. This is
    # the one place multi-card VoWiFi's per-line isolation requirement
    # (FR-013) changes behavior from the single-line 012 recipe, which used
    # to `kill -TERM $$` the whole script and rely on `restart:
    # unless-stopped` — appropriate when there was only ever one line to
    # lose, not once there can be several.
    (
        while true; do
            sleep 30

            if ! kill -0 "$charon_pid" 2>/dev/null; then
                log "line $idx: charon exited after the tunnel was established; restarting charon for this line only"
                : >"$charon_log"
                rm -f /var/run/charon.pid
                STRONGSWAN_CONF="$strongswan_conf" /usr/libexec/ipsec/charon &
                charon_pid=$!
                CHARON_PIDS+=("$charon_pid")
                sleep 2
                STRONGSWAN_CONF="$strongswan_conf" swanctl --load-all --file "$swanctl_top_conf" >>"/tmp/swanctl-load-$idx.log" 2>&1 || true
                STRONGSWAN_CONF="$strongswan_conf" swanctl --initiate --child ims >>"/tmp/swanctl-initiate-$idx.log" 2>&1 &
                pkill -f "vowifi-ims-agent --line $idx\$" 2>/dev/null || true
                continue
            fi

            # tun can vanish from the kernel entirely while swanctl still
            # reports the CHILD_SA ESTABLISHED/INSTALLED (observed live,
            # specs/012-strongswan-epdg) — recreate and force a clean
            # terminate+reinitiate rather than trusting the desynced SA.
            if ! ip netns exec "$netns" ip link show "$tun_iface" >/dev/null 2>&1; then
                log "line $idx: $tun_iface missing from netns $netns; recreating and forcing reinitiate"
                ensure_epdg_interface "$netns" "$tun_iface" "$if_id"
                STRONGSWAN_CONF="$strongswan_conf" swanctl --terminate --ike ims >/dev/null 2>&1 || true
                STRONGSWAN_CONF="$strongswan_conf" swanctl --initiate --child ims >>"/tmp/swanctl-initiate-$idx.log" 2>&1 &
                pkill -f "vowifi-ims-agent --line $idx\$" 2>/dev/null || true
                continue
            fi

            local sas_output sas_status
            # Captured first, not piped directly into grep -q: pipefail
            # + grep's early exit on match SIGPIPEs a live swanctl mid-write,
            # which then outranks grep's own successful match (Greptile PR #2,
            # specs/012-strongswan-epdg).
            sas_output="$(STRONGSWAN_CONF="$strongswan_conf" swanctl --list-sas 2>/dev/null)"
            sas_status=$?
            if [ "$sas_status" -ne 0 ]; then
                log "line $idx: swanctl --list-sas failed (vici connection broken); restarting charon for this line only"
                kill "$charon_pid" 2>/dev/null
                : >"$charon_log"
                rm -f /var/run/charon.pid
                STRONGSWAN_CONF="$strongswan_conf" /usr/libexec/ipsec/charon &
                charon_pid=$!
                CHARON_PIDS+=("$charon_pid")
                sleep 2
                STRONGSWAN_CONF="$strongswan_conf" swanctl --load-all --file "$swanctl_top_conf" >>"/tmp/swanctl-load-$idx.log" 2>&1 || true
                STRONGSWAN_CONF="$strongswan_conf" swanctl --initiate --child ims >>"/tmp/swanctl-initiate-$idx.log" 2>&1 &
                pkill -f "vowifi-ims-agent --line $idx\$" 2>/dev/null || true
                continue
            fi
            if ! grep -q '^ims:' <<<"$sas_output"; then
                log "line $idx: ims CHILD_SA missing; re-initiating"
                STRONGSWAN_CONF="$strongswan_conf" swanctl --initiate --child ims >>"/tmp/swanctl-initiate-$idx.log" 2>&1 &
            fi

            # A rekey/re-auth can assign a *different* P-CSCF without "ims:"
            # ever going missing above — refresh the file and restart this
            # line's vowifi-ims-agent (its own supervisor loop relaunches it
            # immediately, picking up the new address) whenever it changes.
            local latest_pcscf current_pcscf
            latest_pcscf="$(extract_latest_pcscf "$charon_log")"
            if [ -n "$latest_pcscf" ]; then
                current_pcscf="$(cat "$pcscf_path" 2>/dev/null || true)"
                if [ "$latest_pcscf" != "$current_pcscf" ]; then
                    log "line $idx: P-CSCF changed ($current_pcscf -> $latest_pcscf); refreshing and restarting vowifi-ims-agent"
                    echo "$latest_pcscf" >"$pcscf_path"
                    pkill -f "vowifi-ims-agent --line $idx\$" 2>/dev/null || true
                fi
            fi
        done
    ) &
    LINE_SUPERVISOR_PIDS+=("$!")

    # Idle-tunnel keepalive (TCP connect, not ICMP — operators filter ICMP
    # over the tunnel, confirmed on Vodafone India). Re-reads $pcscf_path
    # every cycle so it keeps pinging the right address after the
    # supervisor above refreshes it.
    (
        while true; do
            local pcscf_now
            pcscf_now="$(cat "$pcscf_path" 2>/dev/null || true)"
            if [ -n "$pcscf_now" ]; then
                ip netns exec "$netns" bash -c "timeout 3 bash -c '>/dev/tcp/$pcscf_now/5060'" >/dev/null 2>&1
            fi
            sleep "$KEEPALIVE_INTERVAL"
        done
    ) &
    KEEPALIVE_PIDS+=("$!")

    start_line_tail "$idx" "$netns" "$veth_sip" "$veth_ims" "$veth_peer/30" "$veth_local/30" "$card_id"
}

# --- swu engine, one dialer per line -----------------------------------------
# The original SWu-IKEv2 Python dialer (specs/011-vowifi-sip-bridge), kept as
# an explicit fallback (`[vowifi].tunnel_engine = "swu"`). Simpler process
# model than strongswan (no separate pcscd/vpcd — the dialer talks to the
# modem directly), so one dialer process per line is the whole story.
start_line_swu() {
    local idx="$1"
    local card_id="${LINE_CARD_ID[idx]}"
    local modem="${LINE_MODEM_PORT[idx]}"
    local netns="${LINE_NETNS[idx]}"
    local mcc="${LINE_MCC[idx]}"
    local mnc="${LINE_MNC[idx]}"
    local pcscf_path="${LINE_PCSCF_SOURCE_PATH[idx]}"
    local veth_local="${LINE_VETH_LOCAL_ADDR[idx]}"
    local veth_peer="${LINE_VETH_PEER_ADDR[idx]}"
    local veth_sip="${LINE_VETH_SIP_IFACE[idx]}"
    local veth_ims="${LINE_VETH_IMS_IFACE[idx]}"
    local log_file="/tmp/swu-$idx.log"

    log "line $idx ($card_id): modem=$modem netns=$netns mcc=$mcc mnc=$mnc"

    [ -c /dev/net/tun ] || {
        log "line $idx: FATAL: /dev/net/tun missing; skipping this line"
        return 1
    }
    [ -e "$modem" ] || {
        log "line $idx: FATAL: modem port $modem not present in container (check devices:); skipping this line"
        return 1
    }

    reconcile_line_ims_mode "$idx" "$modem" || return 1

    if [ -z "$mcc" ] || [ -z "$mnc" ]; then
        log "line $idx: mcc/mnc not set — deriving the home PLMN from the SIM ..."
        local plmn
        plmn="$("$GSM_SIP_BRIDGE_BIN" vowifi-plmn --modem "$modem")" || plmn=""
        read -r mcc mnc <<<"$plmn"
        if [ -z "${mcc:-}" ] || [ -z "${mnc:-}" ]; then
            log "line $idx: FATAL: could not derive MCC/MNC from $modem; skipping this line"
            return 1
        fi
    fi

    local src_opt=()
    [ -n "${SRC_ADDR:-}" ] && src_opt=(-s "$SRC_ADDR")

    : >"$log_file"
    log "line $idx: starting SWu-IKEv2: modem=$modem apn=$APN mcc=$mcc mnc=$mnc netns=$netns"
    ( cd /opt/SWu-IKEv2 &&
        python3 -u swu_emulator.py -m "$modem" -a "$APN" -M "$mcc" -N "$mnc" \
            -n "$netns" "${src_opt[@]}" <(tail -f /dev/null) \
            > >(tee "$log_file") 2>&1 ) &
    local swu_pid=$!
    SWU_PIDS+=("$swu_pid")

    log "line $idx: waiting for tunnel (P-CSCF assignment + netns/tun setup) ..."
    local connected=0
    for _ in $(seq 1 90); do
        if grep -q "STATE CONNECTED" "$log_file" 2>/dev/null; then
            connected=1
            break
        fi
        if ! kill -0 "$swu_pid" 2>/dev/null; then
            log "line $idx: FATAL: dialer exited before establishing the tunnel; skipping this line"
            return 1
        fi
        sleep 2
    done
    if [ "$connected" -ne 1 ]; then
        log "line $idx: FATAL: tunnel did not reach STATE CONNECTED within 180s; skipping this line"
        return 1
    fi

    local pcscf pcscf6
    pcscf="$(grep 'P-CSCF IPV4 ADDRESS' "$log_file" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
    if [ -z "$pcscf" ]; then
        pcscf6="$(grep 'P-CSCF IPV6 ADDRESS' "$log_file" | grep -oE '([0-9a-fA-F]{0,4}:){2,}[0-9a-fA-F:]+' | head -1)"
    fi
    local pcscf_addr="${pcscf:-${pcscf6:-}}"
    if [ -z "$pcscf_addr" ]; then
        log "line $idx: FATAL: STATE CONNECTED but no P-CSCF address found; skipping this line"
        return 1
    fi

    log "line $idx: tunnel UP. P-CSCF: $pcscf_addr"
    echo "$pcscf_addr" >"$pcscf_path"
    STARTED_NETNS+=("$netns")
    ip netns exec "$netns" ip addr show 2>/dev/null | sed "s/^/[epdg][line $idx][netns] /"

    (
        while true; do
            ip netns exec "$netns" bash -c "timeout 3 bash -c '>/dev/tcp/$pcscf_addr/5060'" >/dev/null 2>&1
            sleep "$KEEPALIVE_INTERVAL"
        done
    ) &
    KEEPALIVE_PIDS+=("$!")

    # No re-initiate-in-place concept for this engine — recovery is
    # restarting the dialer for this line only (scoped, unlike the old
    # single-line whole-container restart, per FR-013).
    (
        while true; do
            sleep 5
            if ! kill -0 "$swu_pid" 2>/dev/null; then
                log "line $idx: SWu-IKEv2 dialer exited after the tunnel was established; restarting this line's dialer"
                : >"$log_file"
                ( cd /opt/SWu-IKEv2 &&
                    python3 -u swu_emulator.py -m "$modem" -a "$APN" -M "$mcc" -N "$mnc" \
                        -n "$netns" "${src_opt[@]}" <(tail -f /dev/null) \
                        > >(tee "$log_file") 2>&1 ) &
                swu_pid=$!
                SWU_PIDS+=("$swu_pid")
                pkill -f "vowifi-ims-agent --line $idx\$" 2>/dev/null || true
            fi
        done
    ) &
    LINE_SUPERVISOR_PIDS+=("$!")

    start_line_tail "$idx" "$netns" "$veth_sip" "$veth_ims" "$veth_peer/30" "$veth_local/30" "$card_id"
}

# --- One shared pcscd for every strongswan-engine line ----------------------
# Started once, before the line loop: it serves ALL lines' SIMs through one
# vpcd reader with N slots (ports $VPCD_PORT, $VPCD_PORT+1, ... — vpcd built
# with --enable-vpcdslots, docker/Dockerfile). Each line's vowifi-usim-bridge
# connects to its own slot's port (LINE_VPCD_PORT[i], derived as base+i);
# each line's charon runs eap-sim-pcsc, which scans all slots and picks that
# line's SIM by IMSI. Supervised: if pcscd dies, every line's SIM auth
# breaks, so bring it back and let the per-line supervisors re-initiate.
if [ "$TUNNEL_ENGINE" = "strongswan" ]; then
    # vpcd's base port is configured in two places that MUST agree: the
    # driver's listener (/etc/reader.conf.d/vpcd, read by pcscd — the
    # reader's CHANNELID is the base slot; every other line's slot is
    # CHANNELID+index, assigned automatically by vpcd itself, not by
    # additional reader.conf entries) and each line's dial target
    # (LINE_VPCD_PORT[i] = $VPCD_PORT + i, passed to vowifi-usim-bridge).
    # Render the reader.conf from $VPCD_PORT (the config's base) so a
    # [vowifi].vpcd_port override moves every line's slot together — the
    # image's upstream copy hardcodes vsmartcard's single-slot default
    # (35963), which also sits inside the kernel's ephemeral range
    # (net.ipv4.ip_local_port_range, 32768-60999): under `network_mode:
    # host` an unrelated outbound connection can hold it first, and the
    # driver's bind() then fails with EADDRINUSE — the default base
    # (15963) sits below that range instead.
    VPCD_PORT_HEX="$(printf '0x%04X' "$VPCD_PORT")"
    cat >/etc/reader.conf.d/vpcd <<EOF
FRIENDLYNAME "Virtual PCD"
DEVICENAME   /dev/null:$VPCD_PORT_HEX
LIBPATH      /usr/lib/pcsc/drivers/serial/libifdvpcd.so
CHANNELID    $VPCD_PORT_HEX
EOF

    mkdir -p /run/pcscd
    (
        while true; do
            pcscd --foreground >/tmp/pcscd.log 2>&1
            log "pcscd exited (status $?); restarting in 5s"
            sleep 5
        done
    ) &
    PCSCD_PID=$!
    log "started shared pcscd supervisor (pid $PCSCD_PID); one vpcd reader, slots from $VPCD_PORT, up to 8"
    tail -f /tmp/pcscd.log 2>/dev/null | sed 's/^/[pcscd] /' &
    PCSCD_LOG_TAIL_PID=$!

    # Gate every line's startup on the driver's listener actually being up.
    # Without this, a reader that failed to register leaves each line's
    # charon reporting "no smart card reader" while its usim-bridge spins
    # on ECONNREFUSED forever — neither of which names the real fault.
    #
    # A plain connect probe is not sufficient on its own: the very failure
    # this guards against (an unrelated process holding the port) leaves
    # that process listening and answering the probe, and a usim-bridge
    # would then speak the vpcd protocol at it. But the listener cannot be
    # attributed to pcscd by PID either — under `network_mode: host` the
    # socket table is the host's while /proc is the container's, so
    # netstat reports the owner as "-" for every row and a PID match can
    # never succeed (it would FATAL on a perfectly healthy system).
    #
    # So take pcscd's own verdict: the driver logs its failed bind. An
    # empty log plus an answering base-slot port means the reader really
    # is ours — checking just the base slot is a reasonable proxy for the
    # whole reader, since every slot comes from this one driver/one
    # reader.conf entry together.
    VPCD_READY=0
    for _ in $(seq 1 20); do
        if ! kill -0 "$PCSCD_PID" 2>/dev/null; then
            break # pcscd died; no point waiting out the timeout
        fi
        if grep -qiE "address in use|Open Port .* Failed" /tmp/pcscd.log 2>/dev/null; then
            break # the driver could not bind — the port answering is somebody else
        fi
        if (exec 3<>"/dev/tcp/$VPCD_HOST/$VPCD_PORT") 2>/dev/null; then
            exec 3<&- 3>&-
            VPCD_READY=1
            break
        fi
        sleep 0.5
    done
    if [ "$VPCD_READY" -ne 1 ]; then
        log "FATAL: pcscd's vpcd reader never came up on $VPCD_HOST:$VPCD_PORT (see [pcscd] lines above)."
        log "       If pcscd logged 'Address in use', another process holds $VPCD_PORT — pick a"
        log "       [vowifi].vpcd_port below the ephemeral range (cat /proc/sys/net/ipv4/ip_local_port_range)."
        exit 1
    fi
    log "vpcd reader ready on $VPCD_HOST:$VPCD_PORT"
fi

# --- Start every resolved line ------------------------------------------------
for i in $(seq 0 $((LINE_COUNT - 1))); do
    if [ "$TUNNEL_ENGINE" = "strongswan" ]; then
        start_line_strongswan "$i" || log "line $i: failed to start (see FATAL above) — continuing with the remaining lines"
    else
        start_line_swu "$i" || log "line $i: failed to start (see FATAL above) — continuing with the remaining lines"
    fi
done

# --- Agent B: one shared process for every line's veth pair -----------------
# Presents a single SIP identity/registration to the PBX (the spec's own
# Assumptions section) — reads the same `discover` line-resolution file
# this script just populated to learn how many control-channel listeners to
# start, one per line, each tagging its traffic with that line's card id.
log "starting vowifi-sip-agent (default netns, one shared process for all lines), supervised..."
(
    while true; do
        "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" vowifi-sip-agent
        log "vowifi-sip-agent exited (status $?); restarting in 5s"
        sleep 5
    done
) &
SIP_AGENT_SUPERVISOR_PID=$!

else
    log "[vowifi].enabled is not true in $GSM_SIP_BRIDGE_CONFIG — VoWiFi bridge not started"
fi

# --- Host-side IMS over LTE (specs/015-volte-host-ims) ----------------------
# Opt-in via [volte].enabled. Mutually exclusive with VoWiFi on the same SIM:
# both register the same IMPU with the same IMEI-derived +sip.instance, so the
# network treats one as a re-registration of the other and tears the first
# binding down. `volte-register` enforces this itself (it refuses while a
# vowifi-ims-agent is running), but refusing here too keeps the container from
# spawning a supervisor that could only ever fail.
if [ "$VOLTE_ENABLED" -eq 1 ]; then
    # config already fetched (`$VOLTE_ENV`), mutual-exclusion against VoWiFi
    # already checked, and — for the auto-discovered multi-line case — the
    # line table already resolved and persisted (`$VOLTE_LINE_*`), all up in
    # the "discover once, up front" block above, before the circuit-switched
    # daemon started (specs/020-volte-line-netns).

    # --- specs/020-volte-line-netns: per-line namespace isolation ----------
    # Idempotently ensures line $1's namespace exists and its LTE interface
    # $2 is inside it — moved in, not created (research.md R5): the
    # interface already exists in the default namespace as soon as the
    # modem's kernel driver enumerates it, independent of PDN/attach state,
    # unlike VoWiFi's synthetic XFRM interface. Three-way check mirrors
    # `ensure_epdg_interface`: already in the target namespace (reuse) → in
    # the default namespace (move it) → neither (caller retries/skips).
    # An empty $2 (no host interface configured for this line) is a no-op —
    # the namespace still exists so the line's sockets are isolated even
    # though no netdev needs moving.
    ensure_volte_line_netns() {
        local netns="$1" iface="$2"
        if [ ! -e "/var/run/netns/$netns" ]; then
            ip netns add "$netns"
            log "volte line: created netns $netns"
        fi
        ip netns exec "$netns" ip link set lo up

        [ -z "$iface" ] && return 0

        if ip netns exec "$netns" ip link show "$iface" >/dev/null 2>&1; then
            return 0 # already in place — idempotent restart (FR-011)
        fi
        if ip link show "$iface" >/dev/null 2>&1; then
            ip link set "$iface" netns "$netns"
            log "volte line: moved $iface into netns $netns"
            return 0
        fi
        return 1 # not found in either namespace — caller retries/skips
    }

    # Idempotently creates line $1's veth pair: $2 (default namespace) <->
    # $3 (netns $5), with addresses $4/$6 — same shape as VoWiFi's
    # `start_line_tail` veth handling.
    ensure_volte_line_veth() {
        local veth_telephony="$1" veth_carrier="$2" netns="$3"
        local telephony_addr="$4" carrier_addr="$5"
        if ! ip link show "$veth_telephony" >/dev/null 2>&1; then
            ip link add "$veth_telephony" type veth peer name "$veth_carrier" netns "$netns"
        fi
        ip addr replace "$telephony_addr/30" dev "$veth_telephony"
        ip link set "$veth_telephony" up
        ip netns exec "$netns" ip addr replace "$carrier_addr/30" dev "$veth_carrier"
        ip netns exec "$netns" ip link set "$veth_carrier" up
    }

    # Brings up every auto-discovered VoLTE line in its own namespace, then
    # the one shared telephone-facing half (Agent B). Mirrors VoWiFi's
    # per-line loop followed by `vowifi-sip-agent`.
    start_volte_multiline() {
        log "[volte].enabled + bridge_inbound — answering inbound calls over LTE (auto-discovering modems, up to ${VOLTE_MAX_LINES:-8} line(s))"

        # `$VOLTE_LINE_*` arrays already come from the "discover once, up
        # front" block above — resolved and persisted before the
        # circuit-switched daemon's own scan started, not re-derived here.
        if [ "${VOLTE_LINE_COUNT:-0}" -eq 0 ]; then
            log "PROMINENT ERROR: [volte].enabled + bridge_inbound is true but no usable VoLTE \
line was discovered — the VoLTE subsystem will NOT start this run."
            return 0
        fi

        for i in $(seq 0 $((VOLTE_LINE_COUNT - 1))); do
            local card_id="${VOLTE_LINE_CARD_ID[i]}"
            local modem="${VOLTE_LINE_MODEM_PORT[i]}"
            local iface="${VOLTE_LINE_IFACE[i]}"
            local netns="${VOLTE_LINE_NETNS[i]}"
            local veth_carrier_iface="${VOLTE_LINE_VETH_CARRIER_IFACE[i]}"
            local veth_telephony_iface="${VOLTE_LINE_VETH_TELEPHONY_IFACE[i]}"
            local veth_carrier_addr="${VOLTE_LINE_VETH_CARRIER_ADDR[i]}"
            local veth_telephony_addr="${VOLTE_LINE_VETH_TELEPHONY_ADDR[i]}"

            log "volte line $i ($card_id): netns=$netns iface=$iface"
            # Must run before anything else touches this modem (research.md
            # of specs/020-volte-line-netns — the modem's own internal VoLTE
            # stack otherwise fights our registration for the same IMPU; see
            # the shared definition above for the incident this closes).
            reconcile_line_ims_mode "$i" "$modem" || continue
            if ! ensure_volte_line_netns "$netns" "$iface"; then
                log "volte line $i: FATAL: interface $iface not present in container; skipping this line"
                continue
            fi
            ensure_volte_line_veth "$veth_telephony_iface" "$veth_carrier_iface" "$netns" \
                "$veth_telephony_addr" "$veth_carrier_addr"

            STARTED_NETNS+=("$netns")
            VOLTE_STARTED_LINE_NETNS+=("$netns")
            VOLTE_STARTED_LINE_INDEX+=("$i")

            log "volte line $i: starting volte-carrier-agent (netns $netns), supervised..."
            (
                while true; do
                    ip netns exec "$netns" "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" \
                        volte-carrier-agent --line "$i"
                    log "volte line $i: volte-carrier-agent exited (status $?); restarting in 15s"
                    sleep 15
                done
            ) &
            VOLTE_CARRIER_AGENT_SUPERVISOR_PIDS+=("$!")
        done

        if [ "${#VOLTE_STARTED_LINE_NETNS[@]}" -eq 0 ]; then
            log "PROMINENT ERROR: every VoLTE line failed to start (see FATAL lines above) — the \
VoLTE subsystem will NOT start this run."
            return 0
        fi

        log "starting volte-bridge (default netns, one shared process for all VoLTE lines), supervised..."
        (
            while true; do
                "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" volte-bridge
                log "volte-bridge exited (status $?); restarting in 15s"
                sleep 15
            done
        ) &
        VOLTE_BRIDGE_SUPERVISOR_PID=$!
    }

    # [volte].bridge_inbound picks which of the two services runs
    # (specs/017-volte-inbound-bridge FR-023). Unset means today's behaviour:
    # hold the registration open and nothing more.
    #
    # A non-empty modem_port pins one modem (single-line, back-compat,
    # research.md R7 — no namespace, out of scope for isolation). An empty
    # modem_port auto-discovers every SIM-ready modem and bridges each as its
    # own line (specs/018-volte-multi-modem), now each in its own network
    # namespace (specs/020-volte-line-netns) — per-line cid/apn/pcscf/iface
    # then come from [volte] + [[volte.line]], not these flags.
    if [ "${VOLTE_BRIDGE_INBOUND:-0}" -eq 1 ] && [ -z "$VOLTE_MODEM_PORT" ]; then
        start_volte_multiline
    elif [ "${VOLTE_BRIDGE_INBOUND:-0}" -eq 1 ]; then
        log "[volte].enabled + bridge_inbound — answering inbound calls over LTE (modem $VOLTE_MODEM_PORT, cid $VOLTE_CID)"
        (
            while true; do
                # --pcscf omitted deliberately: resolved from the ePDG capture
                # at pcscf_source_path when no address is configured, so a
                # VoWiFi run on this SIM primes the LTE path.
                "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" volte-bridge \
                    --modem "$VOLTE_MODEM_PORT" \
                    ${VOLTE_IFACE:+--iface "$VOLTE_IFACE"} \
                    --cid "$VOLTE_CID" --apn "$VOLTE_APN" \
                    ${VOLTE_PCSCF:+--pcscf "$VOLTE_PCSCF"} \
                    --pcscf-port "$VOLTE_PCSCF_PORT" \
                    --pcscf-source-path "$VOLTE_PCSCF_SOURCE_PATH" \
                    --restore-cid-path "$VOLTE_RESTORE_CID_PATH"
                log "the LTE IMS service exited (status $?); restarting in 15s"
                sleep 15
            done
        ) &
        VOLTE_SUPERVISOR_PID=$!
    else
        log "[volte].enabled — starting host-side IMS over LTE (modem $VOLTE_MODEM_PORT, cid $VOLTE_CID)"
        (
            while true; do
                "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" volte-register \
                    --modem "$VOLTE_MODEM_PORT" \
                    ${VOLTE_IFACE:+--iface "$VOLTE_IFACE"} \
                    --cid "$VOLTE_CID" --apn "$VOLTE_APN" \
                    ${VOLTE_PCSCF:+--pcscf "$VOLTE_PCSCF"} \
                    --pcscf-port "$VOLTE_PCSCF_PORT" \
                    --pcscf-source-path "$VOLTE_PCSCF_SOURCE_PATH" \
                    --status-path "$VOLTE_STATUS_PATH" \
                    --lock-path "$VOLTE_LOCK_PATH" \
                    --restore-cid-path "$VOLTE_RESTORE_CID_PATH" \
                    --keep-pdn
                log "the LTE IMS service exited (status $?); restarting in 15s"
                # Longer than the 5s used elsewhere: a restart re-runs PDN
                # attachment and a full IMS-AKA exchange, so a tight loop
                # would hammer both the modem and the carrier's registrar.
                sleep 15
            done
        ) &
        VOLTE_SUPERVISOR_PID=$!
    fi
fi

# --- Block on everything -----------------------------------------------------
wait
