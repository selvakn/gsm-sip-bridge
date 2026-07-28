//! Pure config-asset rendering (specs/021-entrypoint-supervise-rust Phase 1).
//!
//! Each function here is a direct, behavior-preserving port of one heredoc from
//! the pre-refactor `docker/entrypoint.sh` (now `docker/lib/render_helpers.sh`
//! for the strongswan.conf/swanctl.conf/updown-wrapper trio, since Phase 0 —
//! see that file's history). They return the rendered text; callers decide
//! where to write it (via `CommandRunner::write_file`), matching this crate's
//! existing convention of keeping decision logic separate from I/O
//! (`volte::netcfg`'s `*_steps() -> Vec<NetStep>`).
//!
//! Snapshot-tested with `insta`: a wrong substitution or an accidentally
//! dropped directive shows up as a failing snapshot diff in `cargo test`,
//! before any image is built (spec FR-002/FR-003).

/// Renders this line's `strongswan.conf`: its own vici socket and filelog
/// path, so this line's charon instance never shares a vici socket or log
/// file with any other line's. 1:1 port of
/// `docker/lib/render_helpers.sh`'s `render_line_strongswan_conf` — see that
/// file for the full rationale (why no `charon.pidfile` directive, why the
/// `swanctl { socket = ... }` block is required).
pub fn render_strongswan_conf(idx: u32, vici_socket: &str, charon_log: &str) -> String {
    let lines: [String; 22] = [
        "charon {".to_string(),
        "    plugins {".to_string(),
        "        include /etc/strongswan.d/charon/*.conf".to_string(),
        "        vici {".to_string(),
        format!("            socket = unix://{vici_socket}"),
        "        }".to_string(),
        "    }".to_string(),
        "    filelog {".to_string(),
        format!("        line{idx} {{"),
        format!("            path = {charon_log}"),
        "            default = 1".to_string(),
        "            ike = 1".to_string(),
        "            cfg = 1".to_string(),
        "            append = no".to_string(),
        "            flush_line = yes".to_string(),
        "            ike_name = yes".to_string(),
        "            time_format = %Y-%m-%d %H:%M:%S".to_string(),
        "        }".to_string(),
        "    }".to_string(),
        "}".to_string(),
        "swanctl {".to_string(),
        format!("    socket = unix://{vici_socket}"),
    ];
    let mut rendered = lines.join("\n");
    rendered.push_str("\n}\ninclude /etc/strongswan.d/charon-extra.conf\n");
    rendered
}

/// Renders this line's swanctl top-level conf, pointing at this line's own
/// `conf.d-{idx}` directory (never the shared `/etc/swanctl/conf.d/`) so
/// `swanctl --load-all --file <this>` only ever loads this line's "ims"
/// connection into this line's charon. 1:1 port of
/// `render_line_swanctl_conf` minus the `mkdir -p` side effect (the caller
/// creates `conf_dir` via `CommandRunner`, keeping this function pure).
pub fn render_swanctl_top_conf(conf_dir: &str) -> String {
    format!("include {conf_dir}/*.conf\n")
}

/// Parameters for [`render_swanctl_epdg`] — the per-line ePDG `swanctl.conf`
/// connection block. Mirrors `start_line_strongswan`'s local variables plus
/// the `@SRC_ADDR@` sed-substitution branch.
pub struct SwanctlEpdgParams<'a> {
    pub imsi: &'a str,
    pub mcc: &'a str,
    pub mnc: &'a str,
    pub epdg_ip: &'a str,
    pub if_id: &'a str,
    pub updown_script: &'a str,
    /// `None` omits the `local_addrs` line entirely — the original script's
    /// `sed -e "/local_addrs.*@SRC_ADDR@/d"` deletion branch (spec Acceptance
    /// Scenario 2).
    pub src_addr: Option<&'a str>,
}

