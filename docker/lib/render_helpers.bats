#!/usr/bin/env bats
# Phase 0 safety net (specs/021-entrypoint-supervise-rust): locks in current
# behavior of extract_latest_pcscf, the one helper still pending its Rust
# port (Phase 3, alongside the per-line supervision state machine it feeds).
# The render_line_* trio this file used to also cover was ported to Rust in
# Phase 1 — see gsm-sip-bridge/src/supervise/render.rs's own tests/snapshots.

setup() {
    load_dir="$(cd "$(dirname "$BATS_TEST_FILENAME")" && pwd)"
    # shellcheck source=./render_helpers.sh
    source "$load_dir/render_helpers.sh"
}

@test "extract_latest_pcscf: picks the only IPv4 line" {
    log="$BATS_TEST_TMPDIR/charon.log"
    printf 'received P-CSCF server IP 10.0.0.1\n' >"$log"
    result="$(extract_latest_pcscf "$log")"
    [ "$result" = "10.0.0.1" ]
}

@test "extract_latest_pcscf: picks the chronologically last line across a rekey, not the last-of-one-family (Greptile PR #2 regression)" {
    # A rekey/re-auth can assign a fresh P-CSCF; the fix picks the last VALID
    # line overall (by log order), not the last IPv4 line and separately the
    # last IPv6 line and then some family-priority pick between them.
    log="$BATS_TEST_TMPDIR/charon.log"
    cat >"$log" <<'EOF'
received P-CSCF server IP 10.0.0.1
received P-CSCF server IP 2001:db8::1
received P-CSCF server IP 10.0.0.9
EOF
    result="$(extract_latest_pcscf "$log")"
    [ "$result" = "10.0.0.9" ]
}

@test "extract_latest_pcscf: an IPv6-last rekey is picked correctly too" {
    log="$BATS_TEST_TMPDIR/charon.log"
    cat >"$log" <<'EOF'
received P-CSCF server IP 10.0.0.1
received P-CSCF server IP 2001:db8::9
EOF
    result="$(extract_latest_pcscf "$log")"
    [ "$result" = "2001:db8::9" ]
}

@test "extract_latest_pcscf: no matching line at all yields empty" {
    log="$BATS_TEST_TMPDIR/charon.log"
    printf 'nothing relevant here\n' >"$log"
    result="$(extract_latest_pcscf "$log")"
    [ -z "$result" ]
}

@test "extract_latest_pcscf: missing log file yields empty, not an error" {
    result="$(extract_latest_pcscf "$BATS_TEST_TMPDIR/does-not-exist.log")"
    [ -z "$result" ]
}
