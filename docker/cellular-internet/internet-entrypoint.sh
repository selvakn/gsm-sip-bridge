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

# ---- dual-stack IPv6 (specs/035-dual-stack-ipv6) ---------------------------
# Dual-stack is ON by default. Modern carriers (verified on Jio) do dual-stack as a
# SINGLE IPv4v6 bearer, NOT two sessions: a second WDS session to the same APN is
# refused with 'multiple-connection-to-same-pdn-not-allowed', and QMI has no per-call
# "dual" ip-type (only 4 or 6). So when v6 is enabled the sidecar provisions the data
# profile as IPv4v6 and dials ONE bearer, reading BOTH address families from it.
# INTERNET_ENABLE_IPV6=0 forces a v4-only (ip-type=4) bearer, byte-identical to the
# pre-035 behaviour.
INTERNET_ENABLE_IPV6="${INTERNET_ENABLE_IPV6:-1}"
# 3GPP profile index the sidecar provisions as IPv4v6 and dials for dual-stack.
INTERNET_IPV6_PROFILE="${INTERNET_IPV6_PROFILE:-1}"
# Optional operator hook, invoked as `hook <new-global-v6-addr>` when the global
# IPv6 address first appears or changes. Empty = no hook.
INTERNET_IPV6_HOOK="${INTERNET_IPV6_HOOK:-}"
INTERNET_IPV6_HOOK_TIMEOUT="${INTERNET_IPV6_HOOK_TIMEOUT:-10s}"

PKT_HANDLE=""
WDS_CID=""
# 1 when the session was brought up by the modem itself (autoconnect) rather
# than by us: we hold no client for it, so we must not try to stop it.
ADOPTED=0

# Current applied global IPv6 address state. v6 rides the SAME single bearer as v4
# (same WDS_CID / PKT_HANDLE), so there is no separate v6 session identity to track —
# teardown of the one bearer drops both families.
V6_ADDR=""
V6_PREFIX=""
V6_SINCE=""
# The "last successfully notified" address lives in a marker file next to the status
# file (see notify_v6_hook), NOT an in-memory var: firing gates on it so a failed
# hook is retried on a later tick, and de-dupe survives a restart.

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

# ---- IPv6 dual-stack (specs/035-dual-stack-ipv6) ---------------------------
# v6 rides the SAME single bearer as v4 (one IPv4v6 PDN — see the config note on why
# separate sessions don't work). These helpers read the v6 address from the current
# session (via qmi_wds, the shared client) and apply it; they touch only V6_* vars +
# `ip -6`, so a v6 problem never disturbs v4 (FR-003/FR-004/FR-005).

# Read the status `state` so a v6-only status update preserves the IPv4-derived
# state: IPv6 must never change `state` or the healthcheck (FR-004/FR-007).
_current_state() {
    _cs=""
    [ -r "$INTERNET_STATUS_FILE" ] && _cs=$(sed -n 's/^state=//p' "$INTERNET_STATUS_FILE" 2>/dev/null | head -n1)
    if [ -n "$_cs" ]; then echo "$_cs"; else echo up; fi
}

# Provision the data profile as IPv4v6 so ONE bearer carries both families. QMI has
# no per-call "dual" ip-type (only 4 or 6) and a second session to the same APN is
# refused, so dual-stack must come from the profile's PDP type. Best-effort: a modem
# that rejects the modify still dials (it just won't get v6). Sets the APN on the
# profile too, since the dial then starts by profile-index.
ensure_dualstack_profile() {
    qmicli -d "$INTERNET_QMI_DEV" -p \
        --wds-modify-profile="3gpp,${INTERNET_IPV6_PROFILE},pdp-type=ipv4v6,apn=${INTERNET_APN}" \
        >/dev/null 2>&1 \
        || log "WARNING: could not set profile ${INTERNET_IPV6_PROFILE} to IPv4v6 — the carrier/modem may only grant IPv4"
}

# Un-disable IPv6 on the interface (raw_ip ifaces often come up with it off).
# Guarded so the test harness (no such sysctl node) is a no-op.
enable_v6_iface() {
    _ev_k="/proc/sys/net/ipv6/conf/$1/disable_ipv6"
    if [ -w "$_ev_k" ]; then
        echo 0 > "$_ev_k" 2>/dev/null || true
    fi
}

