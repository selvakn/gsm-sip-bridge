#!/usr/bin/env sh
# Integration test for the sidecar's dual-stack IPv6 lifecycle
# (specs/035-dual-stack-ipv6).
#
# Exercises the REAL v6 functions from internet-entrypoint.sh (bring_up_v6,
# apply_settings_v6, supervise_v6, v6_teardown_cleanup, _mark_v6_*). Only the
# modem is faked: `qmicli` and `ip` are scripted stubs on PATH — the constitution's
# "hardware not available in CI" exception. The logic under test — dual-stack dial,
# global-v6 detection, best-effort capped backoff, and the strict rule that v6 code
# never disturbs the v4 session or `state` — is the real thing.
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
DIR=$(dirname "$HERE")

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
export PATH="$TMP/bin:$PATH"
mkdir -p "$TMP/bin"

QMI_DEV="$TMP/cdc-wdm0"; : > "$QMI_DEV"
IP_LOG="$TMP/ip_log"; : > "$IP_LOG"
V6_START_MODE="$TMP/v6_start_mode"   # ok | fail | noeffect
V6_HAS_SETTINGS="$TMP/v6_has_settings" # 1 => get-current-settings includes IPv6
V6_PRESENT="$TMP/v6_present"          # 1 => `ip -6 addr show` reports the addr
echo ok > "$V6_START_MODE"
echo 1  > "$V6_HAS_SETTINGS"
echo 1  > "$V6_PRESENT"
V6_STOP="$TMP/v6_stop";           echo ok        > "$V6_STOP"        # ok | fail
V6_CONNECTED="$TMP/v6_connected"; echo connected > "$V6_CONNECTED"   # connected | disconnected
REACHABLE="$TMP/reachable";       echo ok        > "$REACHABLE"      # ok | hangup

V6ADDR="2401:4900:1c30:abcd::1"

cat > "$TMP/bin/qmicli" <<EOF
#!/usr/bin/env sh
# A wedged endpoint fails EVERY action (models "endpoint hangup").
if [ "\$(cat $REACHABLE)" != ok ]; then
    echo "error: couldn't open the QmiDevice: endpoint hangup" >&2
    exit 1
fi
for a in "\$@"; do
    case "\$a" in
        --wds-start-network=*ip-type=4*)
            printf '[dev] Network started\n\tPacket data handle: %s\n' "'2264216040'"
            printf '[dev] Client ID not released:\n\tService: %s\n\t    CID: %s\n' "'wds'" "'7'"
            exit 0 ;;
        --wds-start-network=*ip-type=6*)
            case "\$(cat $V6_START_MODE)" in
                fail)     echo "error: wds-start-network failed" >&2; exit 1 ;;
                noeffect) echo "error: NoEffect" >&2; exit 1 ;;
            esac
            printf '[dev] Network started\n\tPacket data handle: %s\n' "'3300000006'"
            printf '[dev] Client ID not released:\n\tService: %s\n\t    CID: %s\n' "'wds'" "'9'"
            exit 0 ;;
        --wds-stop-network=*)
            if [ "\$(cat $V6_STOP)" = ok ]; then exit 0; fi
            exit 1 ;;
        --wds-get-current-settings)
            printf '\tIPv4 address: 100.72.13.4\n\tIPv4 gateway address: 100.72.13.1\n\tIPv4 subnet mask: 255.255.255.0\n\tIPv4 primary DNS: 8.8.8.8\n'
            if [ "\$(cat $V6_HAS_SETTINGS)" = 1 ]; then
                printf '\tIPv6 address: $V6ADDR/64\n\tIPv6 gateway address: fe80::1\n\tIPv6 primary DNS: 2401:4900::1\n'
            fi
            exit 0 ;;
        --wds-get-packet-service-status)
            printf "Connection status: '%s'\n" "\$(cat $V6_CONNECTED)"; exit 0 ;;
        --get-service-version-info) exit 0 ;;
    esac
done
exit 0
EOF

