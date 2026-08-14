#!/usr/bin/env sh
# Integration test for the sidecar's dual-stack IPv6 lifecycle
# (specs/035-dual-stack-ipv6).
#
# Model: v6 rides the SAME single IPv4v6 bearer as v4 (one PDN) — hardware-verified
# on Jio, where a separate ip-type=6 session to the same APN is refused. Exercises
# the REAL functions from internet-entrypoint.sh (dial, refresh_v6, apply_settings_v6,
# teardown, _mark_v6_*). Only the modem is faked: `qmicli`/`ip` are scripted stubs on
# PATH — the constitution's "hardware not available in CI" exception. The logic under
# test — single-bearer dual-stack, global-v6 detection, and the rule that v6 never
# disturbs the v4 session or `state` — is the real thing.
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
DIR=$(dirname "$HERE")

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
export PATH="$TMP/bin:$PATH"
mkdir -p "$TMP/bin"

QMI_DEV="$TMP/cdc-wdm0"; : > "$QMI_DEV"
IP_LOG="$TMP/ip_log"; : > "$IP_LOG"
V6_HAS_SETTINGS="$TMP/v6_has_settings"; echo 1 > "$V6_HAS_SETTINGS"  # 1 => bearer carries v6
V6ADDR_FILE="$TMP/v6addr"                                            # current granted v6 (settable)
MODIFY_LOG="$TMP/modify_log"; : > "$MODIFY_LOG"                      # records modify-profile calls
START_LOG="$TMP/start_log"; : > "$START_LOG"                        # records start-network args
V6ADDR="2409:4072:99:1a6a:48bc:a7c9:296b:a03f"
echo "$V6ADDR" > "$V6ADDR_FILE"

cat > "$TMP/bin/qmicli" <<EOF
#!/usr/bin/env sh
for a in "\$@"; do
    case "\$a" in
        --wds-modify-profile=*) echo "\$a" >> $MODIFY_LOG; exit 0 ;;
        --wds-start-network=*)
            echo "\$a" >> $START_LOG
            printf '[dev] Network started\n\tPacket data handle: %s\n' "'2264216040'"
            printf '[dev] Client ID not released:\n\tService: %s\n\t    CID: %s\n' "'wds'" "'7'"
            exit 0 ;;
        --wds-stop-network=*) exit 0 ;;
        --wds-get-current-settings)
            printf '\tIPv4 address: 100.77.232.222\n\tIPv4 gateway address: 100.77.232.221\n\tIPv4 subnet mask: 255.255.255.252\n\tIPv4 primary DNS: 8.8.8.8\n'
            if [ "\$(cat $V6_HAS_SETTINGS)" = 1 ]; then
                printf '\tIPv6 address: %s/64\n\tIPv6 gateway address: 2409:4072:99:1a6a::1\n' "\$(cat $V6ADDR_FILE)"
            fi
            exit 0 ;;
        --wds-get-packet-service-status) printf "Connection status: 'connected'\n"; exit 0 ;;
        --get-service-version-info) exit 0 ;;
    esac
done
exit 0
EOF
cat > "$TMP/bin/ip" <<EOF
#!/usr/bin/env sh
echo "\$*" >> $IP_LOG
exit 0
EOF
cat > "$TMP/bin/pidof" <<'EOF'
#!/usr/bin/env sh
exit 1
EOF
chmod +x "$TMP/bin/qmicli" "$TMP/bin/ip" "$TMP/bin/pidof"

INTERNET_NO_MAIN=1; export INTERNET_NO_MAIN
INTERNET_LIB="$DIR/internet-lib.sh"; export INTERNET_LIB
INTERNET_STATUS_FILE="$TMP/status"; export INTERNET_STATUS_FILE
INTERNET_APN="jionet"; export INTERNET_APN
INTERNET_QMI_DEV="$QMI_DEV"; export INTERNET_QMI_DEV
INTERNET_ENABLE_IPV6=1; export INTERNET_ENABLE_IPV6
# shellcheck source=docker/cellular-internet/internet-entrypoint.sh
. "$DIR/internet-entrypoint.sh"