# Apply the granted global IPv6 address + default route from the CURRENT bearer's
# settings (the SAME session as v4, read via qmi_wds). Echoes "<addr> <prefix>"; the
# caller sets V6_ADDR/V6_PREFIX in the parent shell (this runs in a $() subshell).
# Returns nonzero (no side effects) when the bearer carries no global v6. Flushes
# prior global v6 first so a changed prefix leaves nothing stale.
apply_settings_v6() {
    _asv_iface="$1"
    _asv_settings=$(qmi_wds --wds-get-current-settings 2>/dev/null) || return 1

    _asv_ipp=$(echo "$_asv_settings" | sed -n 's/.*IPv6 address: *//p'         | head -n1 | tr -d ' \r')
    _asv_gw=$(echo "$_asv_settings"  | sed -n 's/.*IPv6 gateway address: *//p' | head -n1 | tr -d ' \r')

    # qmicli reports the v6 address WITH its prefix (addr/prefix), unlike v4.
    _asv_ip=${_asv_ipp%%/*}
    _asv_prefix=${_asv_ipp#*/}
    [ "$_asv_prefix" = "$_asv_ipp" ] && _asv_prefix=64   # no /prefix => assume 64

    is_global_v6 "$_asv_ip" || return 1

    ip -6 addr flush dev "$_asv_iface" scope global 2>/dev/null || true
    ip -6 addr add "${_asv_ip}/${_asv_prefix}" dev "$_asv_iface"
    if [ -n "$_asv_gw" ] && [ "$_asv_gw" != "::" ]; then
        ip -6 route replace default via "$_asv_gw" dev "$_asv_iface"
    else
        ip -6 route replace default dev "$_asv_iface"
    fi
    echo "$_asv_ip $_asv_prefix"
}

# Fire the operator hook when the global v6 address first appears or CHANGES. Never
# on unchanged, never on loss, never when unconfigured (FR-008). Backgrounded and
# time-bounded so a slow/broken hook cannot stall the supervise loop (FR-009).
#
# De-dupe gates on a MARKER FILE that records the address a hook run SUCCEEDED for,
# not an in-memory var: a hook that fails (e.g. the DDNS endpoint is briefly down)
# leaves the marker stale, so a later supervise tick retries it instead of stranding
# the record for this prefix's whole lifetime.
notify_v6_hook() {
    [ -n "$INTERNET_IPV6_HOOK" ] || return 0
    [ -n "$V6_ADDR" ] || return 0
    _nh_marker="${INTERNET_STATUS_FILE}.v6notified"
    _nh_done=""
    [ -r "$_nh_marker" ] && _nh_done=$(cat "$_nh_marker" 2>/dev/null)
    [ "$V6_ADDR" = "$_nh_done" ] && return 0
    if [ ! -x "$INTERNET_IPV6_HOOK" ]; then
        log "WARNING: INTERNET_IPV6_HOOK=$INTERNET_IPV6_HOOK is not executable — skipping notification"
        return 0
    fi
    log "notifying IPv6 hook of new address $V6_ADDR"
    _nh_to=$(to_seconds "$INTERNET_IPV6_HOOK_TIMEOUT")
    [ "$_nh_to" -ge 1 ] 2>/dev/null || _nh_to=10
    _nh_addr="$V6_ADDR"
    # Detached + bounded (FR-009). Record the address ONLY on hook success, so a
    # failed run leaves the marker stale and the next tick retries.
    ( timeout "$_nh_to" "$INTERNET_IPV6_HOOK" "$_nh_addr" >/dev/null 2>&1 &&
        printf '%s' "$_nh_addr" > "$_nh_marker" 2>/dev/null ) &
}

