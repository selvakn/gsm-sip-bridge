#!/usr/bin/env sh
# Integration test for the sidecar's WDS session lifecycle
# (specs/032-cellular-internet-sidecar).
#
# Exercises the REAL dial/teardown functions from internet-entrypoint.sh. Only
# the modem itself is faked: `qmicli` and `ip` are replaced with scripted stubs
# on PATH, because a cellular modem is precisely the "hardware not available in
# CI" case the constitution allows a mock for. The logic under test — which
# client identity is retained, released, or retried — is the real thing.
#
# This exists because that lifecycle broke three times in review: the client id
# was discarded, a partial dial leaked it, and a failed teardown dropped it.
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
DIR=$(dirname "$HERE")

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
export PATH="$TMP/bin:$PATH"
mkdir -p "$TMP/bin"

STOP_RESULT="$TMP/stop_result"      # "ok" or "fail" — how the fake stop behaves
START_MODE="$TMP/start_mode"        # "full", "no-handle", or "no-cid"
REACHABLE="$TMP/reachable"          # "ok" or "hangup" — can qmicli open the node?
CONNECTED="$TMP/connected"          # "connected" or "disconnected" — PS session
QMI_DEV="$TMP/cdc-wdm0"             # presence stands in for the modem existing
: > "$QMI_DEV"
echo ok > "$STOP_RESULT"
echo full > "$START_MODE"
echo ok > "$REACHABLE"
echo connected > "$CONNECTED"

cat > "$TMP/bin/qmicli" <<EOF
#!/usr/bin/env sh
# A wedged control endpoint fails EVERY action, not just the stop, with the
# libqmi "endpoint hangup" open error. Model that first so a "hangup" run can't
# accidentally satisfy any command.
if [ "\$(cat $REACHABLE)" != ok ]; then
    echo "error: couldn't open the QmiDevice: endpoint hangup" >&2
    exit 1
fi
for a in "\$@"; do
    case "\$a" in
        --wds-start-network=*)
            case "\$(cat $START_MODE)" in
                no-handle)
                    printf '[dev] Network started\n'
                    printf '[dev] Client ID not released:\n\tService: %s\n\t    CID: %s\n' "'wds'" "'7'"
                    exit 0 ;;
                no-cid)
                    printf '[dev] Network started\n\tPacket data handle: %s\n' "'2264216040'"
                    exit 0 ;;
            esac
            printf '[dev] Network started\n\tPacket data handle: %s\n' "'2264216040'"
            printf '[dev] Client ID not released:\n\tService: %s\n\t    CID: %s\n' "'wds'" "'7'"
            exit 0 ;;
        --wds-stop-network=*)
            [ "\$(cat $STOP_RESULT)" = ok ] && exit 0
            exit 1 ;;
        --wds-get-current-settings)
            printf '\tIPv4 address: 100.72.13.4\n\tIPv4 gateway address: 100.72.13.1\n\tIPv4 subnet mask: 255.255.255.0\n\tIPv4 primary DNS: 8.8.8.8\n'
            exit 0 ;;
        --wds-get-packet-service-status)
            printf "Connection status: '%s'\n" "\$(cat $CONNECTED)"
            exit 0 ;;
        --get-service-version-info)
            exit 0 ;;
    esac
done
exit 0
EOF
cat > "$TMP/bin/ip" <<'EOF'
#!/usr/bin/env sh
exit 0
EOF
# Keep proxy recovery hermetic: report no running qmi-proxy, so recover_qmi_proxy
# is inert and the test never signals a real process on the build host.
cat > "$TMP/bin/pidof" <<'EOF'
#!/usr/bin/env sh
exit 1
EOF
chmod +x "$TMP/bin/qmicli" "$TMP/bin/ip" "$TMP/bin/pidof"

# Load the functions without starting the supervise loop.
INTERNET_NO_MAIN=1
export INTERNET_NO_MAIN
INTERNET_LIB="$DIR/internet-lib.sh"
export INTERNET_LIB
INTERNET_STATUS_FILE="$TMP/status"
export INTERNET_STATUS_FILE
INTERNET_APN="testapn"
export INTERNET_APN
INTERNET_QMI_DEV="$QMI_DEV"
export INTERNET_QMI_DEV
# This test is deliberately scoped to the IPv4 WDS lifecycle. Disable dual-stack
# (specs/035) so `dial` does not make incidental v6 calls against this v4-only
# fake modem — the v4-parity of a dual-stack dial is proven in ipv6_lifecycle_test.
INTERNET_ENABLE_IPV6=0
export INTERNET_ENABLE_IPV6
# shellcheck source=docker/cellular-internet/internet-entrypoint.sh
. "$DIR/internet-entrypoint.sh"

fail() { echo "FAIL: $*" >&2; exit 1; }

# --- a successful dial records BOTH the handle and the client id ------------
echo ok > "$STOP_RESULT"
dial wwan0 >/dev/null 2>&1 || fail "dial should succeed with the fake modem"
[ "$PKT_HANDLE" = "2264216040" ] || fail "packet handle not captured (got '$PKT_HANDLE')"
[ "$WDS_CID" = "7" ] || fail "WDS client id not captured (got '$WDS_CID')"
echo "ok: dial captures handle=$PKT_HANDLE cid=$WDS_CID"

