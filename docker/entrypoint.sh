#!/usr/bin/env bash
# Entrypoint for the unified gsm-sip-bridge image.
#
# specs/021-entrypoint-supervise-rust: everything this script used to do
# itself — resolving the VoWiFi/VoLTE line table, rendering strongSwan/vpcd
# config assets, supervising the circuit-switched daemon and every per-line
# process, and a clean shutdown — now lives in the `gsm-sip-bridge supervise`
# subcommand (gsm-sip-bridge/src/supervise/), unit-tested and live-validated
# against a real EC20 + Airtel SIM (see specs/021-entrypoint-supervise-rust/
# DECISIONS-LOG.md). This script is now just the precondition checks that
# have to run before anything else can, plus the exec.
set -uo pipefail

GSM_SIP_BRIDGE_BIN="${GSM_SIP_BRIDGE_BIN:-/usr/local/bin/gsm-sip-bridge}"
GSM_SIP_BRIDGE_CONFIG="${GSM_SIP_BRIDGE_CONFIG:-/etc/gsm-sip-bridge/config.toml}"

log() { echo "[entrypoint] $*"; }

if [ ! -x "$GSM_SIP_BRIDGE_BIN" ]; then
    log "FATAL: $GSM_SIP_BRIDGE_BIN not present in this image (build problem)"
    exit 1
fi
if [ ! -f "$GSM_SIP_BRIDGE_CONFIG" ]; then
    log "FATAL: $GSM_SIP_BRIDGE_CONFIG not mounted — see docker-compose.yml's config.toml volume"
    exit 1
fi

exec "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" supervise
