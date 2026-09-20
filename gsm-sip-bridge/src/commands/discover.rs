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
        match resolve_vowifi_lines(&config) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        }
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

/// The scan + VoWiFi role assignment/line-table resolution `discover` runs
/// when `[vowifi].enabled`. Extracted (specs/080-volte-pcscf-auto-prime) so
/// the transient priming capture (`supervise::orchestrate_prime`) can call it
/// directly, in-process, bypassing the `[vowifi].enabled` gate above — that
/// gate lives here, in the CLI wrapper, not in the resolution logic itself,
/// and priming always runs in exactly the situation where `[vowifi].enabled`
/// is false (the mutual-exclusion check in `supervise::orchestrate::run`
/// guarantees it whenever `[volte].enabled` is being started at all), so a
/// caller going through the real `discover` subcommand can never get a line
/// out of it — confirmed live on the Vodafone rig: `discover` logged "no
/// VoWiFi lines are resolved" and priming failed with "no usable modem/SIM
/// found" every cycle, because it was going through this exact gate.
pub fn resolve_vowifi_lines(
    config: &crate::config::AppConfig,
) -> Result<crate::vowifi::discovery::LineResolution, String> {
    let modems = scan_for_line_resolution(config)?;
    Ok(resolve_vowifi_lines_from_modems(
        &modems,
        config,
        config.cs.enabled,
    ))
}

/// The shared modem scan behind [`resolve_vowifi_lines`] and
/// `orchestrate_prime::discover_priming_line` (specs/080-volte-pcscf-auto-prime)
/// — a modem is only ever scanned once per resolution, regardless of how many
/// role assignments (VoWiFi's, or priming's own VoLTE-target one) get derived
/// from the result afterward.
pub fn scan_for_line_resolution(
    config: &crate::config::AppConfig,
) -> Result<Vec<crate::modules::discovery::ProbedModem>, String> {
    let overrides = crate::vowifi::discovery::effective_line_overrides(&config.vowifi);
    // A device with several AT-capable interfaces means an override's
    // named port isn't necessarily the one the plain first-match probe
    // would settle on (found live-testing an EC200 that answers AT on
    // more than one ttyUSB) — pass every configured port as a
    // preference so probing tries it first on that device.
    let preferred_ports: Vec<std::path::PathBuf> = overrides
        .iter()
        .filter_map(|o| o.modem_port.as_deref().map(std::path::PathBuf::from))
        .chain(
            config
                .volte
                .line_overrides
                .iter()
                .filter_map(|o| o.modem_port.as_deref().map(std::path::PathBuf::from)),
        )
        .collect();
    // The one scan allowed to *repair* an unreadable SIM rather than
    // just report it (specs/027-discover-retry-health): `discover` is
    // one-shot and runs before any line carries traffic, so an
    // `AT+CFUN` cycle here can't interrupt a call the way it could on
    // `scan_modules`' ongoing rescans — see `SimRecovery`.
    let mut policy = crate::modules::discovery::DiscoveryPolicy::new(config.discovery.clone());
    crate::modules::discovery::scan_all_preferring_with_sim_recovery(
        &preferred_ports,
        crate::modules::discovery::SimRecovery::CfunCycleOnUnreadable,
        &mut policy,
    )
    .map_err(|e| format!("modem discovery failed: {e}"))
}

