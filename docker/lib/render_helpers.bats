#!/usr/bin/env bats
# Phase 0 safety net (specs/021-entrypoint-supervise-rust): locks in current behavior
# of the pure bash helpers before they are ported to Rust in Phase 1.

setup() {
    load_dir="$(cd "$(dirname "$BATS_TEST_FILENAME")" && pwd)"
    # shellcheck source=./render_helpers.sh
    source "$load_dir/render_helpers.sh"
    export RENDER_HELPERS_ROOT="$BATS_TEST_TMPDIR"
    # The real container image's /etc and /etc/strongswan.d (created by the
    # strongSwan package install, docker/Dockerfile) already exist by the time
    # entrypoint.sh runs — these functions never create them, only files
    # inside them. Recreate that precondition under RENDER_HELPERS_ROOT so the
    # 1:1 port stays exactly as unconditional as the original.
    mkdir -p "$RENDER_HELPERS_ROOT/etc/strongswan.d"
}

# --- extract_latest_pcscf ---------------------------------------------------

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

# --- render_line_strongswan_conf --------------------------------------------

@test "render_line_strongswan_conf: renders per-line vici socket and filelog path" {
    conf_path="$(render_line_strongswan_conf 1 /var/run/charon-1.vici /tmp/charon-1.log)"
    [ "$conf_path" = "$RENDER_HELPERS_ROOT/etc/strongswan-line-1.conf" ]
    grep -q 'socket = unix:///var/run/charon-1.vici' "$conf_path"
    grep -q 'path = /tmp/charon-1.log' "$conf_path"
    grep -q 'line1 {' "$conf_path"
    # swanctl reads its own top-level `swanctl { socket = ... }` block, not
    # charon.plugins.vici.socket — both must be present.
    grep -q 'include /etc/strongswan.d/charon-extra.conf' "$conf_path"
}

@test "render_line_strongswan_conf: does not set charon.pidfile (charon ignores it; rm -f is the real fix)" {
    conf_path="$(render_line_strongswan_conf 0 /var/run/charon-0.vici /tmp/charon-0.log)"
    [ -f "$conf_path" ]
    ! grep -q 'pidfile' "$conf_path"
}

# --- render_line_swanctl_conf ------------------------------------------------

@test "render_line_swanctl_conf: points at this line's own conf.d, not the shared directory" {
    conf_path="$(render_line_swanctl_conf 2)"
    [ "$conf_path" = "$RENDER_HELPERS_ROOT/etc/swanctl-line-2.conf" ]
    grep -q "include $RENDER_HELPERS_ROOT/etc/swanctl/conf.d-2/\*.conf" "$conf_path"
    [ -d "$RENDER_HELPERS_ROOT/etc/swanctl/conf.d-2" ]
}

@test "render_line_swanctl_conf: two different lines get two different conf.d directories" {
    conf0="$(render_line_swanctl_conf 0)"
    conf1="$(render_line_swanctl_conf 1)"
    [ "$conf0" != "$conf1" ]
    ! grep -q "conf.d-1" "$conf0"
    ! grep -q "conf.d-0" "$conf1"
}

# --- render_line_updown_script -----------------------------------------------

@test "render_line_updown_script: sets this line's own NETNS/STRONGSWAN_TUN_IFACE and execs the shared script" {
    script_path="$(render_line_updown_script 3 ims3 tun23-3)"
    [ "$script_path" = "$RENDER_HELPERS_ROOT/etc/strongswan.d/ims-updown-3.sh" ]
    [ -x "$script_path" ]
    grep -q 'NETNS="ims3" STRONGSWAN_TUN_IFACE="tun23-3" exec /etc/strongswan.d/ims.updown "\$@"' "$script_path"
}

@test "render_line_updown_script: two lines' wrappers set different values, never falling through to line 0's defaults" {
    script0="$(render_line_updown_script 0 ims tun23)"
    script1="$(render_line_updown_script 1 ims1 tun23-1)"
    [ -f "$script0" ]
    [ -f "$script1" ]
    grep -q 'NETNS="ims"' "$script0"
    grep -q 'NETNS="ims1"' "$script1"
    ! grep -q 'NETNS="ims" ' "$script1"
}