cat > "$TMP/bin/ip" <<EOF
#!/usr/bin/env sh
echo "\$*" >> $IP_LOG
case "\$*" in
    "-6 addr show"*)
        [ "\$(cat $V6_PRESENT 2>/dev/null)" = 1 ] && echo "    inet6 $V6ADDR/64 scope global" ;;
esac
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
INTERNET_APN="testapn"; export INTERNET_APN
INTERNET_QMI_DEV="$QMI_DEV"; export INTERNET_QMI_DEV
INTERNET_ENABLE_IPV6=1; export INTERNET_ENABLE_IPV6
INTERNET_IPV6_RETRY_MAX=5m; export INTERNET_IPV6_RETRY_MAX
# shellcheck source=docker/cellular-internet/internet-entrypoint.sh
. "$DIR/internet-entrypoint.sh"

fail() { echo "FAIL: $*" >&2; exit 1; }
status_field() { sed -n "s/^$1=//p" "$INTERNET_STATUS_FILE" 2>/dev/null; }

# --- 1. dual-stack dial: v4 unchanged AND a global v6 addr+route applied ------
echo ok > "$V6_START_MODE"; echo 1 > "$V6_HAS_SETTINGS"; echo 1 > "$V6_PRESENT"
dial wwan0 >/dev/null 2>&1 || fail "dual-stack dial should succeed"
[ "$PKT_HANDLE" = "2264216040" ] || fail "v4 handle changed (got '$PKT_HANDLE')"
[ "$WDS_CID" = "7" ] || fail "v4 cid changed (got '$WDS_CID')"
[ "$V6_ADDR" = "$V6ADDR" ] || fail "v6 address not applied (got '$V6_ADDR')"
[ "$V6_PREFIX" = "64" ] || fail "v6 prefix not parsed (got '$V6_PREFIX')"
[ "$V6_WDS_CID" = "9" ] || fail "v6 client id not captured (got '$V6_WDS_CID')"
[ "$V6_MODE" = "dual-session" ] || fail "v6 mode should be dual-session (got '$V6_MODE')"
grep -q -- "-6 addr add $V6ADDR/64 dev wwan0" "$IP_LOG" || fail "no 'ip -6 addr add' recorded"
grep -q -- "-6 route replace default via fe80::1 dev wwan0" "$IP_LOG" || fail "no 'ip -6 route' recorded"
[ "$(status_field ipv6)" = "$V6ADDR" ] || fail "status ipv6 not set"
[ "$(status_field ipv6_state)" = "up" ] || fail "status ipv6_state should be up"
[ "$(status_field state)" = "up" ] || fail "v4 state should be up"
echo "ok: dual-stack dial applies a global v6 address, v4 unchanged"

# --- 2. carrier grants no v6: v4 stays up, v6 unavailable, v4 untouched -------
teardown wwan0 >/dev/null 2>&1; v6_teardown_cleanup wwan0 >/dev/null 2>&1
: > "$IP_LOG"; echo fail > "$V6_START_MODE"; echo 0 > "$V6_HAS_SETTINGS"
dial wwan0 >/dev/null 2>&1 || fail "dial should still succeed when v6 is ungranted"
[ "$PKT_HANDLE" = "2264216040" ] || fail "v4 must be up even without v6"
[ -z "$V6_ADDR" ] || fail "v6 address must be empty when ungranted (got '$V6_ADDR')"
[ -z "$V6_WDS_CID" ] || fail "no v6 client should be retained on a failed v6 start"
[ "$V6_MODE" = "none" ] || fail "v6 mode should be none when ungranted (got '$V6_MODE')"
[ "$(status_field ipv6_state)" = "unavailable" ] || fail "ipv6_state should be unavailable"
[ "$(status_field state)" = "up" ] || fail "v4 state must stay up when v6 is ungranted"
grep -q -- "-6 addr add" "$IP_LOG" && fail "no v6 address should have been added"
echo "ok: v6 ungranted leaves IPv4 healthy and untouched"