# --- a successful teardown releases the identity ----------------------------
teardown wwan0 >/dev/null 2>&1 || fail "teardown should succeed when stop succeeds"
if [ -n "$PKT_HANDLE" ] || [ -n "$WDS_CID" ]; then
    fail "identity should be cleared after a successful stop"
fi
echo "ok: successful teardown clears the identity"

# --- a FAILED teardown retains the identity so it can be retried ------------
# Precondition for RETAINING: the modem is reachable AND the session is still
# connected, so the stuck stop really is a live session of ours worth retrying.
echo ok > "$STOP_RESULT"
echo ok > "$REACHABLE"; echo connected > "$CONNECTED"
dial wwan0 >/dev/null 2>&1 || fail "second dial should succeed"
echo fail > "$STOP_RESULT"
if teardown wwan0 >/dev/null 2>&1; then
    fail "teardown should report failure when stop keeps failing"
fi
[ "$WDS_CID" = "7" ] || fail "a failed teardown must RETAIN the client id, not drop it (got '$WDS_CID')"
echo "ok: failed teardown retains cid=$WDS_CID for retry"

# --- dial must not stack a new session on an unreleased client --------------
if dial wwan0 >/dev/null 2>&1; then
    fail "dial must refuse to start a new session while the previous is unreleased"
fi
[ "$WDS_CID" = "7" ] || fail "the retained identity should still be the original client"
echo "ok: dial refuses to stack a second client on an unreleased one"

# --- once the stop works again, dial recovers -------------------------------
echo ok > "$STOP_RESULT"
dial wwan0 >/dev/null 2>&1 || fail "dial should recover once the session can be released"
echo "ok: dial recovers after the stuck session is released"

# --- a stop that fails because QMI is UNREACHABLE drops the identity ---------
# A present-but-wedged control endpoint (endpoint hangup after a firmware stall
# or a silent USB re-enumeration) fails every action, stop included. Retaining
# the identity there deadlocks the sidecar forever (dial refuses to start a new
# session while one is retained); teardown must instead drop it — and recycle
# the proxy — so the redial loop can recover once the channel is back.
echo ok > "$STOP_RESULT"; echo ok > "$REACHABLE"; echo connected > "$CONNECTED"
dial wwan0 >/dev/null 2>&1 || fail "dial should succeed before the unreachable case"
echo fail > "$STOP_RESULT"
echo hangup > "$REACHABLE"
teardown wwan0 >/dev/null 2>&1 || fail "teardown must succeed (drop identity) when QMI is unreachable"
if [ -n "$PKT_HANDLE" ] || [ -n "$WDS_CID" ]; then
    fail "an unreachable-QMI teardown must drop the stale identity (handle='$PKT_HANDLE' cid='$WDS_CID')"
fi
echo "ok: unreachable-QMI teardown drops the identity instead of deadlocking"
echo ok > "$REACHABLE"

# --- a stop that fails but the session already ended drops the identity ------
# The carrier can tear the session down on its own, leaving our handle stale:
# the stop then fails with nothing to stop. Retaining would deadlock exactly the
# same way; teardown must drop the identity and let the next dial start fresh.
echo ok > "$STOP_RESULT"; echo ok > "$REACHABLE"; echo connected > "$CONNECTED"
dial wwan0 >/dev/null 2>&1 || fail "dial should succeed before the disconnected case"
echo fail > "$STOP_RESULT"
echo disconnected > "$CONNECTED"
teardown wwan0 >/dev/null 2>&1 || fail "teardown must succeed (drop identity) when the session already ended"
if [ -n "$PKT_HANDLE" ] || [ -n "$WDS_CID" ]; then
    fail "a teardown whose session already ended must drop the stale identity (handle='$PKT_HANDLE' cid='$WDS_CID')"
fi
echo "ok: already-ended-session teardown drops the identity instead of deadlocking"
echo connected > "$CONNECTED"

# --- a started-but-unparseable session must fail the dial, not leak ---------
# A start that returns a partial identity (missing handle or CID) can never be
# torn down cleanly: teardown would clear it while the client stays retained,
# and every redial would stack another until the modem refuses new sessions.
teardown wwan0 >/dev/null 2>&1 || fail "teardown should clear the recovered session"
for mode in no-handle no-cid; do
    echo "$mode" > "$START_MODE"
    if dial wwan0 >/dev/null 2>&1; then
        fail "dial must fail when the start returns a $mode (unstoppable) session"
    fi
    if [ -n "$PKT_HANDLE" ] || [ -n "$WDS_CID" ]; then
        fail "dial ($mode) must not retain a partial identity (handle='$PKT_HANDLE' cid='$WDS_CID')"
    fi
    echo "ok: dial fails and leaks nothing when the start is $mode"
done
echo full > "$START_MODE"

# --- a vanished modem drops the identity (its clients died with it) ---------
rm -f "$QMI_DEV"
echo fail > "$STOP_RESULT"
teardown wwan0 >/dev/null 2>&1 || fail "teardown should succeed when the modem is gone"
[ -z "$WDS_CID" ] || fail "a vanished modem's client id should be dropped, not retried forever"
echo "ok: vanished modem drops the identity"

echo "PASS: wds_lifecycle_test.sh"
