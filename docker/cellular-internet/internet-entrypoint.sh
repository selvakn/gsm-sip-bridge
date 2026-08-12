#!/usr/bin/env sh
# Entrypoint for the cellular-internet sidecar
# (specs/032-cellular-internet-sidecar). Brings up internet over the modem's
# QMI data path and keeps it up, self-healing on drops. QMI only — it never
# opens the modem's AT port (the bridge needs it for AT+CSIM, FR-002).
#
# Lifecycle (data-model.md): dialing -> up -> {probe-fail,down} -> redialing -> up
set -u

# INTERNET_LIB defaults to the in-image path; overridable for the test harness.
INTERNET_LIB="${INTERNET_LIB:-/usr/local/bin/internet-lib.sh}"
# shellcheck source=docker/cellular-internet/internet-lib.sh
. "$INTERNET_LIB"

# ---- configuration (contracts/sidecar-config.md) ---------------------------
INTERNET_APN="${INTERNET_APN:-}"
INTERNET_QMI_DEV="${INTERNET_QMI_DEV:-/dev/cdc-wdm0}"
INTERNET_WWAN_IFACE="${INTERNET_WWAN_IFACE:-}"
INTERNET_PROBE_INTERVAL="${INTERNET_PROBE_INTERVAL:-10s}"

PKT_HANDLE=""
WDS_CID=""
# 1 when the session was brought up by the modem itself (autoconnect) rather
# than by us: we hold no client for it, so we must not try to stop it.
ADOPTED=0

# Is the modem's packet data session already up? Match "connected" as a whole
# word: a plain substring match also fires on "Connection status: 'disconnected'"
# and would report a torn-down session as still up.
session_connected() {
    qmicli -d "$INTERNET_QMI_DEV" -p --wds-get-packet-service-status 2>/dev/null \
        | grep -qiw "connected"
}

# Can we open a QMI channel to the modem at all? This distinguishes a wedged
# control endpoint — the node is present but every open returns "endpoint
# hangup", seen after a firmware stall or a silent USB re-enumeration — from a
# modem that is merely idle. A cheap version-info query forces an actual open +
# QMI transaction; if that can't get through, no WDS action can either, so a
# failed teardown there is a dead channel, not a stuck session we could stop.
qmi_reachable() {
    [ -e "$INTERNET_QMI_DEV" ] || return 1
    qmicli -d "$INTERNET_QMI_DEV" -p --get-service-version-info >/dev/null 2>&1
}

# Restart the libqmi proxy so the next `qmicli -p` re-opens the current device.
# qmi-proxy (spawned by our `-p`) caches its connection to the cdc-wdm node; when
# the endpoint hangs up or the modem silently re-enumerates, the proxy keeps
# serving the DEAD fd, so every later `-p` command fails with "endpoint hangup"
# even after the modem itself recovers (observed after a USB reset: a direct
# qmicli worked, `-p` did not, until the proxy was killed). Recycling it is the
# self-heal that otherwise required a manual container restart. Safe here: this
# sidecar is the only QMI user in its container — the bridge drives the modem
# over AT, never QMI.
recover_qmi_proxy() {
    _rq_pids=$(pidof qmi-proxy 2>/dev/null) || _rq_pids=""
    [ -n "$_rq_pids" ] || return 0
    log "recycling stale qmi-proxy (pids: $_rq_pids) so a fresh one re-opens $INTERNET_QMI_DEV"
    # Word-splitting is intended here: signal every listed pid.
    # shellcheck disable=SC2086
    kill $_rq_pids 2>/dev/null || true
}

# Run a qmicli WDS action, reusing the persistent client (WDS_CID) allocated by
# --wds-start-network when we have one. --wds-start-network allocates a client
# whose id is NOT guaranteed to be 1, so every later action (get-settings,
# stop-network) MUST target that same client or it queries/tears down the wrong
# one and leaks the real session across redials.
qmi_wds() {
    if [ -n "$WDS_CID" ]; then
        qmicli -d "$INTERNET_QMI_DEV" -p --client-cid="$WDS_CID" --client-no-release-cid "$@"
    else
        qmicli -d "$INTERNET_QMI_DEV" -p "$@"
    fi
}

# Strip a trailing unit and return whole seconds (e.g. "10s" -> 10, "2m" -> 120).
to_seconds() {
    _ts_v="$1"
    case "$_ts_v" in
        *m) echo $(( ${_ts_v%m} * 60 )) ;;
        *s) echo "${_ts_v%s}" ;;
        *)  echo "$_ts_v" ;;
    esac
}

