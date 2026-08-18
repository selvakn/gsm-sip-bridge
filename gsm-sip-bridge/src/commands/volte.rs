//! The host-side VoLTE subcommands: PDN attach/status, registration, calls,
//! line discovery, the carrier agent, and per-line cleanup.

use super::shell_quote;
use crate::config::load_config;
use std::fmt::Write;
use std::process::ExitCode;

fn volte_settings(
    modem: &std::path::Path,
    iface: &Option<String>,
    cid: u8,
    apn: &str,
) -> crate::volte::VolteSettings {
    crate::volte::VolteSettings {
        modem_port: modem.to_path_buf(),
        iface: iface.clone().unwrap_or_default(),
        cid,
        apn: apn.to_string(),
        pcscf: None,
        restore_cid_path: None,
    }
}

pub(crate) fn handle_volte_pdn_command(
    args: &crate::cli::VoltePdnArgs,
    config_path: Option<&std::path::Path>,
) -> ExitCode {
    use crate::cli::VoltePdnAction;

    let line = match &args.modem {
        Some(modem) => ResolvedVolteRegisterLine {
            modem: modem.clone(),
            cid: args.cid,
            apn: args.apn.clone(),
            iface: args.iface.clone(),
            pcscf: None,
            msisdn: None,
        },
        None => match resolve_volte_register_line(config_path) {
            Ok(l) => l,
            Err(msg) => {
                eprintln!("volte-pdn: {msg}");
                return ExitCode::FAILURE;
            }
        },
    };
    let settings = volte_settings(&line.modem, &line.iface, line.cid, &line.apn);

    match args.action {
        VoltePdnAction::Up => match crate::volte::attach(&settings) {
            Ok(report) => {
                print!("{}", report.summary());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("volte-pdn: failed to attach the IMS PDN: {e}");
                ExitCode::FAILURE
            }
        },
        VoltePdnAction::Down => match crate::volte::detach(&settings, args.restore_cid) {
            Ok(()) => {
                println!("IMS PDN released (context {}).", line.cid);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("volte-pdn: failed to release the IMS PDN: {e}");
                ExitCode::FAILURE
            }
        },
        // `status` exits 0 whether or not a PDN exists: the state belongs in
        // the output, not the exit code.
        VoltePdnAction::Status => match crate::volte::status(&settings) {
            Ok(Some(report)) => {
                print!("{}", report.summary());
                ExitCode::SUCCESS
            }
            Ok(None) => {
                println!("No IMS PDN attached on context {}.", line.cid);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("volte-pdn: failed to read IMS PDN state: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

pub(crate) fn handle_volte_status_command(args: &crate::cli::VolteStatusArgs) -> ExitCode {
    // Ask the running service first. It owns the modem's AT port exclusively
    // (research R6), so reading the modem directly while it runs races it
    // mid-transaction — and the live service knows things the modem cannot,
    // like whether a call is in progress right now (FR-033). Only when no
    // service answers is a direct modem read both safe and necessary.
    if crate::volte::bridge::print_live_status() {
        return ExitCode::SUCCESS;
    }

    let settings = volte_settings(&args.modem, &args.iface, args.cid, &args.apn);
    match crate::volte::status(&settings) {
        Ok(Some(report)) => {
            print!("{}", report.summary());
            let status = crate::volte::registration::read_status(&args.status_path);
            print!(
                "{}",
                crate::volte::registration::status_summary(status.as_ref())
            );
            ExitCode::SUCCESS
        }
        Ok(None) => {
            println!("No IMS PDN attached on context {}.", args.cid);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("volte-status: {e}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn handle_volte_listen_command(args: &crate::cli::VolteListenArgs) -> ExitCode {
    use std::time::Duration;

    if let Err(e) = crate::volte::guard::check_no_vowifi_conflict(args.force) {
        eprintln!("volte-listen: {e}");
        return ExitCode::FAILURE;
    }
    let _lock = match crate::volte::guard::RegistrationGuard::acquire(&args.lock_path) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("volte-listen: {e}");
            return ExitCode::FAILURE;
        }
    };

    let (pcscf_addr, _src) = match args.pcscf {
        Some(a) => (a, "--pcscf".to_string()),
        None => {
            let cache = std::path::PathBuf::from(&args.pcscf_source_path);
            match crate::volte::pcscf::probe_epdg_cache(&cache).found() {
                Some(a) => (a, format!("ePDG capture at {}", cache.display())),
                None => {
                    eprintln!("volte-listen: [discovering-pcscf] no P-CSCF address available; pass --pcscf");
                    return ExitCode::FAILURE;
                }
            }
        }
    };

    let settings = crate::volte::VolteSettings {
        modem_port: args.modem.clone(),
        iface: args.iface.clone().unwrap_or_default(),
        cid: args.cid,
        apn: args.apn.clone(),
        pcscf: Some(std::net::SocketAddr::new(pcscf_addr, args.pcscf_port)),
        // This command runs its own detach, so it never needs the recorded cid.
        restore_cid_path: None,
    };
    let attach = match crate::volte::attach(&settings) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("volte-listen: [attaching] {e}");
            return ExitCode::FAILURE;
        }
    };

    let plmn = match crate::modules::at_commander::AtCommander::open(&args.modem)
        .and_then(|mut at| crate::vowifi::plmn::derive_plmn(&mut at))
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("volte-listen: [attaching] could not derive the home PLMN: {e}");
            return ExitCode::FAILURE;
        }
    };

    let reg_cfg = crate::ims::ImsRegisterConfig {
        modem_port: args.modem.clone(),
        pcsc_reader: false,
        pcscf_addr,
        pcscf_port: args.pcscf_port,
        mcc: plmn.mcc,
        mnc: plmn.mnc,
        imsi: None,
        imei: None,
        use_tcp: true,
        sec_agree: true,
        msisdn: args.msisdn.clone(),
        access_network_info: crate::volte::read_access_network_info(&args.modem),
        register_uri_home_domain: false,
        gm_auth_alg: None,
        gm_cipher_alg: None,
    };

    println!(
        "Registering, then listening {}s for anything the network delivers.\n\
         DIAL THE SIM NOW — the call will be declined with a busy response, not answered.",
        args.listen_secs
    );

    let result = crate::ims::agent::probe_inbound(&reg_cfg, Duration::from_secs(args.listen_secs));

    if !args.keep_pdn {
        if let Err(e) = crate::volte::detach(&settings, attach.displaced_cid) {
            tracing::warn!(error = %e, "failed to release the IMS PDN");
        }
    }

    match result {
        Ok(report) => {
            println!("\ninbound probe report");
            println!(
                "  port reachable : {}",
                if report.port_proven_reachable {
                    "YES — the network delivered something to us"
                } else {
                    "UNPROVEN — nothing arrived at all"
                }
            );
            println!("  incoming calls : {}", report.invites);
            println!("  other requests : {}", report.other_requests);
            for entry in &report.log {
                println!("    - {entry}");
            }
            if report.invites > 0 {
                println!("\nThe carrier DOES route incoming calls to us over this registration.");
                ExitCode::SUCCESS
            } else {
                if report.port_proven_reachable {
                    println!(
                        "\nThe network CAN reach us — something was delivered — but no \
                         incoming call arrived. If the SIM was dialled during the window, \
                         the carrier is not routing calls to this registration."
                    );
                } else {
                    println!(
                        "\nNothing arrived at all, so this run proves nothing: it cannot \
                         distinguish 'the carrier does not route calls here' from 'our \
                         protected port is unreachable'. Investigate reachability before \
                         concluding anything about incoming calls."
                    );
                }
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("volte-listen: {e}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn handle_volte_call_command(args: &crate::cli::VolteCallArgs) -> ExitCode {
    use crate::ims::call::{run_call, CallConfig, CallOutcome, EchoSettings};
    use std::time::Duration;

    // Refuse before anything touches the modem, so a refusal leaves the system
    // exactly as it was (FR-022).
    if let Err(e) = crate::volte::guard::check_no_vowifi_conflict(args.force) {
        eprintln!("volte-call: {e}");
        return ExitCode::FAILURE;
    }
    let _lock = match crate::volte::guard::RegistrationGuard::acquire(&args.lock_path) {
        Ok(g) => g,
        Err(e) => {
            eprintln!(
                "volte-call: {e}. The call places its own registration, so it cannot run \
                 alongside volte-register — stop the registration loop, run the call, then \
                 restart it."
            );
            return ExitCode::FAILURE;
        }
    };

    // A quality judgement made on a narrowband fallback is meaningless, so
    // find out before dialling rather than from a rejection (FR-010).
    if !amr_safe::is_available() {
        eprintln!(
            "volte-call: [preparing] this build has no wideband codec linked, so only a \
             narrowband offer could be made and any quality judgement would be meaningless. \
             Run the container build."
        );
        return ExitCode::FAILURE;
    }

    let (pcscf_addr, pcscf_source) = match args.pcscf {
        Some(addr) => (addr, "--pcscf".to_string()),
        None => {
            let cache = std::path::PathBuf::from(&args.pcscf_source_path);
            match crate::volte::pcscf::probe_epdg_cache(&cache).found() {
                Some(addr) => (addr, format!("ePDG capture at {}", cache.display())),
                None => {
                    eprintln!(
                        "volte-call: [discovering-pcscf] no P-CSCF address available. Pass \
                         --pcscf, or run the VoWiFi path once so it writes one to {}.",
                        cache.display()
                    );
                    return ExitCode::FAILURE;
                }
            }
        }
    };
    tracing::info!(pcscf = %pcscf_addr, source = %pcscf_source, "resolved P-CSCF");

    let settings = crate::volte::VolteSettings {
        modem_port: args.modem.clone(),
        iface: args.iface.clone().unwrap_or_default(),
        cid: args.cid,
        apn: args.apn.clone(),
        pcscf: Some(std::net::SocketAddr::new(pcscf_addr, args.pcscf_port)),
        // This command runs its own detach, so it never needs the recorded cid.
        restore_cid_path: None,
    };

    let attach = match crate::volte::attach(&settings) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("volte-call: [attaching] {e}");
            return ExitCode::FAILURE;
        }
    };
    if !attach.routed && !settings.iface.is_empty() {
        eprintln!("volte-call: [attaching] the IMS PDN has no default route; media cannot flow");
        return ExitCode::FAILURE;
    }

    let plmn = match crate::modules::at_commander::AtCommander::open(&args.modem)
        .and_then(|mut at| crate::vowifi::plmn::derive_plmn(&mut at))
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("volte-call: [attaching] could not derive the home PLMN: {e}");
            return ExitCode::FAILURE;
        }
    };

    let cfg = CallConfig {
        register: crate::ims::ImsRegisterConfig {
            modem_port: args.modem.clone(),
            pcsc_reader: false,
            pcscf_addr,
            pcscf_port: args.pcscf_port,
            mcc: plmn.mcc.clone(),
            mnc: plmn.mnc.clone(),
            imsi: None,
            imei: None,
            use_tcp: true,
            sec_agree: true,
            msisdn: args.msisdn.clone(),
            access_network_info: crate::volte::read_access_network_info(&args.modem),
            register_uri_home_domain: false,
            gm_auth_alg: None,
            gm_cipher_alg: None,
        },
        callee: args.callee.clone(),
        record_path: args.record.clone(),
        record_sent_path: Some(args.record_sent.clone()),
        ring_timeout: Duration::from_secs(args.ring_timeout_secs),
        call_duration: Duration::from_secs(args.duration_secs),
        echo: Some(EchoSettings {
            attenuation: args.echo_attenuation,
            marker_interval: Duration::from_secs(args.marker_interval_secs),
        }),
        one_way_threshold_percent: args.one_way_threshold,
        // Wideband first. Offering narrowband first is what made the first
        // live call negotiate PCMU and rendered its quality result meaningless.
        codec_offer: crate::ims::sdp::CodecOffer::preferring_wideband(amr_safe::is_available()),
    };

    println!(
        "Placing a call to {}. The answering party will hear their OWN VOICE returned — \
         have them use a handset, not a speakerphone.",
        args.callee
    );

    let result = run_call(&cfg);

    if !args.keep_pdn {
        if let Err(e) = crate::volte::detach(&settings, attach.displaced_cid) {
            tracing::warn!(error = %e, "failed to release the IMS PDN");
        }
    }

    match result {
        Ok(CallOutcome::Answered {
            recorded_path,
            sent_path,
            end_reason,
            media,
            ..
        }) => {
            print!(
                "{}",
                render_call_report(&media, end_reason, &recorded_path, sent_path.as_deref())
            );
            // An answered call whose audio only flowed one way is a failure —
            // the previous one-way-audio incident was painful precisely
            // because a broken call looked like a working one (FR-016).
            if media.is_success() {
                ExitCode::SUCCESS
            } else {
                eprintln!("\nvolte-call: [media] {}", media.verdict.diagnosis());
                ExitCode::FAILURE
            }
        }
        Ok(CallOutcome::NotAnswered { status, reason }) => {
            eprintln!("volte-call: [signalling] the call was not answered: {status} {reason}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("volte-call: [signalling] {e}");
            ExitCode::FAILURE
        }
    }
}

/// Operator-facing media report. Direction is printed first, because it is the
/// line that decides whether the call worked at all.
pub(crate) fn render_call_report(
    media: &crate::ims::call::MediaReport,
    end_reason: crate::ims::call::EndReason,
    recorded: &std::path::Path,
    sent: Option<&std::path::Path>,
) -> String {
    let s = &media.stats;
    let mut out = String::from("\ncall report\n");
    out.push_str(&format!(
        "  direction      : {} — {}\n",
        media.verdict.as_str(),
        media.verdict.diagnosis()
    ));
    out.push_str(&format!("  ended by       : {}\n", end_reason.as_str()));
    out.push_str(&format!(
        "  sent           : {} packets / {} samples\n",
        media.sent_packets, media.sent_samples
    ));
    out.push_str(&format!(
        "  received       : {} packets / {} samples\n",
        s.received_packets, media.received_samples
    ));
    out.push_str(&format!(
        "  loss           : {} ({:.1}%)\n",
        s.lost_packets,
        s.loss_percent()
    ));
    out.push_str(&format!("  reordered      : {}\n", s.reordered_packets));
    out.push_str(&format!(
        "  jitter         : {:.1} ms\n",
        s.jitter.as_secs_f64() * 1000.0
    ));
    match media.round_trip_delay {
        Some(d) => out.push_str(&format!(
            "  round trip     : {:.0} ms\n",
            d.as_secs_f64() * 1000.0
        )),
        None => out.push_str("  round trip     : not measured\n"),
    }
    out.push_str(&format!("  recording      : {}\n", recorded.display()));
    if let Some(p) = sent {
        out.push_str(&format!("  sent audio     : {}\n", p.display()));
    }
    out
}

/// A single line's settings for `volte-register`/`volte-pdn`'s inherently
/// single-line invocations — either taken verbatim from CLI flags (`--modem`
/// given), or resolved from `--config`'s `[[volte.line]]` (`--modem`
/// omitted). `volte-pdn --action down` resolving the exact same line
/// `volte-register` registered (rather than guessing the CLI default) is
/// what lets `supervise::shutdown` tear down the right modem/PDN.
pub(crate) struct ResolvedVolteRegisterLine {
    modem: std::path::PathBuf,
    cid: u8,
    apn: String,
    iface: Option<String>,
    pcscf: Option<String>,
    msisdn: Option<String>,
}

/// Resolves a single line's settings from `--config`'s `[[volte.line]]`: the
/// same SIM-ready-modem scan + resolution `volte-bridge`'s auto-discovery
/// uses (`resolve_volte_lines`), then honors the first `[[volte.line]]`
/// entry's pin if one is configured — not whichever line happened to sort
/// first by card id, which silently ran registration against the wrong
/// modem with default settings whenever more than one SIM-ready modem was
/// present and the pinned one wasn't first alphabetically. Absent any
/// override, the first (arbitrary) line is used, since there's no configured
/// preference to respect. These modes have no manifest/multi-line support,
/// unlike `volte-bridge` — a second `[[volte.line]]` entry is ignored.
/// Requires `--config`; a config with no usable line is a clear error rather
/// than a silent fallback to some default port.
pub(crate) fn resolve_volte_register_line(
    config_path: Option<&std::path::Path>,
) -> Result<ResolvedVolteRegisterLine, String> {
    let path = config_path.ok_or_else(|| {
        "no --modem given and no --config to resolve one from \
         (pass --modem explicitly, or --config a file with [volte])"
            .to_string()
    })?;
    let config = load_config(path).map_err(|e| e.to_string())?;

    // Probe every pinned port first on its device (a modem may answer AT on
    // more than one ttyUSB), mirroring volte-bridge's auto-discovery.
    let preferred: Vec<std::path::PathBuf> = config
        .volte
        .line_overrides
        .iter()
        .filter_map(|o| o.modem_port.as_deref().map(std::path::PathBuf::from))
        .collect();
    let mut policy = crate::modules::discovery::DiscoveryPolicy::new(config.discovery.clone());
    let modems =
        crate::modules::discovery::scan_all_preferring_with_policy(&preferred, &mut policy)
            .map_err(|e| format!("modem discovery failed: {e}"))?;

    let table = crate::volte::discovery::resolve_volte_lines(&modems, &config.volte);
    for failed in &table.failed {
        tracing::error!(
            card_id = %failed.card_id,
            reason = %failed.reason,
            "VoLTE line discovery: modem not usable as a line"
        );
    }

    let line = if let Some(over) = config.volte.line_overrides.first() {
        let target = modems.iter().find(|m| {
            over.modem_serial
                .as_deref()
                .is_some_and(|s| s == m.usb_serial)
                || over.modem_port.as_deref().is_some_and(|p| {
                    m.at_port
                        .as_deref()
                        .is_some_and(|port| port == std::path::Path::new(p))
                })
        });
        let Some(target) = target else {
            return Err(format!(
                "the [[volte.line]] entry (modem_serial={:?}, modem_port={:?}) matched no \
                 discovered modem",
                over.modem_serial, over.modem_port
            ));
        };
        table
            .lines
            .into_iter()
            .find(|l| l.card_id == target.card_id)
    } else {
        table.lines.into_iter().next()
    };
    let line = line.ok_or_else(|| {
        "no usable VoLTE line found (no SIM-ready modem discovered); pass --modem explicitly \
         to bypass discovery"
            .to_string()
    })?;

    Ok(ResolvedVolteRegisterLine {
        modem: line.modem_port,
        cid: line.cid,
        apn: line.apn,
        iface: (!line.iface.is_empty()).then_some(line.iface),
        pcscf: line.pcscf,
        msisdn: line.msisdn,
    })
}

pub(crate) fn handle_volte_register_command(
    args: &crate::cli::VolteRegisterArgs,
    config_path: Option<&std::path::Path>,
) -> ExitCode {
    use crate::ims::RegisterOutcome;

    // Refuse to displace a live VoWiFi registration. Checked before anything
    // touches the modem, so a refusal leaves the system exactly as it was.
    if let Err(e) = crate::volte::guard::check_no_vowifi_conflict(args.force) {
        eprintln!("volte-register: {e}");
        return ExitCode::FAILURE;
    }
    let _lock = match crate::volte::guard::RegistrationGuard::acquire(&args.lock_path) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("volte-register: {e}");
            return ExitCode::FAILURE;
        }
    };

    let line = match &args.modem {
        Some(modem) => ResolvedVolteRegisterLine {
            modem: modem.clone(),
            cid: args.cid,
            apn: args.apn.clone(),
            iface: args.iface.clone(),
            pcscf: args.pcscf.map(|ip| ip.to_string()),
            msisdn: args.msisdn.clone(),
        },
        None => match resolve_volte_register_line(config_path) {
            Ok(l) => l,
            Err(msg) => {
                eprintln!("volte-register: {msg}");
                return ExitCode::FAILURE;
            }
        },
    };

    // P-CSCF resolution order: explicit flag/resolved line, then the address
    // captured by the VoWiFi/ePDG path. Automatic discovery is not consulted
    // here because it is known not to yield an address on the tested
    // carrier and would only add latency before a failure the operator can
    // already act on.
    let (pcscf_addr, pcscf_source) = match line.pcscf.as_deref().and_then(|s| s.parse().ok()) {
        Some(addr) => (addr, "--pcscf / [[volte.line]].pcscf".to_string()),
        None => {
            let cache = std::path::PathBuf::from(&args.pcscf_source_path);
            match crate::volte::pcscf::probe_epdg_cache(&cache).found() {
                Some(addr) => (addr, format!("ePDG capture at {}", cache.display())),
                None => {
                    eprintln!(
                        "volte-register: [discovering-pcscf] no P-CSCF address available. \
                         Pass --pcscf, or run the VoWiFi path once so it writes one to {}. \
                         `volte-discover` reports what each mechanism returned.",
                        cache.display()
                    );
                    return ExitCode::FAILURE;
                }
            }
        }
    };
    tracing::info!(pcscf = %pcscf_addr, source = %pcscf_source, "resolved P-CSCF");

    let settings = crate::volte::VolteSettings {
        modem_port: line.modem.clone(),
        iface: line.iface.clone().unwrap_or_default(),
        cid: line.cid,
        apn: line.apn.clone(),
        pcscf: Some(std::net::SocketAddr::new(pcscf_addr, args.pcscf_port)),
        // With --keep-pdn this process does not detach; an external teardown
        // does, and reads this file to restore the displaced context. Without
        // --keep-pdn it detaches itself and the path is simply unset.
        restore_cid_path: args.restore_cid_path.clone(),
    };

    // Stage 1: the network attachment. Reported separately so a failure here
    // is never mistaken for a credential problem (FR-015).
    let attach = match crate::volte::attach(&settings) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("volte-register: [attaching] {e}");
            return ExitCode::FAILURE;
        }
    };
    print!("{}", attach.summary());
    if !attach.routed && !settings.iface.is_empty() {
        eprintln!(
            "volte-register: [attaching] the IMS PDN is attached but has no default route, \
             so signalling cannot reach the P-CSCF"
        );
        return ExitCode::FAILURE;
    }

    // The IMS realm is built from the home PLMN, so derive it from the SIM
    // exactly as the VoWiFi agent does rather than making the operator pass
    // it in.
    let plmn = match crate::modules::at_commander::AtCommander::open(&line.modem)
        .and_then(|mut at| crate::vowifi::plmn::derive_plmn(&mut at))
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "volte-register: [attaching] could not derive the home PLMN from the SIM: {e}"
            );
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(mcc = %plmn.mcc, mnc = %plmn.mnc, "derived home PLMN from the SIM");

    // Stage 2: registration, over the same shared code the VoWiFi path uses.
    let reg_cfg = crate::ims::ImsRegisterConfig {
        modem_port: line.modem.clone(),
        pcsc_reader: false,
        pcscf_addr,
        pcscf_port: args.pcscf_port,
        mcc: plmn.mcc.clone(),
        mnc: plmn.mnc.clone(),
        imsi: None,
        imei: None,
        use_tcp: args.tcp,
        sec_agree: args.sec_agree,
        msisdn: line.msisdn.clone(),
        access_network_info: crate::volte::read_access_network_info(&line.modem),
        register_uri_home_domain: false,
        gm_auth_alg: None,
        gm_cipher_alg: None,
    };

    // Staying up and renewing is the default; --once is the one-shot
    // diagnostic. A rejected first attempt never enters the renewal loop.
    let result = crate::volte::registration::run(
        &reg_cfg,
        // Lets the renewal loop re-establish the PDN when it drops; without
        // it a dropped attachment is unrecoverable.
        Some(&settings),
        args.once,
        &args.status_path,
        crate::ims::DEFAULT_EXPIRES,
    );

    if !args.keep_pdn {
        if let Err(e) = crate::volte::detach(&settings, attach.displaced_cid) {
            tracing::warn!(error = %e, "failed to release the IMS PDN");
        }
    }

    match result {
        Ok(RegisterOutcome::Success { status, .. }) => {
            println!("\nIMS registration over LTE ACCEPTED (status {status}).");
            ExitCode::SUCCESS
        }
        Ok(RegisterOutcome::Rejected { status, reason }) => {
            eprintln!("\nvolte-register: [registering] the network rejected the registration: {status} {reason}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("\nvolte-register: [registering] {e}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn handle_volte_discover_command(args: &crate::cli::VolteDiscoverArgs) -> ExitCode {
    use crate::cli::VolteDiscoverMethod;
    use crate::volte::pcscf::{self, DiscoveryInputs, DiscoveryMethod};

    let only = match args.method {
        VolteDiscoverMethod::Auto => None,
        VolteDiscoverMethod::Dhcpv6 => Some(DiscoveryMethod::Dhcpv6),
        VolteDiscoverMethod::Pco => Some(DiscoveryMethod::Pco),
        VolteDiscoverMethod::Dns => Some(DiscoveryMethod::Dns),
    };

    // The DNS probe needs the home realm. Deriving it from the SIM keeps the
    // command usable with no arguments, matching how the VoWiFi path resolves
    // its PLMN.
    let realm = match (&args.mcc, &args.mnc) {
        (Some(mcc), Some(mnc)) => Some(pcscf::home_realm(mcc, mnc)),
        _ => match crate::modules::at_commander::AtCommander::open(&args.modem)
            .and_then(|mut at| crate::vowifi::plmn::derive_plmn(&mut at))
        {
            Ok(plmn) => Some(pcscf::home_realm(&plmn.mcc, &plmn.mnc)),
            Err(e) => {
                tracing::warn!(error = %e, "could not derive the home PLMN; the DNS probe will be skipped");
                None
            }
        },
    };

    let iface = args.iface.clone().unwrap_or_default();
    let inputs = DiscoveryInputs {
        iface: &iface,
        cid: args.cid,
        modem_port: &args.modem,
        realm,
        override_pcscf: args.pcscf,
        only,
        epdg_cache_path: Some(std::path::PathBuf::from(&args.pcscf_source_path)),
    };

    match pcscf::discover(&inputs) {
        Ok(report) => {
            print!("{}", report.summary());
            // The breakdown is printed either way; the exit code reflects only
            // whether an address was determined.
            if report.outcome.is_some() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("volte-discover: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the long-lived inbound bridging service
/// (specs/017-volte-inbound-bridge). Unlike `volte-listen`, which registers
/// for a fixed window and declines everything, this holds the registration
/// open and answers calls until stopped.
pub(crate) fn handle_volte_bridge_command(
    args: &crate::cli::VolteBridgeArgs,
    cli: &crate::cli::Cli,
) -> ExitCode {
    let app_config = match load_config(cli.config.as_deref().unwrap_or(std::path::Path::new(""))) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("volte-bridge: {e}");
            return ExitCode::FAILURE;
        }
    };

    // An explicit `--modem` bridges exactly that one modem (the diagnostic /
    // single-line path, no namespace — research.md R7). Omitting it means the
    // production, auto-discovered path (specs/020-volte-line-netns): the line
    // table was already resolved and written by `volte-discover-lines`, so
    // this reads it back rather than re-scanning (research.md R7's "discover
    // once" principle) and runs Agent B only — each line's carrier half is
    // its own `volte-carrier-agent` process, started separately by
    // `supervise::orchestrate_volte` inside that line's namespace.
    let (lines, spawn_carrier_threads) = match &args.modem {
        Some(modem) => (volte_bridge_single_line(args, modem), true),
        None => (
            volte_bridge_manifest_lines(&app_config.volte, args.pcscf_port),
            false,
        ),
    };

    let lines = match lines {
        Ok(lines) if !lines.is_empty() => lines,
        Ok(_) => {
            eprintln!(
                "volte-bridge: no usable LTE lines in the manifest — run `volte-discover-lines` \
                 first, or check it found a usable modem"
            );
            return ExitCode::FAILURE;
        }
        Err(msg) => {
            eprintln!("volte-bridge: {msg}");
            return ExitCode::FAILURE;
        }
    };

    crate::volte::bridge::run(
        crate::volte::bridge::ServiceConfig {
            lines,
            force: args.force,
            spawn_carrier_threads,
        },
        &app_config,
    )
}

/// The single explicit-`--modem` line (index 0, default port trio) — today's
/// behaviour, so a diagnostic `volte-bridge --modem /dev/ttyUSBx` is unchanged.
/// No namespace, no veth (research.md R7): `netns`/`veth_*` stay empty, which
/// is what selects `LOOPBACK` throughout.
fn volte_bridge_single_line(
    args: &crate::cli::VolteBridgeArgs,
    modem: &std::path::Path,
) -> Result<Vec<crate::volte::bridge::BridgeLine>, String> {
    use crate::volte::discovery;
    let explicit = args.pcscf.map(|a| a.to_string());
    let Some(pcscf) = resolve_line_pcscf(explicit, args.pcscf_port, &args.pcscf_source_path) else {
        return Err(format!(
            "[discovering-pcscf] no P-CSCF address available: none passed with --pcscf, and \
             nothing usable in {}. The VoWiFi path writes one file per line \
             (`<base>-<index>`, e.g. /tmp/pcscf-0), so a path with no index will never \
             appear — point --pcscf-source-path at a specific line's file.",
            args.pcscf_source_path
        ));
    };
    let card_id = args
        .card_id
        .clone()
        .unwrap_or_else(|| crate::volte::bridge::DEFAULT_CARD_ID.to_string());
    let settings = crate::volte::VolteSettings {
        modem_port: modem.to_path_buf(),
        iface: args.iface.clone().unwrap_or_default(),
        cid: args.cid,
        apn: args.apn.clone(),
        pcscf: Some(pcscf),
        restore_cid_path: args.restore_cid_path.clone(),
    };
    Ok(vec![crate::volte::bridge::BridgeLine {
        card_id,
        settings,
        msisdn: args.msisdn.clone(),
        sip_leg_port: discovery::sip_leg_port(0),
        control_port: discovery::control_port(0),
        status_port: discovery::status_port(0),
        netns: String::new(),
        veth_carrier_addr: String::new(),
        veth_telephony_addr: String::new(),
    }])
}

/// Every line from the manifest `volte-discover-lines` already wrote
/// (specs/020-volte-line-netns) — the production, auto-discovered path's
/// `volte-bridge` (Agent B only) no longer scans or resolves lines itself.
/// P-CSCF is still resolved fresh here (not cached in the manifest, since it
/// can change — an ePDG capture completing after discovery ran, say) using
/// each line's recorded override, exactly the precedence
/// `volte_bridge_single_line`/the pre-020 discovered-lines path always used.
pub(crate) fn volte_bridge_manifest_lines(
    volte: &crate::config::VolteConfig,
    pcscf_port: u16,
) -> Result<Vec<crate::volte::bridge::BridgeLine>, String> {
    use crate::volte::discovery;

    let manifest = discovery::read_manifest(&discovery::manifest_path()).map_err(|e| {
        format!("no VoLTE line manifest ({e}) — run `volte-discover-lines` before `volte-bridge`")
    })?;

    let mut lines = Vec::new();
    for entry in &manifest.lines {
        let explicit = if entry.pcscf.is_empty() {
            None
        } else {
            Some(entry.pcscf.clone())
        };
        let Some(pcscf) = resolve_line_pcscf(explicit, pcscf_port, &volte.pcscf_source_path) else {
            tracing::error!(
                card_id = %entry.card_id,
                pcscf_source_path = %volte.pcscf_source_path,
                "no P-CSCF available for this line: none configured, and nothing usable at \
                 pcscf_source_path. The VoWiFi path writes one file per line \
                 (`<base>-<index>`, e.g. /tmp/pcscf-0), so a path with no index will never \
                 appear — point [volte].pcscf_source_path at a specific line's file. \
                 Skipping this line"
            );
            continue;
        };
        let settings = crate::volte::VolteSettings {
            modem_port: std::path::PathBuf::from(&entry.modem_port),
            iface: entry.iface.clone(),
            cid: entry.cid,
            apn: entry.apn.clone(),
            pcscf: Some(pcscf),
            restore_cid_path: if entry.restore_cid_path.is_empty() {
                None
            } else {
                Some(std::path::PathBuf::from(&entry.restore_cid_path))
            },
        };
        lines.push(crate::volte::bridge::BridgeLine {
            card_id: entry.card_id.clone(),
            settings,
            msisdn: if entry.msisdn.is_empty() {
                None
            } else {
                Some(entry.msisdn.clone())
            },
            sip_leg_port: entry.sip_leg_port,
            control_port: entry.control_port,
            status_port: entry.status_port,
            netns: entry.netns.clone(),
            veth_carrier_addr: entry.veth_carrier_addr.clone(),
            veth_telephony_addr: entry.veth_telephony_addr.clone(),
        });
    }
    Ok(lines)
}

/// Resolves the auto-discovered VoLTE line table and writes it as the
/// manifest — the LTE counterpart to `discover` (specs/020-volte-line-netns).
/// Run once, up front, by `supervise::orchestrate_volte` before any per-line
/// namespace or process exists.
pub(crate) fn handle_volte_discover_lines_command(
    args: &crate::cli::VolteDiscoverLinesArgs,
    cli: &crate::cli::Cli,
) -> ExitCode {
    use crate::volte::discovery;

    let app_config = match load_config(cli.config.as_deref().unwrap_or(std::path::Path::new(""))) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("volte-discover-lines: {e}");
            return ExitCode::FAILURE;
        }
    };
    let volte = &app_config.volte;

    let preferred: Vec<std::path::PathBuf> = volte
        .line_overrides
        .iter()
        .filter_map(|o| o.modem_port.as_deref().map(std::path::PathBuf::from))
        .collect();
    let mut policy = crate::modules::discovery::DiscoveryPolicy::new(app_config.discovery.clone());
    let modems =
        match crate::modules::discovery::scan_all_preferring_with_policy(&preferred, &mut policy) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("volte-discover-lines: modem discovery failed: {e}");
                return ExitCode::FAILURE;
            }
        };

    let table = discovery::resolve_volte_lines(&modems, volte);
    for failed in &table.failed {
        eprintln!(
            "volte-discover-lines: {} not usable as a line: {}",
            failed.card_id, failed.reason
        );
    }

    if let Err(e) = discovery::write_manifest(&table.lines, args.restore_cid_path.as_deref()) {
        eprintln!("volte-discover-lines: failed to write the line manifest: {e}");
        return ExitCode::FAILURE;
    }

    // stderr, not stdout: `supervise::orchestrate_volte` captures this command's
    // stdout wholesale into `eval` when `--shell-env` is set (mirroring
    // `discover`'s own contract) — any other stdout output gets `eval`'d
    // right alongside the KEY=value lines and breaks the shell (found live:
    // this line's `(`/`)` triggered a bash syntax error the first time this
    // ran against real hardware).
    eprintln!(
        "volte-discover-lines: resolved {} line(s), {} failed",
        table.lines.len(),
        table.failed.len()
    );
    if args.shell_env {
        print!("{}", render_volte_discover_lines_shell_env(&table.lines));
    }

    ExitCode::SUCCESS
}

/// Bash indexed-array output for `supervise::orchestrate_volte`'s VoLTE per-line
/// loop to `eval` — mirrors `render_discover_shell_env`'s array convention
/// exactly (`LINE_CARD_ID=(...)`, indexed by position, not per-index scalar
/// variables) so both subsystems' entrypoint loops read the same shape.
pub fn render_volte_discover_lines_shell_env(
    lines: &[crate::volte::discovery::ResolvedVolteLine],
) -> String {
    let mut out = String::new();
    fn arr<T: ToString>(vals: impl Iterator<Item = T>) -> String {
        format!(
            "({})",
            vals.map(|v| shell_quote(&v.to_string()))
                .collect::<Vec<_>>()
                .join(" ")
        )
    }

    let _ = writeln!(&mut out, "VOLTE_LINE_COUNT={}", lines.len());
    let _ = writeln!(
        &mut out,
        "VOLTE_LINE_CARD_ID={}",
        arr(lines.iter().map(|l| l.card_id.clone()))
    );
    let _ = writeln!(
        &mut out,
        "VOLTE_LINE_MODEM_PORT={}",
        arr(lines.iter().map(|l| l.modem_port.display().to_string()))
    );
    let _ = writeln!(
        &mut out,
        "VOLTE_LINE_IFACE={}",
        arr(lines.iter().map(|l| l.iface.clone()))
    );
    let _ = writeln!(
        &mut out,
        "VOLTE_LINE_NETNS={}",
        arr(lines.iter().map(|l| l.netns.clone()))
    );
    let _ = writeln!(
        &mut out,
        "VOLTE_LINE_VETH_CARRIER_IFACE={}",
        arr(lines.iter().map(|l| l.veth_carrier_iface.clone()))
    );
    let _ = writeln!(
        &mut out,
        "VOLTE_LINE_VETH_TELEPHONY_IFACE={}",
        arr(lines.iter().map(|l| l.veth_telephony_iface.clone()))
    );
    let _ = writeln!(
        &mut out,
        "VOLTE_LINE_VETH_CARRIER_ADDR={}",
        arr(lines.iter().map(|l| l.veth_carrier_addr.clone()))
    );
    let _ = writeln!(
        &mut out,
        "VOLTE_LINE_VETH_TELEPHONY_ADDR={}",
        arr(lines.iter().map(|l| l.veth_telephony_addr.clone()))
    );
    out
}

/// The per-line carrier-facing half (specs/020-volte-line-netns) — reads its
/// settings from the manifest `volte-discover-lines` wrote, attaches this
/// line's IMS PDN, registers, and answers calls until the registration ends.
/// One-shot: does not retry internally (`supervise::orchestrate_volte` restarts it on
/// exit, mirroring `vowifi-ims-agent`'s supervision).
pub(crate) fn handle_volte_carrier_agent_command(
    args: &crate::cli::VolteCarrierAgentArgs,
    cli: &crate::cli::Cli,
) -> ExitCode {
    use crate::volte::discovery;

    let app_config = match load_config(cli.config.as_deref().unwrap_or(std::path::Path::new(""))) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("volte-carrier-agent: {e}");
            return ExitCode::FAILURE;
        }
    };

    let manifest = match discovery::read_manifest(&discovery::manifest_path()) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "volte-carrier-agent: no line manifest ({e}) — run `volte-discover-lines` first"
            );
            return ExitCode::FAILURE;
        }
    };
    let Some(entry) = manifest.lines.iter().find(|l| l.index == args.line) else {
        eprintln!(
            "volte-carrier-agent: no line {} in the manifest ({} line(s) resolved)",
            args.line,
            manifest.lines.len()
        );
        return ExitCode::FAILURE;
    };

    let explicit = if entry.pcscf.is_empty() {
        None
    } else {
        Some(entry.pcscf.clone())
    };
    let Some(pcscf) = resolve_line_pcscf(
        explicit,
        args.pcscf_port,
        &app_config.volte.pcscf_source_path,
    ) else {
        eprintln!(
            "volte-carrier-agent: line {}: no P-CSCF available (none configured and none \
             captured by the ePDG path)",
            args.line
        );
        return ExitCode::FAILURE;
    };

    let settings = crate::volte::VolteSettings {
        modem_port: std::path::PathBuf::from(&entry.modem_port),
        iface: entry.iface.clone(),
        cid: entry.cid,
        apn: entry.apn.clone(),
        pcscf: Some(pcscf),
        restore_cid_path: if entry.restore_cid_path.is_empty() {
            None
        } else {
            Some(std::path::PathBuf::from(&entry.restore_cid_path))
        },
    };
    let line = crate::volte::bridge::BridgeLine {
        card_id: entry.card_id.clone(),
        settings,
        msisdn: if entry.msisdn.is_empty() {
            None
        } else {
            Some(entry.msisdn.clone())
        },
        sip_leg_port: entry.sip_leg_port,
        control_port: entry.control_port,
        status_port: entry.status_port,
        netns: entry.netns.clone(),
        veth_carrier_addr: entry.veth_carrier_addr.clone(),
        veth_telephony_addr: entry.veth_telephony_addr.clone(),
    };

    let modem_port = line.settings.modem_port.clone();
    let modem_lock = std::sync::Arc::new(crate::modules::modem_lock::ModemLock::new());
    // Shared with `carrier_agent::run` below, not owned by the sweep thread:
    // the same message delivered over both the registration and the modem
    // must collapse to one (specs/038-reliable-sms-delivery).
    let dedupe = std::sync::Arc::new(std::sync::Mutex::new(crate::volte::sms::Dedupe::default()));
    {
        let modem_port = modem_port.clone();
        let lock = modem_lock.clone();
        let dedupe = dedupe.clone();
        let control_addr = std::net::SocketAddr::new(
            if line.veth_telephony_addr.is_empty() {
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
            } else {
                line.veth_telephony_addr
                    .parse()
                    .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
            },
            line.control_port,
        );
        if let Err(e) = std::thread::Builder::new()
            .name(format!("volte-sms-{}", line.card_id))
            .spawn(move || {
                crate::volte::sms::run_modem_reader(modem_port, control_addr, lock, dedupe)
            })
        {
            eprintln!(
                "volte-carrier-agent: failed to start the modem SMS reader for this line: {e}"
            );
        }
    }

    // Cross-process: cannot share the telephony half's `pbx_registered` flag
    // (see carrier_agent.rs's module docs) — the same limitation
    // `vowifi-ims-agent` already has for the same reason.
    // One attempt only on this path — the subcommand *is* the attempt — so the
    // registration lives exactly as long as the call. `carrier_agent::run` no
    // longer registers for itself; see its docs for the retry-loop leak that
    // forced the ownership the other way round.
    let progress = crate::ims::agent::watchdog::register(std::sync::Arc::new(
        crate::ims::agent::watchdog::Progress::new("volte-dispatch"),
    ));
    crate::volte::carrier_agent::run(&line, &app_config, modem_lock, dedupe, None, &progress);

    eprintln!(
        "volte-carrier-agent: line {} ({}) stopped",
        args.line, line.card_id
    );
    ExitCode::FAILURE
}

