#!/usr/bin/env sh
# Integration test for the IPv6 address-change hook (specs/035-dual-stack-ipv6).
#
# Exercises the REAL notify_v6_hook / _mark_v6_* / bring_up_v6 / supervise_v6 from
# internet-entrypoint.sh. `qmicli`, `ip`, and `timeout` are scripted stubs on PATH
# (the modem is the only "hardware" mock; the stub `timeout` just runs the command
# so the test is deterministic). Verifies the hook contract: fire once on
# appear/change, never on unchanged/loss/unset, and — crucially — that de-dupe
# gates on a SUCCESS marker so a FAILED hook is retried on a later tick rather than
# stranding the DDNS record, all while a failing hook never breaks the caller.
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
DIR=$(dirname "$HERE")

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
export PATH="$TMP/bin:$PATH"
mkdir -p "$TMP/bin"

QMI_DEV="$TMP/cdc-wdm0"; : > "$QMI_DEV"
V6ADDR_FILE="$TMP/v6addr"
V6_PRESENT="$TMP/v6_present"; echo 1 > "$V6_PRESENT"
A1="2401:4900:1c30:abcd::1"
A2="2401:4900:1c30:ef01::2"
A3="2401:4900:1c30:2222::3"
echo "$A1" > "$V6ADDR_FILE"

cat > "$TMP/bin/qmicli" <<EOF
#!/usr/bin/env sh
for a in "\$@"; do
    case "\$a" in
        --wds-start-network=*ip-type=6*)
            printf '[dev] Network started\n\tPacket data handle: %s\n' "'3300000006'"
            printf '[dev] Client ID not released:\n\tService: %s\n\t    CID: %s\n' "'wds'" "'9'"
            exit 0 ;;
        --wds-get-current-settings)
            printf '\tIPv6 address: %s/64\n\tIPv6 gateway address: fe80::1\n' "\$(cat $V6ADDR_FILE)"
            exit 0 ;;
        --get-service-version-info) exit 0 ;;
    esac
done
exit 0
EOF
# Fake ip: report the address as present (for supervise_v6 liveness) when asked.
cat > "$TMP/bin/ip" <<EOF
#!/usr/bin/env sh
case "\$*" in
    "-6 addr show"*) [ "\$(cat $V6_PRESENT 2>/dev/null)" = 1 ] && echo "    inet6 present/64 scope global" ;;
esac
exit 0
EOF
# Deterministic stub timeout: ignore the duration, run the rest.
cat > "$TMP/bin/timeout" <<'EOF'
#!/usr/bin/env sh
shift
exec "$@"
EOF
chmod +x "$TMP/bin/qmicli" "$TMP/bin/ip" "$TMP/bin/timeout"

INTERNET_NO_MAIN=1; export INTERNET_NO_MAIN
INTERNET_LIB="$DIR/internet-lib.sh"; export INTERNET_LIB
INTERNET_STATUS_FILE="$TMP/status"; export INTERNET_STATUS_FILE
INTERNET_APN="testapn"; export INTERNET_APN
INTERNET_QMI_DEV="$QMI_DEV"; export INTERNET_QMI_DEV
INTERNET_ENABLE_IPV6=1; export INTERNET_ENABLE_IPV6
INTERNET_IPV6_HOOK_TIMEOUT=2s; export INTERNET_IPV6_HOOK_TIMEOUT

MARKER="$INTERNET_STATUS_FILE.v6notified"   # notify_v6_hook's success marker
HOOK="$TMP/bin/hook.sh"; HOOK_LOG="$TMP/hook_log"; : > "$HOOK_LOG"
# write_hook <exit-code>: (re)write the hook to append its address arg and exit N.
write_hook() {
cat > "$HOOK" <<EOF
#!/usr/bin/env sh
echo "\$1" >> $HOOK_LOG
exit ${1:-0}
EOF
chmod +x "$HOOK"
}
write_hook 0
INTERNET_IPV6_HOOK="$HOOK"; export INTERNET_IPV6_HOOK

# shellcheck source=docker/cellular-internet/internet-entrypoint.sh
. "$DIR/internet-entrypoint.sh"

fail() { echo "FAIL: $*" >&2; exit 1; }
hook_count() { wc -l < "$HOOK_LOG" | tr -d ' '; }
marker() { cat "$MARKER" 2>/dev/null; }

