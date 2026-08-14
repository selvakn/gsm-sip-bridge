#!/usr/bin/env sh
# Shared helpers for the cellular-internet sidecar
# (specs/032-cellular-internet-sidecar). POSIX sh; sourced by
# internet-entrypoint.sh and internet-healthcheck.sh.
#
# Deliberately dependency-light: `nslookup` (busybox or bind), `getent`,
# `ip`, `qmicli`. No bashisms so it runs under Alpine's /bin/sh (busybox ash).

# Where the human-readable status lives (sidecar-local only — FR-008).
INTERNET_STATUS_FILE="${INTERNET_STATUS_FILE:-/run/internet-status}"

# Probe configuration (data-model.md). Empty resolver => use the system
# resolver (getent), which is also what makes the probe testable offline.
# Note the resolver default uses `-` not `:-`: an *unset* resolver falls back
# to 1.1.1.1, but an explicitly-empty one is honoured as "system resolver"
# (the test harness relies on this to stay hermetic — no network, no nslookup).
INTERNET_PROBE_HOST="${INTERNET_PROBE_HOST:-one.one.one.one}"
INTERNET_PROBE_RESOLVER="${INTERNET_PROBE_RESOLVER-1.1.1.1}"

log() { echo "[internet] $*"; }

# is_global_v6 ADDR
# Return 0 only for a global unicast IPv6 address — the kind that provides inbound
# reach-back (specs/035). Reject the unspecified/loopback addresses, link-local
# (fe80::/10), ULA (fc00::/7), and multicast (ff00::/8). A non-global address is
# never written to status as the reach-back address, and never fires the hook.
is_global_v6() {
    _ig_a=$(printf '%s' "${1:-}" | tr 'A-F' 'a-f')
    case "$_ig_a" in
        '' | :: | ::1) return 1 ;;                 # empty / unspecified / loopback
        fe8* | fe9* | fea* | feb*) return 1 ;;     # fe80::/10 link-local
        fc* | fd*) return 1 ;;                      # fc00::/7 unique-local
        ff*) return 1 ;;                            # ff00::/8 multicast
    esac
    # Anything else that still looks like an IPv6 literal is treated as global.
    case "$_ig_a" in
        *:*) return 0 ;;
        *) return 1 ;;
    esac
}