# 255.255.255.0 -> 24
mask2prefix() {
    _mp_p=0
    _IFS_SAVE="$IFS"; IFS='.'
    for _mp_o in $1; do
        while [ "$_mp_o" -gt 0 ]; do
            _mp_p=$(( _mp_p + (_mp_o & 1) ))
            _mp_o=$(( _mp_o >> 1 ))
        done
    done
    IFS="$_IFS_SAVE"
    echo "$_mp_p"
}

validate_config() {
    if [ -z "$INTERNET_APN" ]; then
        log "FATAL: INTERNET_APN is required (the carrier's INTERNET apn — not IMS). Refusing to dial a guessed default."
        exit 1
    fi
    if [ ! -e "$INTERNET_QMI_DEV" ]; then
        log "FATAL: INTERNET_QMI_DEV=$INTERNET_QMI_DEV not present. This sidecar drives internet over QMI only; a non-QMI (e.g. UNISOC/EC200U) modem is out of scope. Never falling back to the AT port."
        exit 1
    fi
    case "$INTERNET_QMI_DEV" in
        *cdc-wdm*) : ;;
        *) log "FATAL: INTERNET_QMI_DEV=$INTERNET_QMI_DEV does not look like a QMI (cdc-wdm) node." ; exit 1 ;;
    esac
    if ! command -v qmicli >/dev/null 2>&1; then
        log "FATAL: qmicli not found in image (build problem)."
        exit 1
    fi
}

# Discover the wwan netdev bound to the QMI control device, unless pinned.
detect_iface() {
    if [ -n "$INTERNET_WWAN_IFACE" ]; then
        echo "$INTERNET_WWAN_IFACE"
        return 0
    fi
    _di_wdm="${INTERNET_QMI_DEV##*/}"           # cdc-wdm0
    _di_net="/sys/class/usbmisc/${_di_wdm}/device/net"
    if [ -d "$_di_net" ]; then
        for _di_i in "$_di_net"/*; do
            [ -e "$_di_i" ] || continue
            basename "$_di_i"
            return 0
        done
    fi
    # Fallback: first wwan* interface.
    for _di_i in /sys/class/net/wwan*; do
        [ -e "$_di_i" ] || continue
        basename "$_di_i"
        return 0
    done
    return 1
}

setup_raw_ip() {
    _sri_iface="$1"
    ip link set "$_sri_iface" down 2>/dev/null || true
    if [ -w "/sys/class/net/${_sri_iface}/qmi/raw_ip" ]; then
        echo Y > "/sys/class/net/${_sri_iface}/qmi/raw_ip" 2>/dev/null || true
    fi
    ip link set "$_sri_iface" up
}

# Configure the interface from QMI's granted settings. Echoes the assigned IPv4.
apply_settings() {
    _as_iface="$1"
    _as_settings=$(qmi_wds --wds-get-current-settings 2>/dev/null) || return 1

    _as_ip=$(echo "$_as_settings"   | sed -n 's/.*IPv4 address: *//p'         | head -n1)
    _as_gw=$(echo "$_as_settings"   | sed -n 's/.*IPv4 gateway address: *//p' | head -n1)
    _as_mask=$(echo "$_as_settings" | sed -n 's/.*IPv4 subnet mask: *//p'     | head -n1)
    _as_dns=$(echo "$_as_settings"  | sed -n 's/.*IPv4 primary DNS: *//p'     | head -n1)

    [ -n "$_as_ip" ] || return 1
    _as_prefix=24
    [ -n "$_as_mask" ] && _as_prefix=$(mask2prefix "$_as_mask")

    ip addr flush dev "$_as_iface" 2>/dev/null || true
    ip addr add "${_as_ip}/${_as_prefix}" dev "$_as_iface"
    if [ -n "$_as_gw" ]; then
        ip route replace default via "$_as_gw" dev "$_as_iface"
    else
        ip route replace default dev "$_as_iface"
    fi
    if [ -n "$_as_dns" ]; then
        echo "nameserver $_as_dns" > /etc/resolv.conf 2>/dev/null || true
    fi
    echo "$_as_ip"
}