fail() { echo "FAIL: $*" >&2; exit 1; }
status_field() { sed -n "s/^$1=//p" "$INTERNET_STATUS_FILE" 2>/dev/null; }

# --- 1. dual-stack dial: ONE bearer, v4 + v6 from the same session -----------
echo 1 > "$V6_HAS_SETTINGS"; echo "$V6ADDR" > "$V6ADDR_FILE"
dial wwan0 >/dev/null 2>&1 || fail "dual-stack dial should succeed"
[ "$PKT_HANDLE" = "2264216040" ] || fail "v4 handle not captured (got '$PKT_HANDLE')"
[ "$WDS_CID" = "7" ] || fail "v4 cid not captured (got '$WDS_CID')"
grep -q 'pdp-type=ipv4v6' "$MODIFY_LOG" || fail "profile should have been provisioned IPv4v6"
grep -q 'profile-index=1' "$START_LOG" || fail "dual-stack should dial by profile-index, got: $(cat "$START_LOG")"
[ "$V6_ADDR" = "$V6ADDR" ] || fail "v6 address not applied (got '$V6_ADDR')"
[ "$V6_PREFIX" = "64" ] || fail "v6 prefix not parsed (got '$V6_PREFIX')"
grep -q -- "-6 addr add $V6ADDR/64 dev wwan0" "$IP_LOG" || fail "no 'ip -6 addr add' recorded"
grep -q -- "-6 route replace default via 2409:4072:99:1a6a::1 dev wwan0" "$IP_LOG" || fail "no 'ip -6 route' recorded"
[ "$(status_field ipv6)" = "$V6ADDR" ] || fail "status ipv6 not set"
[ "$(status_field ipv6_state)" = "up" ] || fail "status ipv6_state should be up"
[ "$(status_field ipv4)" = "100.77.232.222" ] || fail "status ipv4 not set"
[ "$(status_field state)" = "up" ] || fail "v4 state should be up"
echo "ok: dual-stack dials one IPv4v6 bearer and applies both families"

# --- 2. carrier grants no v6: v4 healthy, v6 unavailable, no ip -6 add --------
teardown wwan0 >/dev/null 2>&1
: > "$IP_LOG"; : > "$START_LOG"; echo 0 > "$V6_HAS_SETTINGS"
dial wwan0 >/dev/null 2>&1 || fail "dial should still succeed when v6 is ungranted"
[ "$PKT_HANDLE" = "2264216040" ] || fail "v4 must be up even without v6"
[ -z "$V6_ADDR" ] || fail "v6 address must be empty when ungranted (got '$V6_ADDR')"
[ "$(status_field ipv6_state)" = "unavailable" ] || fail "ipv6_state should be unavailable"
[ "$(status_field state)" = "up" ] || fail "v4 state must stay up when v6 is ungranted"
grep -q -- "-6 addr add" "$IP_LOG" && fail "no v6 address should have been added"
echo "ok: v6 ungranted leaves IPv4 healthy and untouched"

# --- 3. v6 appears later (RA delay) — a supervise refresh picks it up ---------
echo 1 > "$V6_HAS_SETTINGS"; echo "$V6ADDR" > "$V6ADDR_FILE"
refresh_v6 wwan0 || fail "refresh_v6 must never fail"
[ "$V6_ADDR" = "$V6ADDR" ] || fail "a later refresh should apply the now-granted v6"
[ "$(status_field ipv6_state)" = "up" ] || fail "ipv6_state should flip to up"
echo "ok: a later supervise refresh brings up v6 without a redial"

# --- 4. v6 address change is re-applied --------------------------------------
NEW6="2409:4072:99:1a6a:dead:beef:cafe:0001"
echo "$NEW6" > "$V6ADDR_FILE"; : > "$IP_LOG"
refresh_v6 wwan0 || fail "refresh_v6 must never fail"
[ "$V6_ADDR" = "$NEW6" ] || fail "a changed v6 address should be re-applied (got '$V6_ADDR')"
grep -q -- "-6 addr add $NEW6/64 dev wwan0" "$IP_LOG" || fail "the new v6 address should be added"
[ "$(status_field ipv6)" = "$NEW6" ] || fail "status should reflect the new v6 address"
echo "ok: a changed v6 address is re-applied"