# write_status STATE [PROBE_RESULT]
# Atomically (temp + mv) rewrite the status file. Merges over prior values so a
# writer that only knows the new state/probe preserves iface/ipv4/since.
write_status() {
    _st_state="$1"
    _st_probe="${2:-}"

    _st_iface=""
    _st_ipv4=""
    _st_since=""
    _st_ipv6=""
    _st_ipv6_prefix=""
    _st_ipv6_state=""
    _st_ipv6_since=""
    if [ -r "$INTERNET_STATUS_FILE" ]; then
        _st_iface=$(sed -n 's/^iface=//p' "$INTERNET_STATUS_FILE" 2>/dev/null)
        _st_ipv4=$(sed -n 's/^ipv4=//p' "$INTERNET_STATUS_FILE" 2>/dev/null)
        _st_since=$(sed -n 's/^since=//p' "$INTERNET_STATUS_FILE" 2>/dev/null)
        [ -z "$_st_probe" ] && _st_probe=$(sed -n 's/^probe=//p' "$INTERNET_STATUS_FILE" 2>/dev/null)
        _st_ipv6=$(sed -n 's/^ipv6=//p' "$INTERNET_STATUS_FILE" 2>/dev/null)
        _st_ipv6_prefix=$(sed -n 's/^ipv6_prefix=//p' "$INTERNET_STATUS_FILE" 2>/dev/null)
        _st_ipv6_state=$(sed -n 's/^ipv6_state=//p' "$INTERNET_STATUS_FILE" 2>/dev/null)
        _st_ipv6_since=$(sed -n 's/^ipv6_since=//p' "$INTERNET_STATUS_FILE" 2>/dev/null)
    fi
    # Allow callers to export overrides for the fuller fields.
    [ -n "${STATUS_IFACE:-}" ] && _st_iface="$STATUS_IFACE"
    [ -n "${STATUS_IPV4:-}" ] && _st_ipv4="$STATUS_IPV4"
    [ -n "${STATUS_SINCE:-}" ] && _st_since="$STATUS_SINCE"
    # v6 overrides use set-or-unset (${VAR+x}) semantics, NOT non-empty: an
    # explicitly-empty override CLEARS the field (v6 lost), while an unset
    # override preserves the prior value. `state` is derived from IPv4 only —
    # ipv6_state is informational and never influences it (FR-004/FR-007).
    [ -n "${STATUS_IPV6+x}" ] && _st_ipv6="$STATUS_IPV6"
    [ -n "${STATUS_IPV6_PREFIX+x}" ] && _st_ipv6_prefix="$STATUS_IPV6_PREFIX"
    [ -n "${STATUS_IPV6_STATE+x}" ] && _st_ipv6_state="$STATUS_IPV6_STATE"
    [ -n "${STATUS_IPV6_SINCE+x}" ] && _st_ipv6_since="$STATUS_IPV6_SINCE"
    # A file that never carried v6 state reads as unavailable.
    [ -z "$_st_ipv6_state" ] && _st_ipv6_state="unavailable"

    _st_now=$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo unknown)
    _st_tmp="${INTERNET_STATUS_FILE}.tmp.$$"
    {
        echo "state=${_st_state}"
        echo "iface=${_st_iface}"
        echo "ipv4=${_st_ipv4}"
        echo "probe=${_st_probe}"
        echo "since=${_st_since}"
        echo "last_change=${_st_now}"
        echo "ipv6=${_st_ipv6}"
        echo "ipv6_prefix=${_st_ipv6_prefix}"
        echo "ipv6_state=${_st_ipv6_state}"
        echo "ipv6_since=${_st_ipv6_since}"
    } > "$_st_tmp" 2>/dev/null || return 0
    mv -f "$_st_tmp" "$INTERNET_STATUS_FILE" 2>/dev/null || rm -f "$_st_tmp"
}

# probe_dns [HOST] [RESOLVER]
# Resolve HOST (via RESOLVER when set, else the system resolver) through the
# current default route. Returns 0 when an address is resolved, nonzero
# otherwise. Bounded by `timeout` so a black-holed link fails fast.
# True once the entrypoint has completed a dial (it records the address the
# carrier assigned). The healthcheck gates on this so it cannot report healthy
# off some *other* uplink before this sidecar's cellular session exists — FR-003
# requires reachability *through the cellular link*, not merely that the host
# happens to have internet from somewhere.
session_established() {
    [ -r "$INTERNET_STATUS_FILE" ] || return 1
    _se_ip=$(sed -n 's/^ipv4=//p' "$INTERNET_STATUS_FILE" 2>/dev/null)
    [ -n "$_se_ip" ]
}

# Configured entirely through the environment (INTERNET_PROBE_HOST /
# INTERNET_PROBE_RESOLVER) rather than positional parameters: every caller uses
# the configured values, and taking no arguments keeps this free of shellcheck
# SC2119 across shellcheck versions.
probe_dns() {
    _pd_host="$INTERNET_PROBE_HOST"
    _pd_resolver="$INTERNET_PROBE_RESOLVER"

    if [ -n "$_pd_resolver" ]; then
        # DNS query to the configured resolver (survives ICMP blocking; proves
        # routing + name resolution — research R3).
        _pd_out=$(timeout 4 nslookup "$_pd_host" "$_pd_resolver" 2>/dev/null) || return 1
    else
        # Offline-capable fallback: system resolver via NSS (files + dns).
        if getent hosts "$_pd_host" >/dev/null 2>&1; then
            return 0
        fi
        _pd_out=$(timeout 4 nslookup "$_pd_host" 2>/dev/null) || return 1
    fi
    # Guard against resolvers that exit 0 but answer nothing.
    echo "$_pd_out" | grep -qiE 'address|has address|^Name:' || return 1
    return 0
}
