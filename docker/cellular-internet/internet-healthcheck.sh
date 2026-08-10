#!/usr/bin/env sh
# Docker healthcheck for the cellular-internet sidecar
# (specs/032-cellular-internet-sidecar, contracts/healthcheck-compose.md).
#
# Exit 0  => internet reachable (DNS probe resolved)  => Docker "healthy"
#             => the bridge's `depends_on: service_healthy` gate is released.
# Exit 1  => probe failed                             => "unhealthy" (gate held).
#
# Side-effect-free apart from updating the sidecar-local status file. It never
# dials or reconfigures — that is the entrypoint's job (FR-003).
set -u

# INTERNET_LIB defaults to the in-image path; overridable so the scripts can be
# exercised from a source checkout by the test harness (not a mock — the real
# lib is sourced either way).
INTERNET_LIB="${INTERNET_LIB:-/usr/local/bin/internet-lib.sh}"
# shellcheck source=docker/cellular-internet/internet-lib.sh
. "$INTERNET_LIB"

if probe_dns; then
    write_status up "ok ${INTERNET_PROBE_HOST}@${INTERNET_PROBE_RESOLVER:-system}"
    exit 0
fi

write_status probe-fail "fail ${INTERNET_PROBE_HOST}@${INTERNET_PROBE_RESOLVER:-system}"
exit 1