dial() {
    _d_iface="$1"

    # A retained identity means an earlier teardown could not stop that session.
    # Retry it before starting another, so we never stack a second retained
    # client on top of an unreleased one.
    if [ -n "$WDS_CID" ] || [ -n "$PKT_HANDLE" ]; then
        teardown "$_d_iface" || {
            log "previous session still not released — not starting another"
            return 1
        }
    fi

    log "dialing APN '$INTERNET_APN' over $INTERNET_QMI_DEV (iface $_d_iface)"
    STATUS_IFACE="$_d_iface" write_status dialing "pending"

    setup_raw_ip "$_d_iface"

    ADOPTED=0
    if _d_out=$(qmicli -d "$INTERNET_QMI_DEV" -p \
        --wds-start-network="ip-type=4,apn=${INTERNET_APN}" \
        --client-no-release-cid 2>&1); then

        # We started it: keep BOTH the packet-data handle and the allocated WDS
        # client id — the latter is what every later WDS action must target.
        PKT_HANDLE=$(printf '%s\n' "$_d_out" | grep -iE 'packet data handle' | grep -oE '[0-9]+' | head -n1)
        WDS_CID=$(printf '%s\n' "$_d_out" | grep -iE 'CID:' | grep -oE '[0-9]+' | head -n1)
        log "session: handle=${PKT_HANDLE:-?} cid=${WDS_CID:-?}"
        if [ -z "$WDS_CID" ] || [ -z "$PKT_HANDLE" ]; then
            # We started a session but can't fully address it, so teardown could
            # never stop it: it would take the [ -z CID/handle ] shortcut and
            # clear the identity while the client stays retained. Every redial
            # would then stack a fresh client on the modem until it refuses new
            # ones and the sidecar is stuck offline. Release whatever we can and
            # fail this dial rather than proceed with an unstoppable session.
            log "WARNING: could not parse the WDS client id/handle (handle=${PKT_HANDLE:-?} cid=${WDS_CID:-?}) from qmicli output; releasing what we can and re-dialing"
            if [ -n "$WDS_CID" ]; then
                # A WDS action WITHOUT --client-no-release-cid frees the client.
                # (Without a CID we can't target it at all; the next start will
                # return NoEffect and we adopt the still-up session instead.)
                qmicli -d "$INTERNET_QMI_DEV" -p --client-cid="$WDS_CID" \
                    --wds-get-packet-service-status >/dev/null 2>&1 || true
            fi
            PKT_HANDLE=""
            WDS_CID=""
            return 1
        fi
    else
        # A *failed* start can still have retained a client id — release it, or
        # every retry leaks one until the modem can allocate no more.
        _d_stray=$(printf '%s\n' "$_d_out" | grep -iE 'CID:' | grep -oE '[0-9]+' | head -n1)
        if [ -n "$_d_stray" ]; then
            log "releasing stray WDS client $_d_stray left by the failed start"
            qmicli -d "$INTERNET_QMI_DEV" -p --client-cid="$_d_stray" \
                --wds-get-packet-service-status >/dev/null 2>&1 || true
        fi

        # 'NoEffect' means the network is already started — modems ship with
        # autoconnect enabled and bring a session up by themselves. That is not
        # an error: adopt the existing session rather than fighting it.
        if printf '%s' "$_d_out" | grep -qi 'noeffect' || session_connected; then
            log "a data session is already connected (modem autoconnect) — adopting it"
            ADOPTED=1
            PKT_HANDLE=""
            WDS_CID=""
        else
            log "wds-start-network failed: $(printf '%s' "$_d_out" | grep -i 'error' | head -n1)"
            return 1
        fi
    fi

    _d_ip=$(apply_settings "$_d_iface") || {
        # The session IS started at this point, so returning straight to the
        # retry loop would strand the retained client: the next dial overwrites
        # CID/handle and the modem eventually refuses to allocate any more.
        # Release it before giving up.
        log "could not read/apply QMI settings — releasing the started session"
        teardown "$_d_iface"
        return 1
    }

    STATUS_IFACE="$_d_iface" STATUS_IPV4="$_d_ip" STATUS_SINCE="$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo unknown)" \
        write_status up "pending"
    log "internet up: iface=$_d_iface ipv4=$_d_ip"
    return 0
}

