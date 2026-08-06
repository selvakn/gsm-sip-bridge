//! `discover` and `render` — the "resolve the line table once, up front"
//! commands every per-line process reads back rather than re-scanning.

use super::shell_quote;
use crate::cli::Cli;
use crate::config::load_config;
use std::fmt::Write;
use std::process::ExitCode;

/// `gsm-sip-bridge discover` (specs/013-multi-card-vowifi,
/// contracts/discover-cli-contract.md): runs the shared scan + VoWiFi role
/// assignment/line-table resolution exactly once, writes it to `--out` (JSON,
/// consumed by `main()`'s daemon-startup path via
/// `modules::discovery::scan_modules`'s own exclusion read and by
/// `--line`-selecting `vowifi-ims-agent`/`vowifi-status`), and optionally
/// prints `eval`-able shell output.
pub(crate) fn handle_discover_command(args: &crate::cli::DiscoverArgs, cli: &Cli) -> ExitCode {
    let out_path = args
        .out
        .clone()
        .unwrap_or_else(super::vowifi::lines_file_path);

    if args.from_file {
        let resolution =
            crate::vowifi::discovery::read_line_resolution(&out_path).unwrap_or_default();
        if args.shell_env {
            print!("{}", render_discover_shell_env(&resolution));
        }
        return ExitCode::SUCCESS;
    }

    let Some(path) = cli.config.as_deref() else {
        eprintln!("error: --config is required for the discover subcommand");
        return ExitCode::FAILURE;
    };
    let config = match load_config(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let resolution = if !config.vowifi.enabled {
        tracing::info!("[vowifi].enabled is false — discovery still runs for the circuit-switched pool, but no VoWiFi lines are resolved");
        crate::vowifi::discovery::LineResolution::default()
    } else {
        let overrides = crate::vowifi::discovery::effective_line_overrides(&config.vowifi);
        // A device with several AT-capable interfaces means an override's
        // named port isn't necessarily the one the plain first-match probe
        // would settle on (found live-testing an EC200 that answers AT on
        // more than one ttyUSB) — pass every configured port as a
        // preference so probing tries it first on that device.
        let preferred_ports: Vec<std::path::PathBuf> = overrides
            .iter()
            .filter_map(|o| o.modem_port.as_deref().map(std::path::PathBuf::from))
            .collect();
        // The one scan allowed to *repair* an unreadable SIM rather than
        // just report it (specs/027-discover-retry-health): `discover` is
        // one-shot and runs before any line carries traffic, so an
        // `AT+CFUN` cycle here can't interrupt a call the way it could on
        // `scan_modules`' ongoing rescans — see `SimRecovery`.
        let modems = match crate::modules::discovery::scan_all_preferring_with_sim_recovery(
            &preferred_ports,
            crate::modules::discovery::SimRecovery::CfunCycleOnUnreadable,
        ) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("error: modem discovery failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        let assignment = crate::vowifi::discovery::RoleAssignment::from_probed(
            &modems,
            &overrides,
            config.cs.enabled,
        );
        let mut result = crate::vowifi::discovery::resolve_lines(&assignment, &config.vowifi);
        // specs/027-discover-retry-health follow-up: pre-derive whatever
        // identity (imsi/imei/mcc/mnc) each resolved modem line doesn't
        // already have pinned, while this is still the only process
        // touching the modem — see `enrich_resolved_line_identity`'s doc
        // comment for the AT-port race this closes.
        for line in &mut result.lines {
            crate::vowifi::discovery::enrich_resolved_line_identity(line);
        }
        // specs/027-discover-retry-health FR-001: a configured override
        // that matched no probed device at all (never even enumerated on
        // the USB bus) is invisible to `resolve_lines` — it only sees
        // candidates that made it into `assignment.vowifi`. Merge those in
        // too, so every `discover` pass — not just a future retry —
        // reports a missing configured line immediately.
        result
            .failed
            .extend(crate::vowifi::discovery::unmatched_overrides(
                &overrides, &modems,
            ));
        for failed in &result.failed {
            tracing::error!(
                card_id = %failed.card_id,
                reason = %failed.reason,
                "VoWiFi line discovery: modem not usable as a line"
            );
        }
        if result.lines.is_empty() {
            // The spec's clarification: degrade, don't fail — the caller
            // (`supervise::orchestrate`) still starts the circuit-switched daemon.
            tracing::error!(
                "[vowifi].enabled is true but no usable VoWiFi line was discovered; \
                 the VoWiFi subsystem will not start this run"
            );
        }
        crate::vowifi::discovery::LineResolution::from_result(&assignment.vowifi, &result)
    };

    if let Err(e) = write_line_resolution(&out_path, &resolution) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    if args.shell_env {
        print!("{}", render_discover_shell_env(&resolution));
    }
    ExitCode::SUCCESS
}

pub(crate) fn handle_render_command(args: &crate::cli::RenderArgs) -> ExitCode {
    use crate::cli::RenderAsset;
    use crate::supervise::render;

    let rendered = match &args.asset {
        RenderAsset::StrongswanConf {
            vici_socket,
            charon_log,
        } => render::render_strongswan_conf(vici_socket, charon_log),
        RenderAsset::SwanctlTopConf { conf_dir } => render::render_swanctl_top_conf(conf_dir),
        RenderAsset::SwanctlEpdg {
            template_path,
            conn_name,
            imsi,
            mcc,
            mnc,
            epdg_ip,
            if_id,
            updown_script,
            src_addr,
        } => {
            let template = match std::fs::read_to_string(template_path) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("error: could not read {}: {e}", template_path.display());
                    return ExitCode::FAILURE;
                }
            };
            let params = render::SwanctlEpdgParams {
                conn_name,
                imsi,
                mcc,
                mnc,
                epdg_ip,
                if_id,
                updown_script,
                src_addr: src_addr.as_deref(),
            };
            render::render_swanctl_epdg(&template, &params)
        }
        RenderAsset::UpdownScript { netns, tun_iface } => {
            render::render_updown_script(netns, tun_iface)
        }
        RenderAsset::VpcdReaderConf { port } => render::render_vpcd_reader_conf(*port),
    };

    print!("{rendered}");
    ExitCode::SUCCESS
}

