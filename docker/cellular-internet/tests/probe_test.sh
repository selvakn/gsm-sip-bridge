#!/usr/bin/env sh
# Integration test for the cellular-internet readiness probe
# (specs/032-cellular-internet-sidecar, task T007).
#
# Exercises the REAL internet-healthcheck.sh + internet-lib.sh (no mocks,
# Constitution I) through two hermetic cases that need no cellular link and no
# external network:
#   - success: resolve `localhost` via the system resolver (/etc/hosts) -> exit 0
#   - failure: resolve a guaranteed-unresolvable name              -> exit 1
# and asserts the sidecar-local status file reflects each outcome.
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
DIR=$(dirname "$HERE")                       # docker/cellular-internet
LIB="$DIR/internet-lib.sh"
HC="$DIR/internet-healthcheck.sh"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
STATUS="$TMP/internet-status"

fail() { echo "FAIL: $*" >&2; exit 1; }

# --- success path ----------------------------------------------------------
# Empty resolver => system resolver (getent), localhost is always in /etc/hosts.
if INTERNET_LIB="$LIB" INTERNET_STATUS_FILE="$STATUS" \
   INTERNET_PROBE_RESOLVER="" INTERNET_PROBE_HOST="localhost" \
   sh "$HC"; then
    :
else
    fail "healthcheck should exit 0 when the probe host resolves (localhost)"
fi
grep -q '^state=up' "$STATUS" || fail "status should be state=up on success (got: $(cat "$STATUS"))"
grep -q '^probe=ok' "$STATUS" || fail "status should record probe=ok on success"
echo "ok: success path -> exit 0, state=up"

# --- failure path ----------------------------------------------------------
# `.invalid` is reserved (RFC 6761) and never resolves; getent fails fast and
# nslookup returns NXDOMAIN (or times out offline) — either way nonzero.
if INTERNET_LIB="$LIB" INTERNET_STATUS_FILE="$STATUS" \
   INTERNET_PROBE_RESOLVER="" INTERNET_PROBE_HOST="nope.this-does-not-exist.invalid" \
   sh "$HC"; then
    fail "healthcheck should exit nonzero when the probe host does not resolve"
fi
grep -q '^state=probe-fail' "$STATUS" || fail "status should be state=probe-fail on failure (got: $(cat "$STATUS"))"
echo "ok: failure path -> nonzero, state=probe-fail"

echo "PASS: probe_test.sh"