# --- 1. first appearance fires once and records the success marker -----------
rm -f "$MARKER"; V6_ADDR="$A1"
notify_v6_hook; wait
[ "$(hook_count)" = 1 ] || fail "first appearance should fire once (got $(hook_count))"
[ "$(tail -n1 "$HOOK_LOG")" = "$A1" ] || fail "hook should receive the new address as arg 1"
[ "$(marker)" = "$A1" ] || fail "a successful hook should record the marker"
echo "ok: hook fires once on first global address"

# --- 2. unchanged address does NOT re-fire -----------------------------------
notify_v6_hook; wait
[ "$(hook_count)" = 1 ] || fail "unchanged address must not re-fire the hook"
echo "ok: unchanged address does not re-fire"

# --- 3. a changed address fires again with the new value ---------------------
V6_ADDR="$A2"
notify_v6_hook; wait
[ "$(hook_count)" = 2 ] || fail "changed address should fire again"
[ "$(tail -n1 "$HOOK_LOG")" = "$A2" ] || fail "hook should receive the changed address"
[ "$(marker)" = "$A2" ] || fail "marker should advance to the changed address"
echo "ok: a changed address fires again"

# --- 4. loss (v6 down) does NOT fire -----------------------------------------
_mark_v6_down
notify_v6_hook; wait
[ "$(hook_count)" = 2 ] || fail "v6 loss must not fire the hook"
echo "ok: v6 loss does not fire the hook"

# --- 5. unset hook does nothing ----------------------------------------------
V6_ADDR="$A3"; INTERNET_IPV6_HOOK=""
notify_v6_hook; wait
[ "$(hook_count)" = 2 ] || fail "an unset hook must not fire"
INTERNET_IPV6_HOOK="$HOOK"
echo "ok: unset hook is a no-op"

# --- 6. non-executable hook warns and does not fire --------------------------
V6_ADDR="$A3"; : > "$TMP/nonexec"; INTERNET_IPV6_HOOK="$TMP/nonexec"
_out=$(notify_v6_hook 2>&1); wait
echo "$_out" | grep -qi 'not executable' || fail "should warn about a non-executable hook"
[ "$(hook_count)" = 2 ] || fail "a non-executable hook must not fire"
INTERNET_IPV6_HOOK="$HOOK"
echo "ok: non-executable hook warns, does not fire"

# --- 7. a FAILED hook runs, does NOT advance the marker, isolates the caller --
rm -f "$MARKER"; : > "$HOOK_LOG"; write_hook 1; V6_ADDR="$A3"
notify_v6_hook || fail "notify must not propagate the hook's failure"
wait
[ "$(hook_count)" = 1 ] || fail "a failing hook should still have run once"
[ "$(marker)" != "$A3" ] || fail "a FAILED hook must NOT advance the success marker"
echo "ok: a failed hook runs, leaves the marker stale, does not break the caller"

# --- 8. a stale marker is retried until the hook succeeds ---------------------
write_hook 0
notify_v6_hook; wait
[ "$(hook_count)" = 2 ] || fail "a stale marker should trigger a retry"
[ "$(marker)" = "$A3" ] || fail "a successful retry should advance the marker"
echo "ok: a stale marker is retried until the hook succeeds"

# --- 9. end-to-end: a real bring_up_v6 fires the hook once -------------------
write_hook 0; : > "$HOOK_LOG"; rm -f "$MARKER"
V6_ADDR=""; V6_WDS_CID=""; V6_MODE="none"; echo "$A1" > "$V6ADDR_FILE"
bring_up_v6 wwan0 >/dev/null 2>&1 || fail "bring_up_v6 should succeed"
wait
[ "$(hook_count)" = 1 ] || fail "a real bring_up should fire the hook once (got $(hook_count))"
[ "$(marker)" = "$A1" ] || fail "end-to-end should record the marker"
echo "ok: bring_up_v6 wires the hook on the up transition"

# --- 10. supervise_v6 retries a failed hook while v6 stays up -----------------
rm -f "$MARKER"; : > "$HOOK_LOG"; echo 1 > "$V6_PRESENT"; write_hook 1
V6_ADDR="$A1"; V6_WDS_CID="9"; V6_MODE="dual-session"
supervise_v6 wwan0 10 || fail "supervise must not fail"
wait
[ "$(hook_count)" = 1 ] || fail "supervise up-branch should (re)fire a stale hook"
[ "$(marker)" != "$A1" ] || fail "a failed hook must leave the marker stale"
write_hook 0
supervise_v6 wwan0 10 || fail "supervise must not fail"
wait
[ "$(marker)" = "$A1" ] || fail "supervise should keep retrying until the hook succeeds"
echo "ok: supervise_v6 retries a failed hook while v6 stays up"

echo "PASS: ipv6_hook_test.sh"