/// Renders the per-line ePDG `swanctl.conf` connection block by substituting
/// into the shared template (`docker/strongswan/swanctl-epdg.conf.template`,
/// unchanged by this feature — still the single source of the *shape* of an
/// "ims" connection; this function 1:1-ports the `sed` substitutions
/// `start_line_strongswan` used to apply to it).
pub fn render_swanctl_epdg(template: &str, params: &SwanctlEpdgParams<'_>) -> String {
    let mut rendered = template
        .replace("@IMSI@", params.imsi)
        .replace("@MCC@", params.mcc)
        .replace("@MNC@", params.mnc)
        .replace("@EPDG_IP@", params.epdg_ip)
        .replace("@IF_ID@", params.if_id)
        .replace("@UPDOWN@", params.updown_script);

    rendered = match params.src_addr {
        Some(addr) => rendered.replace("@SRC_ADDR@", addr),
        None => {
            // 1:1 port of `sed -e "/local_addrs.*@SRC_ADDR@/d"` — note that
            // regex is order-sensitive: it only deletes a line where
            // "local_addrs" appears, FOLLOWED (anywhere later on that same
            // line) by "@SRC_ADDR@". A naive "both substrings present"
            // check is wrong: this template's own header comment has a line
            // mentioning "@SRC_ADDR@" *before* it explains "the local_addrs
            // line" — `local_addrs` after `@SRC_ADDR@` there, so sed's `.*`
            // (which only matches forward) does NOT delete it, and neither
            // must this port (caught by diffing against the real `sed`
            // pipeline on the actual template, not just a hand-written
            // fixture).
            let should_delete = |line: &str| {
                line.find("local_addrs")
                    .is_some_and(|la_pos| line[la_pos..].contains("@SRC_ADDR@"))
            };
            rendered
                .lines()
                .filter(|line| !should_delete(line))
                .collect::<Vec<_>>()
                .join("\n")
                + if rendered.ends_with('\n') { "\n" } else { "" }
        }
    };

    rendered
}

/// Renders this line's strongSwan `updown` wrapper: sets `NETNS`/
/// `STRONGSWAN_TUN_IFACE` to this line's own values, then execs the shared
/// `ims.updown` script unchanged — so the verb-handling logic itself still
/// lives in exactly one place. 1:1 port of `render_line_updown_script`.
pub fn render_updown_script(netns: &str, tun_iface: &str) -> String {
    format!(
        "#!/bin/sh\n\
         NETNS=\"{netns}\" STRONGSWAN_TUN_IFACE=\"{tun_iface}\" exec /etc/strongswan.d/ims.updown \"$@\"\n"
    )
}