fn write_line_resolution(
    path: &std::path::Path,
    resolution: &crate::vowifi::discovery::LineResolution,
) -> Result<(), String> {
    let json = serde_json::to_string_pretty(resolution)
        .map_err(|e| format!("failed to serialize line resolution: {e}"))?;
    std::fs::write(path, json).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

pub fn render_discover_shell_env(resolution: &crate::vowifi::discovery::LineResolution) -> String {
    let mut out = String::new();
    fn arr<T: ToString>(vals: impl Iterator<Item = T>) -> String {
        format!(
            "({})",
            vals.map(|v| shell_quote(&v.to_string()))
                .collect::<Vec<_>>()
                .join(" ")
        )
    }

    let _ = writeln!(&mut out, "LINE_COUNT={}", resolution.lines.len());
    let _ = writeln!(
        &mut out,
        "LINE_CARD_ID={}",
        arr(resolution.lines.iter().map(|l| l.card_id.clone()))
    );
    let _ = writeln!(
        &mut out,
        "LINE_MODEM_PORT={}",
        arr(resolution.lines.iter().map(|l| l.modem_port.clone()))
    );
    let _ = writeln!(
        &mut out,
        "LINE_NETNS={}",
        arr(resolution.lines.iter().map(|l| l.netns.clone()))
    );
    let _ = writeln!(
        &mut out,
        "LINE_CONTROL_PORT={}",
        arr(resolution.lines.iter().map(|l| l.control_port))
    );
    let _ = writeln!(
        &mut out,
        "LINE_VETH_LOCAL_ADDR={}",
        arr(resolution.lines.iter().map(|l| l.veth_local_addr.clone()))
    );
    let _ = writeln!(
        &mut out,
        "LINE_VETH_PEER_ADDR={}",
        arr(resolution.lines.iter().map(|l| l.veth_peer_addr.clone()))
    );
    let _ = writeln!(
        &mut out,
        "LINE_VPCD_PORT={}",
        arr(resolution.lines.iter().map(|l| l.vpcd_port))
    );
    let _ = writeln!(
        &mut out,
        "LINE_STRONGSWAN_IF_ID={}",
        arr(resolution.lines.iter().map(|l| l.strongswan_if_id))
    );
    let _ = writeln!(
        &mut out,
        "LINE_STRONGSWAN_TUN_IFACE={}",
        arr(resolution
            .lines
            .iter()
            .map(|l| l.strongswan_tun_iface.clone()))
    );
    let _ = writeln!(
        &mut out,
        "LINE_PCSCF_SOURCE_PATH={}",
        arr(resolution.lines.iter().map(|l| l.pcscf_source_path.clone()))
    );
    let _ = writeln!(
        &mut out,
        "LINE_VETH_SIP_IFACE={}",
        arr(resolution
            .lines
            .iter()
            .map(|l| l.config.veth_sip_iface.clone()))
    );
    let _ = writeln!(
        &mut out,
        "LINE_VETH_IMS_IFACE={}",
        arr(resolution
            .lines
            .iter()
            .map(|l| l.config.veth_ims_iface.clone()))
    );
    let _ = writeln!(
        &mut out,
        "LINE_MCC={}",
        arr(resolution.lines.iter().map(|l| l.mcc.clone()))
    );
    let _ = writeln!(
        &mut out,
        "LINE_MNC={}",
        arr(resolution.lines.iter().map(|l| l.mnc.clone()))
    );
    let _ = writeln!(
        &mut out,
        "LINE_IMSI={}",
        arr(resolution
            .lines
            .iter()
            .map(|l| l.config.imsi_override.clone().unwrap_or_default()))
    );
    let _ = writeln!(
        &mut out,
        "CS_EXCLUDED_PORTS={}",
        arr(resolution.circuit_switched_excluded_ports.iter().cloned())
    );
    out
}
