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
    log "dialing APN '$INTERNET_APN' over $INTERNET_QMI_DEV (iface $_d_iface)"
    STATUS_IFACE="$_d_iface" write_status dialing "pending"

    setup_raw_ip "$_d_iface"

    _d_out=$(qmicli -d "$INTERNET_QMI_DEV" -p \
        --wds-start-network="ip-type=4,apn=${INTERNET_APN}" \
        --client-no-release-cid 2>/dev/null) || {
        log "wds-start-network failed"
        return 1
    }
    # Keep BOTH the packet-data handle and the allocated WDS client id — the
    # latter is what all subsequent WDS actions must target (see qmi_wds).
    PKT_HANDLE=$(printf '%s\n' "$_d_out" | grep -iE 'packet data handle' | grep -oE '[0-9]+' | head -n1)
    WDS_CID=$(printf '%s\n' "$_d_out" | grep -iE 'CID:' | grep -oE '[0-9]+' | head -n1)
    log "session: handle=${PKT_HANDLE:-?} cid=${WDS_CID:-?}"
    if [ -z "$WDS_CID" ] || [ -z "$PKT_HANDLE" ]; then
        # A started session we cannot address is a client we can never release.
        # Say so loudly rather than silently skipping teardown later.
        log "WARNING: could not parse the WDS client id/handle from qmicli output; this session cannot be torn down cleanly"
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

teardown() {
    _t_iface="$1"
    log "tearing down data session on $_t_iface (handle=${PKT_HANDLE:-?} cid=${WDS_CID:-?})"
    # Stop on the SAME client that started the session, and let it be released
    # (no --client-no-release-cid here) so redials do not leak WDS clients.
    if [ -n "$WDS_CID" ] && [ -n "$PKT_HANDLE" ]; then
        qmicli -d "$INTERNET_QMI_DEV" -p --client-cid="$WDS_CID" \
            --wds-stop-network="$PKT_HANDLE" >/dev/null 2>&1 || true
    fi
    PKT_HANDLE=""
    WDS_CID=""
    ip addr flush dev "$_t_iface" 2>/dev/null || true
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

main "$@"