/// Renders `/etc/reader.conf.d/vpcd`, pcscd's driver config for the shared
/// vpcd reader every strongswan-engine line's `vowifi-usim-bridge` connects
/// to (one shared pcscd, N slots from `port`..`port+7` — see
/// `docker/entrypoint.sh`'s "One shared pcscd for every strongswan-engine
/// line" section). 1:1 port of the entrypoint's `cat >/etc/reader.conf.d/vpcd`
/// heredoc.
pub fn render_vpcd_reader_conf(port: u16) -> String {
    let port_hex = format!("0x{port:04X}");
    format!(
        "FRIENDLYNAME \"Virtual PCD\"\n\
         DEVICENAME   /dev/null:{port_hex}\n\
         LIBPATH      /usr/lib/pcsc/drivers/serial/libifdvpcd.so\n\
         CHANNELID    {port_hex}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strongswan_conf_line_0() {
        insta::assert_snapshot!(render_strongswan_conf(
            0,
            "/var/run/charon-0.vici",
            "/tmp/charon-0.log"
        ));
    }

    #[test]
    fn strongswan_conf_line_1_has_its_own_socket_and_log_never_line_0s() {
        let rendered = render_strongswan_conf(1, "/var/run/charon-1.vici", "/tmp/charon-1.log");
        insta::assert_snapshot!(rendered);
        assert!(!rendered.contains("charon-0"));
    }

    #[test]
    fn strongswan_conf_never_sets_pidfile() {
        // charon's own "already running" guard ignores a `pidfile =`
        // directive (verified live, specs/013-multi-card-vowifi) — the
        // real fix is `rm -f /var/run/charon.pid` before each launch, done
        // by the caller, not this function. Asserting its absence here
        // guards against someone "helpfully" adding it back.
        let rendered = render_strongswan_conf(0, "/var/run/charon-0.vici", "/tmp/charon-0.log");
        assert!(!rendered.contains("pidfile"));
    }

    #[test]
    fn swanctl_top_conf_points_at_this_lines_own_conf_dir() {
        insta::assert_snapshot!(render_swanctl_top_conf("/etc/swanctl/conf.d-2"));
    }

    const EPDG_TEMPLATE: &str = "\
connections {
    ims {
        local_addrs = @SRC_ADDR@
        remote_addrs = @EPDG_IP@
        local {
            auth = eap-aka
            id = 0@IMSI@@realm.mnc@MNC@.mcc@MCC@.3gppnetwork.org
        }
        children {
            ims {
                if_id_in = @IF_ID@
                if_id_out = @IF_ID@
                updown = @UPDOWN@
            }
        }
    }
}
";

    #[test]
    fn swanctl_epdg_with_src_addr_keeps_the_local_addrs_line() {
        let params = SwanctlEpdgParams {
            imsi: "404101234567890",
            mcc: "404",
            mnc: "10",
            epdg_ip: "10.1.2.3",
            if_id: "23",
            updown_script: "/etc/strongswan.d/ims-updown-0.sh",
            src_addr: Some("192.168.1.50"),
        };
        let rendered = render_swanctl_epdg(EPDG_TEMPLATE, &params);
        insta::assert_snapshot!(rendered);
        assert!(rendered.contains("local_addrs = 192.168.1.50"));
    }

    #[test]
    fn swanctl_epdg_without_src_addr_does_not_delete_a_comment_that_mentions_src_addr_before_local_addrs(
    ) {
        // Regression test for a real bug found by diffing this port against
        // the actual `sed -e "/local_addrs.*@SRC_ADDR@/d"` pipeline on the
        // real template: sed's pattern is order-sensitive (`local_addrs`
        // must appear BEFORE `@SRC_ADDR@` on the line to match), so a
        // comment line mentioning `@SRC_ADDR@` first and "local_addrs"
        // later must survive, not be deleted by an unordered
        // "both substrings present" check.
        let template = "\
before
#   @SRC_ADDR@ - optional; entrypoint deletes the local_addrs line
        local_addrs = @SRC_ADDR@
after
";
        let params = SwanctlEpdgParams {
            imsi: "1",
            mcc: "1",
            mnc: "1",
            epdg_ip: "1",
            if_id: "1",
            updown_script: "1",
            src_addr: None,
        };
        let rendered = render_swanctl_epdg(template, &params);
        assert!(rendered.contains("optional; entrypoint deletes the local_addrs line"));
        assert!(!rendered.contains("local_addrs = "));
    }

    #[test]
    fn swanctl_epdg_without_src_addr_omits_the_local_addrs_line_entirely() {
        // spec Acceptance Scenario 2: no source address configured -> the
        // whole `local_addrs ... @SRC_ADDR@` line is deleted, matching the
        // current script's `sed -e "/local_addrs.*@SRC_ADDR@/d"` branch —
        // not left with a dangling `@SRC_ADDR@` or an empty value.
        let params = SwanctlEpdgParams {
            imsi: "404101234567890",
            mcc: "404",
            mnc: "10",
            epdg_ip: "10.1.2.3",
            if_id: "23",
            updown_script: "/etc/strongswan.d/ims-updown-0.sh",
            src_addr: None,
        };
        let rendered = render_swanctl_epdg(EPDG_TEMPLATE, &params);
        insta::assert_snapshot!(rendered);
        assert!(!rendered.contains("local_addrs"));
        assert!(!rendered.contains("@SRC_ADDR@"));
    }

    #[test]
    fn updown_script_line_0_and_line_1_never_share_netns_or_iface() {
        let line0 = render_updown_script("ims", "tun23");
        let line1 = render_updown_script("ims1", "tun23-1");
        insta::assert_snapshot!("updown_line0", line0);
        insta::assert_snapshot!("updown_line1", line1);
        assert!(line0.contains(r#"NETNS="ims""#));
        assert!(line1.contains(r#"NETNS="ims1""#));
        // Regression guard for the bug this wrapper exists to fix: without
        // per-line NETNS/STRONGSWAN_TUN_IFACE, every line fell through to
        // "ims"/"tun23" (line 0's values) — found live-testing a genuine
        // second line for the first time (specs/013-multi-card-vowifi).
        assert!(!line1.contains(r#"NETNS="ims" "#));
    }

    #[test]
    fn vpcd_reader_conf_default_port() {
        insta::assert_snapshot!(render_vpcd_reader_conf(15963));
    }

    #[test]
    fn vpcd_reader_conf_custom_port_moves_every_slots_channel_id_together() {
        let rendered = render_vpcd_reader_conf(20000);
        insta::assert_snapshot!(rendered);
        assert!(rendered.contains("0x4E20"));
    }
}