# --- 3. capped backoff while v6 stays unavailable ----------------------------
echo fail > "$V6_START_MODE"; echo 0 > "$V6_HAS_SETTINGS"
V6_ADDR=""; V6_NEXT_RETRY=0; V6_RETRY_INTERVAL=0
supervise_v6 wwan0 10 || fail "supervise_v6 must never return nonzero"
[ "$V6_RETRY_INTERVAL" = "10" ] || fail "first backoff should be the floor 10 (got '$V6_RETRY_INTERVAL')"
[ "$V6_NEXT_RETRY" -gt 0 ] 2>/dev/null || fail "next-retry deadline should be scheduled"
# Before the deadline, no new attempt and the interval is unchanged.
_saved_iface_calls=$(wc -l < "$IP_LOG")
supervise_v6 wwan0 10 || fail "supervise_v6 must never return nonzero"
[ "$V6_RETRY_INTERVAL" = "10" ] || fail "interval must not grow before the deadline"
# Force the deadline to pass: the interval doubles to 20.
V6_NEXT_RETRY=0
supervise_v6 wwan0 10 || fail "supervise_v6 must never return nonzero"
[ "$V6_RETRY_INTERVAL" = "20" ] || fail "backoff should double to 20 (got '$V6_RETRY_INTERVAL')"
# Cap at INTERNET_IPV6_RETRY_MAX (5m => 300s).
V6_RETRY_INTERVAL=200; V6_NEXT_RETRY=0
supervise_v6 wwan0 10 || fail "supervise_v6 must never return nonzero"
[ "$V6_RETRY_INTERVAL" = "300" ] || fail "backoff should cap at 300 (got '$V6_RETRY_INTERVAL')"
[ "$(status_field state)" = "up" ] || fail "v4 state must stay up throughout v6 backoff"
echo "ok: v6 retry uses a capped backoff and never disturbs v4"

# --- 4. success resets the backoff -------------------------------------------
echo ok > "$V6_START_MODE"; echo 1 > "$V6_HAS_SETTINGS"; echo 1 > "$V6_PRESENT"
V6_WDS_CID=""; V6_MODE="none"; V6_ADDR=""; V6_NEXT_RETRY=0; V6_RETRY_INTERVAL=99
supervise_v6 wwan0 10 || fail "supervise_v6 must never return nonzero"
[ "$V6_ADDR" = "$V6ADDR" ] || fail "v6 should re-establish on success"
[ "$V6_RETRY_INTERVAL" = "0" ] || fail "backoff must reset to 0 once v6 is up (got '$V6_RETRY_INTERVAL')"
echo "ok: a successful re-establish resets the backoff"

# --- 5. carrier drop while v4 up: v6 flipped down, v4 identity untouched ------
_h_before="$PKT_HANDLE"; _c_before="$WDS_CID"
echo fail > "$V6_START_MODE"; echo 0 > "$V6_HAS_SETTINGS"; echo 0 > "$V6_PRESENT"
: > "$IP_LOG"
supervise_v6 wwan0 10 || fail "supervise_v6 must never return nonzero"
[ -z "$V6_ADDR" ] || fail "a dropped v6 address must be cleared (got '$V6_ADDR')"
[ "$(status_field ipv6_state)" = "unavailable" ] || fail "ipv6_state should be unavailable after a drop"
[ "$PKT_HANDLE" = "$_h_before" ] || fail "v4 handle must be untouched by a v6 drop"
[ "$WDS_CID" = "$_c_before" ] || fail "v4 cid must be untouched by a v6 drop"
[ "$(status_field ipv4)" = "100.72.13.4" ] || fail "v4 address must survive a v6 drop"
[ "$(status_field state)" = "up" ] || fail "v4 state must stay up on a v6 drop"
grep -q -- "-6 " "$IP_LOG" || fail "the drop path should only touch v6 (ip -6 ...)"
grep -qE "^addr (add 100|flush)" "$IP_LOG" && fail "the v6 drop must not touch the v4 address"
echo "ok: a v6 drop clears v6 only and leaves the v4 session intact"

