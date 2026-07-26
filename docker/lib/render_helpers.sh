#!/usr/bin/env bash
# Pure bash helpers extracted from entrypoint.sh (specs/021-entrypoint-supervise-rust
# Phase 0): the "safety net before the port" step — these functions do no I/O beyond
# reading an already-written log file / writing a rendered config file from their
# arguments, so they can be covered by bats-core (render_helpers.bats) before any of
# their logic moves to Rust (Phase 1). Sourced by entrypoint.sh; not meant to be run
# standalone.

# Overridable root for the render_line_* functions' rendered-file paths, default
# empty so production behavior (writing to the real /etc/...) is byte-for-byte
# unchanged — bats tests set this to a tmpdir so the functions can be exercised
# without root/without touching the real filesystem (specs/021-entrypoint-supervise-rust
# Phase 0; this sandbox has no root to write to /etc with).
RENDER_HELPERS_ROOT="${RENDER_HELPERS_ROOT:-}"

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

# Renders a per-line strongswan.conf: its own vici socket and filelog path —
# so this line's charon instance is fully independent of every other line's,
# never sharing a vici socket or log file with them. Launched via
# `STRONGSWAN_CONF="$conf" charon`/`STRONGSWAN_CONF="$conf" swanctl ...`
# (both charon and swanctl load their settings — including the vici socket
# — through this same file/env var; verified against the actual pinned
# source, src/swanctl/command.c's `command_dispatch()`: it reads
# `swanctl.socket`/`swanctl.plugins.vici.socket` from `lib->settings`
# *before* parsing any CLI flags, which is why the `swanctl { socket = ... }`
# block below exists — swanctl does NOT read `charon.plugins.vici.socket`).
#
# Deliberately does NOT set `charon.pidfile` here to get per-line pidfiles:
# tried that first, and it does not work — the raw `charon` binary's
# "already running" startup guard checks the unqualified `/var/run/charon.pid`
# regardless of this directive (verified live), and there is no `--pid-file`
# CLI flag either. `start_line_strongswan`'s `rm -f /var/run/charon.pid`
# immediately before each launch is what actually fixes the second-line-onward
# refusal (specs/013-multi-card-vowifi, found live-testing a genuine 2-line
# strongswan deployment for the first time).
render_line_strongswan_conf() {
    local idx="$1" vici_socket="$2" charon_log="$3"
    local conf="${RENDER_HELPERS_ROOT}/etc/strongswan-line-$idx.conf"
    cat >"$conf" <<EOF
charon {
    plugins {
        include /etc/strongswan.d/charon/*.conf
        vici {
            socket = unix://$vici_socket
        }
    }
    filelog {
        line$idx {
            path = $charon_log
            default = 1
            ike = 1
            cfg = 1
            append = no
            flush_line = yes
            ike_name = yes
            time_format = %Y-%m-%d %H:%M:%S
        }
    }
}
swanctl {
    socket = unix://$vici_socket
}
include /etc/strongswan.d/charon-extra.conf
EOF
    echo "$conf"
}

# Renders a per-line swanctl.conf pointing at this line's own conf.d
# directory (never the shared /etc/swanctl/conf.d/) so `swanctl --load-all
# --file <this>` only ever loads *this* line's "ims" connection into *this*
# line's charon — sharing one directory across lines would load every
# line's same-named "ims" connection into every charon instance. `--file`
# is `--load-all`'s own option (src/swanctl/commands/load_all.c: `-f,
# --file "custom path to swanctl.conf"`) — verified against the pinned
# source, and must come *after* `--load-all` on the command line (swanctl's
# top-level `getopt_long` pass only recognizes registered command names
# until one matches; a global/per-command flag given before the command
# name comes back "unrecognized option" — found live-testing).
render_line_swanctl_conf() {
    local idx="$1"
    local conf_dir="${RENDER_HELPERS_ROOT}/etc/swanctl/conf.d-$idx"
    local conf="${RENDER_HELPERS_ROOT}/etc/swanctl-line-$idx.conf"
    mkdir -p "$conf_dir"
    echo "include $conf_dir/*.conf" >"$conf"
    echo "$conf"
}

# Renders this line's updown wrapper: sets NETNS/STRONGSWAN_TUN_IFACE (which
# the shared /etc/strongswan.d/ims.updown reads to know which netns/interface
# to install the carrier-assigned address on — see that script's own
# comments) to this line's own values, then execs the shared script
# unchanged, so the verb-handling logic itself still lives in exactly one
# place.
#
# A wrapper, not an env-var export on the `charon` launch line itself: tried
# that first, and it does not work — charon does not propagate its own
# launch environment down into the updown program it execs on CHILD_SA
# up/down (verified live), so every line fell through to the script's
# defaults ("ims"/"tun23", i.e. line 0's values) regardless. A script that
# sets the vars and execs the next program *is* that program's environment,
# no propagation required.
render_line_updown_script() {
    local idx="$1" netns="$2" tun_iface="$3"
    local script="${RENDER_HELPERS_ROOT}/etc/strongswan.d/ims-updown-$idx.sh"
    cat >"$script" <<EOF
#!/bin/sh
NETNS="$netns" STRONGSWAN_TUN_IFACE="$tun_iface" exec /etc/strongswan.d/ims.updown "\$@"
EOF
    chmod +x "$script"
    echo "$script"
}
