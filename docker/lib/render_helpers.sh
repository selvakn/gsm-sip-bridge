#!/usr/bin/env bash
# Pure bash helpers extracted from entrypoint.sh (specs/021-entrypoint-supervise-rust
# Phase 0). The render_line_* trio that used to live here (strongswan.conf/
# swanctl.conf/updown-wrapper rendering) was ported to Rust in Phase 1 —
# see gsm-sip-bridge/src/supervise/render.rs, reached via `gsm-sip-bridge
# render ...`. extract_latest_pcscf remains here pending its own Phase 3 port
# (it feeds the per-line supervision state machine, not the rendering phase).
# Sourced by entrypoint.sh; not meant to be run standalone.

# charon.log accumulates every "received P-CSCF server IP" line for the life
# of the container — including ones from a later re-auth/rekey that assigned
# a *different* P-CSCF than the first. Picks the chronologically last
# matching line overall (`tail -1` after filtering to valid v4/v6
# addresses), not the last of one family checked first (Greptile PR #2,
# specs/012-strongswan-epdg).
extract_latest_pcscf() {
    local log_file="$1"
    local lines
    lines="$(grep -oE 'received P-CSCF server IP .*' "$log_file" 2>/dev/null | sed 's/^received P-CSCF server IP //')"
    echo "$lines" | grep -E '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$|^([0-9a-fA-F]{0,4}:){2,}[0-9a-fA-F:]+$' | tail -1
}