# Record a v6 up/change: on a CHANGED address, update state + status and log; always
# (re)try the hook (its own marker de-dupes). Writes status only on change, so a
# stable v6 does not churn the status file every probe tick.
_mark_v6_up() {
    _mu_addr="$1"
    if [ "$_mu_addr" != "$V6_ADDR" ]; then
        V6_SINCE=$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo unknown)
        V6_ADDR="$_mu_addr"
        STATUS_IPV6="$V6_ADDR" STATUS_IPV6_PREFIX="$V6_PREFIX" \
            STATUS_IPV6_STATE="up" STATUS_IPV6_SINCE="$V6_SINCE" \
            write_status "$(_current_state)"
        log "internet v6 up: ipv6=$V6_ADDR/$V6_PREFIX"
    fi
    notify_v6_hook
}

# Record v6 as unavailable. No-op (no status churn) when already down. Does NOT fire
# the hook (FR-008: never on loss).
_mark_v6_down() {
    [ -z "$V6_ADDR" ] && return 0
    V6_ADDR=""
    V6_PREFIX=""
    V6_SINCE=""
    STATUS_IPV6="" STATUS_IPV6_PREFIX="" STATUS_IPV6_STATE="unavailable" STATUS_IPV6_SINCE="" \
        write_status "$(_current_state)"
}

# Read the current bearer's global IPv6 and apply/refresh it. Best-effort: never
# touches v4, never exits. Called from dial() and every supervise tick — since v6
# rides the v4 bearer this is just a settings read (cheap), so there is no separate
# v6 dial to back off. Fires the hook on appear/change; clears v6 on loss.
refresh_v6() {
    _rv_iface="$1"
    [ "$INTERNET_ENABLE_IPV6" = "1" ] || return 0
    enable_v6_iface "$_rv_iface"
    if _rv_applied=$(apply_settings_v6 "$_rv_iface"); then
        V6_PREFIX=${_rv_applied##* }
        _mark_v6_up "${_rv_applied%% *}"
    else
        if [ -n "$V6_ADDR" ]; then
            # The bearer dropped v6 while v4 stays up — flush the now-stale global
            # address/route so nothing lingers, then clear the status.
            log "IPv6 address withdrawn from the bearer"
            ip -6 addr flush dev "$_rv_iface" scope global 2>/dev/null || true
            ip -6 route flush default dev "$_rv_iface" 2>/dev/null || true
        fi
        _mark_v6_down
    fi
    return 0
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

    # Dual-stack dials ONE IPv4v6 bearer via the profile we provision; v4-only dials
    # ip-type=4 exactly as before. Both start a single WDS session (PKT_HANDLE/CID).
    if [ "$INTERNET_ENABLE_IPV6" = "1" ]; then
        ensure_dualstack_profile
        _d_start="profile-index=${INTERNET_IPV6_PROFILE}"
    else
        _d_start="ip-type=4,apn=${INTERNET_APN}"
    fi

    ADOPTED=0
    if _d_out=$(qmicli -d "$INTERNET_QMI_DEV" -p \
        --wds-start-network="${_d_start}" \
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

    # Best-effort dual-stack: read+apply the v6 address the same bearer granted.
    # Failure never fails the dial — the supervise loop re-reads it each tick.
    # IPv4/VoWiFi is up regardless (FR-004/FR-005).
    refresh_v6 "$_d_iface"
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

    # v6 rides the same bearer, so stopping the session below drops it; just clear
    # the host-side v6 config and status. Guarded by the kill-switch so a v4-only
    # deployment makes no ip -6 change (FR-011/SC-007).
    if [ "$INTERNET_ENABLE_IPV6" = "1" ]; then
        ip -6 addr flush dev "$_t_iface" scope global 2>/dev/null || true
        ip -6 route flush default dev "$_t_iface" 2>/dev/null || true
        _mark_v6_down
    fi

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

    # Supervise: probe on an interval; self-heal on loss (FR-007). IPv6 is a
    # best-effort SECONDARY concern refreshed AFTER the v4 logic (a cheap settings
    # read on the same bearer) — it never blocks health, redial, or the bridge
    # (FR-004/FR-005).
    while true; do
        sleep "$_m_interval"
        if probe_dns; then
            STATUS_IFACE="$_m_iface" write_status up "ok ${INTERNET_PROBE_HOST}@${INTERNET_PROBE_RESOLVER:-system}"
            refresh_v6 "$_m_iface"
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
