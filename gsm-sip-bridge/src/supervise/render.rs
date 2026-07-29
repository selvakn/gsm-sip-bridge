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

/// Renders the **one shared** `strongswan.conf`: the single vici socket and
/// filelog path used by the single charon daemon that serves every line.
///
/// This used to be rendered per line, each with its own vici socket and log,
/// because each line ran its own charon process. That was the bug: charon's
/// socket-default plugin binds the IKE ports with `SO_REUSEADDR` only (never
/// `SO_REUSEPORT`), so N charon processes in one network namespace all
/// wildcard-bind `0.0.0.0:500`/`0.0.0.0:4500` and exactly **one** of them
/// receives every reply. Whichever process the kernel picked won the whole
/// port; the others retransmitted into the void and gave up, which presented
/// as the carrier reporting a perfectly good line as "switched off".
///
/// Observed live 2026-07-29: on one boot line 0 established and line 1 timed
/// out; on the very next restart of the same image the two swapped. The
/// coin-flip across restarts is what identified this as a local collision
/// rather than anything carrier-side.
///
/// Per-line isolation does not depend on per-line *processes* — it comes from
/// each connection's own XFRM `if_id` and its pre-created `tunN` interface
/// inside that line's netns, both of which the kernel keys on independently of
/// which daemon negotiated the SA. See `SharedCharon` in `supervise::engines`.
pub fn render_strongswan_conf(vici_socket: &str, charon_log: &str) -> String {
    let lines: [String; 22] = [
        "charon {".to_string(),
        "    plugins {".to_string(),
        "        include /etc/strongswan.d/charon/*.conf".to_string(),
        "        vici {".to_string(),
        format!("            socket = unix://{vici_socket}"),
        "        }".to_string(),
        "    }".to_string(),
        "    filelog {".to_string(),
        "        shared {".to_string(),
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

/// Renders the swanctl top-level conf: `include <conf_dir>/*.conf`.
///
/// Now points at the **shared** `/etc/swanctl/conf.d/`, into which each line
/// writes its own `epdg-{idx}.conf`. This must stay a directory glob covering
/// every line rather than one file per line: `swanctl --load-all` *unloads*
/// any connection absent from what it just read, so a per-line file would make
/// each line's load evict the previously-loaded lines. Loading the union
/// instead is idempotent and order-independent, which matters because lines
/// start concurrently on their own threads.
pub fn render_swanctl_top_conf(conf_dir: &str) -> String {
    format!("include {conf_dir}/*.conf\n")
}

/// Renders the osmocom fork's P-CSCF plugin config, enabling the config-payload
/// request for each line's connection **by name**.
///
/// This has to be generated rather than shipped static, because the plugin's
/// `enable` block is keyed by connection name and connections are now named per
/// line (`ims0`, `ims1`, ...). The image used to ship a fixed
/// `enable { ims = yes }`, which stopped matching anything the moment the
/// rename landed.
///
/// The failure is silent and looks nothing like its cause: charon simply omits
/// PCSCF4/PCSCF6 from its `CPRQ`, so the carrier never sends a P-CSCF address,
/// so every line establishes a perfectly good tunnel, fails the
/// "established but no P-CSCF" check, tears it down and tries again — forever,
/// about every 30s. Caught live 2026-07-29 by `CPRQ(ADDR ADDR6 DNS DNS6)`
/// appearing where `CPRQ(ADDR ADDR6 DNS DNS6 PCSCF4 PCSCF6)` belonged.
///
/// Callers must pass exactly the names used for the swanctl connections; keep
/// this in step with `orchestrate::vowifi_conn_name`.
pub fn render_pcscf_plugin_conf(conn_names: &[String]) -> String {
    let mut rendered = String::from("p-cscf {\n    load = yes\n\n    enable {\n");
    for name in conn_names {
        rendered.push_str(&format!("        {name} = yes\n"));
    }
    rendered.push_str("    }\n}\n");
    rendered
}

/// Parameters for [`render_swanctl_epdg`] — the per-line ePDG `swanctl.conf`
/// connection block. Mirrors `start_line_strongswan`'s local variables plus
/// the `@SRC_ADDR@` sed-substitution branch.
pub struct SwanctlEpdgParams<'a> {
    /// This line's swanctl connection *and* child name (`ims0`, `ims1`, ...).
    /// Must be unique per line: every line's connection is loaded into one
    /// shared charon, so a shared name would make `--initiate --child` and
    /// `--terminate --ike` hit every line at once and would make the shared
    /// charon log's `<name|N>` prefix ambiguous. Distinct from the template's
    /// literal `remote { id = ims }`, which is a protocol identity.
    pub conn_name: &'a str,
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
        .replace("@CONN_NAME@", params.conn_name)
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
    fn strongswan_conf_shared() {
        insta::assert_snapshot!(render_strongswan_conf(
            "/var/run/charon.vici",
            "/tmp/charon.log"
        ));
    }

    #[test]
    fn strongswan_conf_enables_ike_name_so_the_shared_log_can_be_attributed() {
        // Load-bearing now that every line writes into one charon log:
        // `ike_name = yes` is what emits the `<conn|id>` prefix that
        // `engines::lines_for_conn` filters on to tell one line's events from
        // another's. Drop it and every line reads every other line's
        // establishment and P-CSCF events as its own.
        let rendered = render_strongswan_conf("/var/run/charon.vici", "/tmp/charon.log");
        assert!(rendered.contains("ike_name = yes"));
    }

    #[test]
    fn strongswan_conf_never_sets_pidfile() {
        // charon's own "already running" guard ignores a `pidfile =`
        // directive (verified live, specs/013-multi-card-vowifi) — the
        // real fix is `rm -f /var/run/charon.pid` before each launch, done
        // by the caller, not this function. Asserting its absence here
        // guards against someone "helpfully" adding it back.
        let rendered = render_strongswan_conf("/var/run/charon.vici", "/tmp/charon.log");
        assert!(!rendered.contains("pidfile"));
    }

    #[test]
    fn swanctl_top_conf_points_at_the_shared_conf_dir() {
        insta::assert_snapshot!(render_swanctl_top_conf("/etc/swanctl/conf.d"));
    }

    const EPDG_TEMPLATE: &str = "\
connections {
    @CONN_NAME@ {
        local_addrs = @SRC_ADDR@
        remote_addrs = @EPDG_IP@
        local {
            auth = eap-aka
            id = 0@IMSI@@realm.mnc@MNC@.mcc@MCC@.3gppnetwork.org
        }
        remote {
            id = ims
        }
        children {
            @CONN_NAME@ {
                if_id_in = @IF_ID@
                if_id_out = @IF_ID@
                updown = @UPDOWN@
            }
        }
    }
}
";

    #[test]
    fn pcscf_plugin_conf_enables_every_lines_connection_by_name() {
        let rendered = render_pcscf_plugin_conf(&["ims0".to_string(), "ims1".to_string()]);
        assert!(rendered.contains("load = yes"));
        assert!(rendered.contains("ims0 = yes"));
        assert!(rendered.contains("ims1 = yes"));
        // The name this replaced. Leaving it in would enable a connection that
        // no longer exists while the real ones stayed disabled — which is
        // exactly the silent failure this file is generated to avoid.
        assert!(
            !rendered.contains("\n        ims = yes"),
            "the pre-rename `ims` connection must not be enabled"
        );
    }

    #[test]
    fn swanctl_epdg_names_the_connection_per_line_but_leaves_the_remote_id_literal() {
        // Two different things are spelled "ims" in this template. The
        // connection/child name must become unique per line (one shared charon
        // holds every line's connection), while `remote { id = ims }` is a
        // protocol identity the ePDG matches to select the IMS APN and must
        // stay literal — substituting it would break APN selection.
        let params = SwanctlEpdgParams {
            conn_name: "ims1",
            imsi: "404101234567890",
            mcc: "404",
            mnc: "10",
            epdg_ip: "10.1.2.3",
            if_id: "24",
            updown_script: "/etc/strongswan.d/ims-updown-1.sh",
            src_addr: None,
        };
        let rendered = render_swanctl_epdg(EPDG_TEMPLATE, &params);
        assert!(
            rendered.contains("    ims1 {"),
            "connection must be renamed"
        );
        assert!(
            rendered.contains("            ims1 {"),
            "child must be renamed too, so --initiate --child targets one line"
        );
        assert!(
            rendered.contains("id = ims\n"),
            "the remote protocol identity must stay literal `ims`"
        );
        assert!(!rendered.contains("@CONN_NAME@"));
    }

    #[test]
    fn swanctl_epdg_with_src_addr_keeps_the_local_addrs_line() {
        let params = SwanctlEpdgParams {
            conn_name: "ims0",
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
            conn_name: "ims0",
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
            conn_name: "ims0",
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