/// The post-scan half of [`resolve_vowifi_lines`], parameterized on
/// `cs_enabled` rather than always reading it from `config.cs.enabled`:
/// priming (specs/080-volte-pcscf-auto-prime) needs to resolve the *same*
/// modem `[volte]` itself would pick as a VoWiFi-shaped line for the transient
/// capture tunnel, and must not let `[cs].enabled` (true by default) exclude
/// it — priming tears its tunnel all the way down before any real line
/// starts, so it never actually contends with a real circuit-switched
/// reservation the way a persistent `[vowifi]` line would.
pub fn resolve_vowifi_lines_from_modems(
    modems: &[crate::modules::discovery::ProbedModem],
    config: &crate::config::AppConfig,
    cs_enabled: bool,
) -> crate::vowifi::discovery::LineResolution {
    let overrides = crate::vowifi::discovery::effective_line_overrides(&config.vowifi);
    let assignment =
        crate::vowifi::discovery::RoleAssignment::from_probed(modems, &overrides, cs_enabled);
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
            &overrides, modems,
        ));
    for failed in &result.failed {
        tracing::error!(
            card_id = %failed.card_id,
            reason = %failed.reason,
            "VoWiFi line discovery: modem not usable as a line"
        );
    }
    if result.lines.is_empty() {
        // The spec's clarification: degrade, don't fail — the caller decides
        // what "no usable line" means for it (a persistent [vowifi] run
        // skips the subsystem; a priming attempt reports a failure and
        // relies on its own retry cadence).
        tracing::error!("no usable VoWiFi-capable line was discovered from this scan");
    }
    crate::vowifi::discovery::LineResolution::from_result(&assignment.vowifi, &result)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::discovery::{ProbedModem, SimStatus};
    use std::path::PathBuf;

    fn test_config() -> crate::config::AppConfig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[sip]\nserver = \"sip.example.com\"\nusername = \"user\"\npassword = \"pass\"\n",
        )
        .unwrap();
        load_config(&path).unwrap()
    }

    fn ready_audio_modem(card_id: &str, port: &str) -> ProbedModem {
        ProbedModem {
            card_id: card_id.to_string(),
            model: "EC20",
            usb_serial: card_id.to_string(),
            has_audio_capability: true,
            audio_device: None,
            net_device: None,
            at_port: Some(PathBuf::from(port)),
            sim_status: Some(SimStatus::Ready {
                imsi: "1".to_string(),
            }),
        }
    }

    /// Greptile PR #87 review, finding 1: on the single most common VoLTE
    /// deployment shape — one audio-capable modem, `[cs].enabled` left at its
    /// default `true`, no `[vowifi]` overrides at all — VoWiFi's own resolver
    /// (`cs_enabled = true`) must exclude the modem (it's reserved for the
    /// circuit-switched pool), while priming's forced `cs_enabled = false`
    /// call must still resolve it, and to the very same `card_id`
    /// `resolve_volte_lines` picks as VoLTE's own line 0.
    #[test]
    fn forcing_cs_enabled_false_recovers_the_modem_the_real_resolver_excludes() {
        let modems = vec![ready_audio_modem("ec20-AAAAAA", "/dev/ttyUSB0")];
        let mut config = test_config();
        config.cs.enabled = true;
        config.volte.enabled = true;

        let excluded = resolve_vowifi_lines_from_modems(&modems, &config, true);
        assert!(
            excluded.lines.is_empty(),
            "cs_enabled=true must still reserve the only modem for the CS pool"
        );

        let recovered = resolve_vowifi_lines_from_modems(&modems, &config, false);
        assert_eq!(recovered.lines.len(), 1);

        let volte_line = crate::volte::discovery::resolve_volte_lines(&modems, &config.volte)
            .lines
            .into_iter()
            .next()
            .expect("VoLTE must pick this modem as its own line 0");
        assert_eq!(recovered.lines[0].card_id, volte_line.card_id);
    }

    /// Mixed-modem case: priming must target the exact modem VoLTE picked as
    /// line 0, not merely "any modem VoWiFi could see once cs_enabled is
    /// forced off" — the first candidate in card-id order happens to differ
    /// from VoLTE's pinned choice here, so a naive `.next()` would silently
    /// cache the P-CSCF for the wrong SIM.
    #[test]
    fn recovers_the_same_modem_volte_pinned_even_when_it_sorts_second() {
        let modems = vec![
            ready_audio_modem("ec20-AAAAAA", "/dev/ttyUSB0"),
            ready_audio_modem("ec20-ZZZZZZ", "/dev/ttyUSB1"),
        ];
        let mut config = test_config();
        config.cs.enabled = true;
        config.volte.enabled = true;
        config.volte.line_overrides = vec![crate::config::VolteLineOverride {
            modem_serial: Some("ec20-ZZZZZZ".to_string()),
            ..Default::default()
        }];
        config.volte.max_lines = 1;

        let volte_line = crate::volte::discovery::resolve_volte_lines(&modems, &config.volte)
            .lines
            .into_iter()
            .next()
            .expect("the pinned modem must win the single available slot");
        assert_eq!(volte_line.card_id, "ec20-ZZZZZZ");

        let recovered = resolve_vowifi_lines_from_modems(&modems, &config, false);
        let matched = recovered
            .lines
            .iter()
            .find(|l| l.card_id == volte_line.card_id);
        assert!(
            matched.is_some(),
            "the vowifi-shaped resolution must still contain VoLTE's pinned modem"
        );
    }
}