# Stop the data session and release the retained WDS client.
#
# The identity (CID + handle) is only cleared once we know the client is gone —
# either because the stop succeeded, or because the modem itself went away and
# took all of its clients with it. Clearing it after a *failed* stop would
# strand that client: the next dial would allocate a second one on top of an
# unreleased first, and repeated recovery failures would exhaust the modem's
# WDS clients and leave the sidecar permanently offline.
teardown() {
    _t_iface="$1"
    ip addr flush dev "$_t_iface" 2>/dev/null || true

    # An adopted (autoconnect) session is not ours to stop — we hold no client
    # for it, and stopping it would fight the modem, which would just bring it
    # back. Drop our host-side config and leave the carrier session alone.
    if [ "$ADOPTED" -eq 1 ]; then
        ADOPTED=0
        PKT_HANDLE=""
        WDS_CID=""
        return 0
    fi

    if [ -z "$WDS_CID" ] || [ -z "$PKT_HANDLE" ]; then
        PKT_HANDLE=""
        WDS_CID=""
        return 0
    fi

    # A vanished QMI device means the modem reset/re-enumerated; every client it
    # held died with it, so the identity is meaningless rather than leaked.
    if [ ! -e "$INTERNET_QMI_DEV" ]; then
        log "modem gone — session (handle=$PKT_HANDLE cid=$WDS_CID) died with it"
        PKT_HANDLE=""
        WDS_CID=""
        return 0
    fi

    log "tearing down data session on $_t_iface (handle=$PKT_HANDLE cid=$WDS_CID)"
    _t_try=1
    while [ "$_t_try" -le 3 ]; do
        # Stop on the SAME client that started the session, and let qmicli
        # release it on exit (no --client-no-release-cid here).
        if qmicli -d "$INTERNET_QMI_DEV" -p --client-cid="$WDS_CID" \
            --wds-stop-network="$PKT_HANDLE" >/dev/null 2>&1; then
            PKT_HANDLE=""
            WDS_CID=""
            return 0
        fi
        _t_try=$(( _t_try + 1 ))
        [ "$_t_try" -le 3 ] && sleep 1
    done

    # Every stop attempt failed. Retaining the identity blocks ALL future dials
    # (dial() refuses to start a new session while one is retained), so before we
    # wedge the sidecar that way, work out WHY the stop failed — only a session
    # that is genuinely still ours warrants holding on:
    #
    #   * QMI unreachable — the control endpoint hung up, or the modem silently
    #     re-enumerated. No qmicli action can get through, so this is not a stuck
    #     session we can stop; the channel needs a reset. Recycle the (possibly
    #     stale) proxy and drop the identity so the redial loop resumes instead
    #     of spinning forever on a stop that can never open the device.
    #   * session already gone — the carrier tore it down and our handle is
    #     stale. There is nothing left to stop; drop the identity and let dial()
    #     start a fresh session.
    if ! qmi_reachable; then
        log "WARNING: QMI control endpoint unreachable while stopping (handle=$PKT_HANDLE cid=$WDS_CID) — modem wedged or re-enumerated; recycling the proxy and dropping the stale identity"
        recover_qmi_proxy
        PKT_HANDLE=""
        WDS_CID=""
        return 0
    fi
    if ! session_connected; then
        log "data session already ended (handle=$PKT_HANDLE cid=$WDS_CID) — dropping the stale identity"
        PKT_HANDLE=""
        WDS_CID=""
        return 0
    fi

    # Reachable AND still connected but the stop keeps failing — genuinely ours
    # and stuck. Keep the identity so the next teardown retries this client
    # instead of stacking another one behind it.
    log "WARNING: could not stop WDS session (handle=$PKT_HANDLE cid=$WDS_CID) after 3 attempts; retaining its identity to retry"
    return 1
}

main() {
    validate_config

    _m_iface=$(detect_iface) || {
        log "FATAL: could not find a wwan netdev for $INTERNET_QMI_DEV"
        write_status down "no-iface"
        exit 1
    }

    _m_interval=$(to_seconds "$INTERNET_PROBE_INTERVAL")
    [ "$_m_interval" -ge 1 ] 2>/dev/null || _m_interval=10

    trap 'teardown "$_m_iface"; exit 0' TERM INT

    # Initial dial with a short retry so a slow modem attach doesn't wedge us.
    until dial "$_m_iface"; do
        write_status redialing "dial-retry"
        sleep 5
    done

    # Supervise: probe on an interval; self-heal on loss (FR-007).
    while true; do
        sleep "$_m_interval"
        if probe_dns; then
            STATUS_IFACE="$_m_iface" write_status up "ok ${INTERNET_PROBE_HOST}@${INTERNET_PROBE_RESOLVER:-system}"
            continue
        fi
        log "reachability lost — re-dialing"
        write_status down "probe-fail"
        teardown "$_m_iface"
        # If the modem re-enumerated, wait for the QMI node to reappear.
        while [ ! -e "$INTERNET_QMI_DEV" ]; do
            write_status redialing "await-modem"
            sleep 5
        done
        until dial "$_m_iface"; do
            write_status redialing "dial-retry"
            sleep 5
        done
    done
}

# Test seam: sourcing this with INTERNET_NO_MAIN=1 loads the functions without
# starting the supervise loop, so the WDS session lifecycle (dial/teardown) can
# be exercised against a fake qmicli. POSIX sh has no __main__ idiom.
[ "${INTERNET_NO_MAIN:-}" = "1" ] || main "$@"