# --- 5. v6 drop while v4 up: v6 flushed, v4 identity untouched ----------------
_h_before="$PKT_HANDLE"; _c_before="$WDS_CID"
echo 0 > "$V6_HAS_SETTINGS"; : > "$IP_LOG"
refresh_v6 wwan0 || fail "refresh_v6 must never fail"
[ -z "$V6_ADDR" ] || fail "a dropped v6 address must be cleared (got '$V6_ADDR')"
[ "$(status_field ipv6_state)" = "unavailable" ] || fail "ipv6_state should be unavailable after a drop"
grep -q -- "-6 addr flush dev wwan0 scope global" "$IP_LOG" || fail "the stale v6 address should be flushed on drop"
[ "$PKT_HANDLE" = "$_h_before" ] || fail "v4 handle must be untouched by a v6 drop"
[ "$WDS_CID" = "$_c_before" ] || fail "v4 cid must be untouched by a v6 drop"
[ "$(status_field ipv4)" = "100.77.232.222" ] || fail "v4 address must survive a v6 drop"
[ "$(status_field state)" = "up" ] || fail "v4 state must stay up on a v6 drop"
grep -qE "^addr (add 100|flush)" "$IP_LOG" && fail "the v6 drop must not touch the v4 address"
echo "ok: a v6 drop clears v6 only and leaves the v4 session intact"

# --- 6. teardown of the bearer flushes v6 ------------------------------------
echo 1 > "$V6_HAS_SETTINGS"; refresh_v6 wwan0 >/dev/null 2>&1
[ -n "$V6_ADDR" ] || fail "setup: v6 should be up before teardown"
: > "$IP_LOG"
teardown wwan0 >/dev/null 2>&1 || fail "teardown should succeed"
grep -q -- "-6 addr flush dev wwan0 scope global" "$IP_LOG" || fail "teardown should flush v6"
[ -z "$V6_ADDR" ] || fail "teardown should clear the v6 address"
[ "$(status_field ipv6_state)" = "unavailable" ] || fail "ipv6_state should be unavailable after teardown"
echo "ok: teardown of the single bearer flushes v6"

# --- 7. INTERNET_ENABLE_IPV6=0 forces byte-identical IPv4-only ---------------
INTERNET_ENABLE_IPV6=0
: > "$IP_LOG"; : > "$START_LOG"; : > "$MODIFY_LOG"; echo 1 > "$V6_HAS_SETTINGS"
dial wwan0 >/dev/null 2>&1 || fail "v4-only dial should succeed with IPv6 disabled"
[ "$PKT_HANDLE" = "2264216040" ] || fail "v4 must be up with IPv6 disabled"
[ -z "$V6_ADDR" ] || fail "no v6 address when IPv6 disabled"
grep -q 'ip-type=4' "$START_LOG" || fail "disabled dial should use ip-type=4, got: $(cat "$START_LOG")"
[ -s "$MODIFY_LOG" ] && fail "disabled dial must NOT provision an IPv4v6 profile"
grep -q -- "-6 " "$IP_LOG" && fail "no 'ip -6' calls allowed when IPv6 disabled"
[ "$(status_field ipv6_state)" = "unavailable" ] || fail "ipv6_state should be unavailable when disabled"
: > "$IP_LOG"
teardown wwan0 >/dev/null 2>&1
grep -q -- "-6 " "$IP_LOG" && fail "teardown must make no ip -6 change when IPv6 disabled"
INTERNET_ENABLE_IPV6=1
echo "ok: INTERNET_ENABLE_IPV6=0 disables all v6 behavior (dial AND teardown)"

echo "PASS: ipv6_lifecycle_test.sh"