/// Resolves one line's P-CSCF: an explicitly-configured address wins, else the
/// address the ePDG/VoWiFi path captured at `source_path` (so a VoWiFi run on
/// this SIM primes the LTE path). `None` when neither is available.
fn resolve_line_pcscf(
    explicit: Option<String>,
    pcscf_port: u16,
    source_path: &str,
) -> Option<std::net::SocketAddr> {
    if let Some(addr) = explicit {
        if let Ok(ip) = addr.parse::<std::net::IpAddr>() {
            return Some(std::net::SocketAddr::new(ip, pcscf_port));
        }
    }
    let cache = std::path::PathBuf::from(source_path);
    crate::volte::pcscf::probe_epdg_cache(&cache)
        .found()
        .map(|ip| std::net::SocketAddr::new(ip, pcscf_port))
}

/// Per-line restore-cid file so each modem's displaced context is recorded and
/// restored independently: `<base>-<index>`. `None` when no base was given.
/// Releases every LTE line the running bridge recorded in its manifest, each
/// with the displaced context read from that line's own restore-cid file, then
/// removes the manifest. A no-op (success) when no manifest exists — the
/// single-line `volte-register` path writes none and is torn down by the
/// entrypoint's own `volte-pdn down`.
/// Tears down one line (`line = Some(idx)`) or every line (`line = None`).
///
/// With `--line`, this is meant to be run as `ip netns exec <that line's
/// netns> ... volte-cleanup --line <idx>` (specs/020-volte-line-netns
/// research.md R6): `detach`'s `netcfg::teardown` issues namespace-scoped
/// `ip`/sysctl commands that only find the interface when run inside the
/// namespace it currently lives in — running them from the default namespace
/// after the interface has already been moved into a per-line namespace
/// would silently fail to restore the displaced data context, reopening the
/// exact bug `e50ddca` fixed once already for the single-namespace case.
pub(crate) fn handle_volte_cleanup_command(line: Option<u32>) -> ExitCode {
    use crate::volte::discovery;
    let path = discovery::manifest_path();
    let manifest = match discovery::read_manifest(&path) {
        Ok(m) => m,
        Err(_) => return ExitCode::SUCCESS,
    };
    let mut all_ok = true;
    for entry in manifest
        .lines
        .iter()
        .filter(|l| line.is_none_or(|i| i == l.index))
    {
        let restore_cid = std::fs::read_to_string(&entry.restore_cid_path)
            .ok()
            .and_then(|s| s.trim().parse::<u8>().ok());
        let settings = crate::volte::VolteSettings {
            modem_port: std::path::PathBuf::from(&entry.modem_port),
            iface: entry.iface.clone(),
            cid: entry.cid,
            // `detach` uses only the modem port, interface, cid and restore-cid.
            apn: String::new(),
            pcscf: None,
            restore_cid_path: None,
        };
        match crate::volte::detach(&settings, restore_cid) {
            Ok(()) => println!(
                "volte-cleanup: released line {} ({})",
                entry.card_id, entry.modem_port
            ),
            Err(e) => {
                eprintln!("volte-cleanup: line {} teardown failed: {e}", entry.card_id);
                all_ok = false;
            }
        }
    }
    // Remove the manifest only once every line has been processed (no
    // `--line` filter) — a per-line invocation leaves it for the remaining
    // lines' own cleanup calls to read; the next `volte-discover-lines` run
    // overwrites it wholesale regardless, so a stale manifest between now and
    // then is harmless.
    if line.is_none() {
        let _ = std::fs::remove_file(&path);
    }
    if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