# --- 6. failed v6 stop on a GONE session drops the stale id and recovers ------
# A retained-but-dead v6 client would make bring_up_v6 skip starting a fresh
# session and forever query the dead client, stranding reach-back. When the stop
# fails AND the session is no longer connected, teardown must DROP the identity so
# a fresh session can start.
echo ok > "$V6_STOP"; echo connected > "$V6_CONNECTED"; echo ok > "$REACHABLE"
echo ok > "$V6_START_MODE"; echo 1 > "$V6_HAS_SETTINGS"; echo 1 > "$V6_PRESENT"
teardown wwan0 >/dev/null 2>&1; v6_teardown_cleanup wwan0 >/dev/null 2>&1
dial wwan0 >/dev/null 2>&1 || fail "setup dial should succeed"
[ "$V6_WDS_CID" = "9" ] || fail "setup: v6 session should be up (cid 9), got '$V6_WDS_CID'"
echo fail > "$V6_STOP"; echo disconnected > "$V6_CONNECTED"
v6_teardown_cleanup wwan0 >/dev/null 2>&1
[ -z "$V6_WDS_CID" ] || fail "a stop-fail on a GONE session must drop the stale cid (got '$V6_WDS_CID')"
[ "$V6_MODE" = "none" ] || fail "mode should reset to none after dropping a stale v6 client"
echo ok > "$V6_STOP"; echo connected > "$V6_CONNECTED"
bring_up_v6 wwan0 >/dev/null 2>&1 || fail "bring_up_v6 should start a FRESH session after the stale id is dropped"
[ "$V6_WDS_CID" = "9" ] || fail "a fresh v6 session should be started (cid 9)"
[ "$V6_ADDR" = "$V6ADDR" ] || fail "v6 should be back up after recovery"
echo "ok: a stop-fail on a gone session drops the stale id and recovers"

# --- 7. failed v6 stop but STILL CONNECTED retains the id to retry ------------
echo fail > "$V6_STOP"; echo connected > "$V6_CONNECTED"; echo ok > "$REACHABLE"
v6_teardown_cleanup wwan0 >/dev/null 2>&1
[ "$V6_WDS_CID" = "9" ] || fail "a stop-fail on a still-connected session must RETAIN the cid"
[ "$V6_MODE" = "dual-session" ] || fail "a retained v6 session stays dual-session"
echo "ok: a stop-fail on a still-connected session retains the id to retry"
echo ok > "$V6_STOP"

# --- 8. INTERNET_ENABLE_IPV6=0 forces byte-identical IPv4-only ----------------
teardown wwan0 >/dev/null 2>&1; v6_teardown_cleanup wwan0 >/dev/null 2>&1
INTERNET_ENABLE_IPV6=0
: > "$IP_LOG"; echo ok > "$V6_START_MODE"; echo 1 > "$V6_HAS_SETTINGS"
dial wwan0 >/dev/null 2>&1 || fail "v4-only dial should succeed with IPv6 disabled"
[ "$PKT_HANDLE" = "2264216040" ] || fail "v4 must be up with IPv6 disabled"
[ -z "$V6_ADDR" ] || fail "no v6 address when IPv6 disabled"
grep -q -- "-6 " "$IP_LOG" && fail "no 'ip -6' calls allowed when IPv6 disabled"
[ "$(status_field ipv6_state)" = "unavailable" ] || fail "ipv6_state should be unavailable when disabled"
# The kill-switch must ALSO silence the teardown/trap path (FR-011/SC-007): a
# redial or shutdown must make no ip -6 change on a disabled interface, in case
# something else manages v6 there.
: > "$IP_LOG"
v6_teardown_cleanup wwan0 >/dev/null 2>&1
grep -q -- "-6 " "$IP_LOG" && fail "v6_teardown_cleanup must be a no-op when IPv6 disabled"
INTERNET_ENABLE_IPV6=1
echo "ok: INTERNET_ENABLE_IPV6=0 disables all v6 behavior (dial AND teardown)"

echo "PASS: ipv6_lifecycle_test.sh"
