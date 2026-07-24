use gsm_sip_bridge::cli::{Cli, Commands};
use gsm_sip_bridge::config::load_config;
use gsm_sip_bridge::control::client;
use gsm_sip_bridge::control::protocol::{ControlCmd, ControlResp};
use gsm_sip_bridge::control::server::start_control_server;
use gsm_sip_bridge::metrics;
use gsm_sip_bridge::modules::{CardPool, ControlCmdSender};
use gsm_sip_bridge::observability::{logging, modemmanager};
use gsm_sip_bridge::runtime;
use gsm_sip_bridge::sip::SipBridge;
use gsm_sip_bridge::sms::SmsHandler;
use gsm_sip_bridge::store::StoreHandle;
use std::process::ExitCode;
use tokio::sync::{mpsc, watch};

fn main() -> ExitCode {
    let cli = Cli::parse_args();

    // Read [logging].level ahead of the full config load below (which may
    // legitimately fail, e.g. an unset secret env var) so logging is set up
    // before anything else runs.
    let log_level = cli
        .config
        .as_deref()
        .map(gsm_sip_bridge::config::read_log_level)
        .unwrap_or_else(|| "info".to_string());
    logging::init(&log_level, cli.verbose);

    // Handle card subcommands before daemon startup
    if let Some(Commands::Card(card_args)) = &cli.command {
        return handle_card_command(card_args, &cli);
    }

    if let Some(Commands::ImsRegister(args)) = &cli.command {
        return handle_ims_register_command(args);
    }

    if let Some(Commands::ImsCall(args)) = &cli.command {
        return handle_ims_call_command(args);
    }

    if let Some(Commands::VowifiImsAgent(args)) = &cli.command {
        return handle_vowifi_ims_agent_command(&cli, args.line);
    }

    if let Some(Commands::VowifiSipAgent) = &cli.command {
        return handle_vowifi_sip_agent_command(&cli);
    }

    if let Some(Commands::VowifiStatus) = &cli.command {
        return handle_vowifi_status_command(&cli);
    }

    if let Some(Commands::VowifiUsimBridge(args)) = &cli.command {
        return gsm_sip_bridge::vowifi::usim_bridge::run(
            &args.modem,
            &args.vpcd_host,
            args.vpcd_port,
        );
    }

    if let Some(Commands::VowifiImsi(args)) = &cli.command {
        return gsm_sip_bridge::vowifi::imsi::run(&args.modem);
    }

    if let Some(Commands::VowifiPlmn(args)) = &cli.command {
        return gsm_sip_bridge::vowifi::plmn::run(&args.modem);
    }

    if let Some(Commands::ModemIms(args)) = &cli.command {
        return handle_modem_ims_command(args, &cli);
    }

    if let Some(Commands::VoltePdn(args)) = &cli.command {
        return handle_volte_pdn_command(args);
    }

    if let Some(Commands::VolteStatus(args)) = &cli.command {
        return handle_volte_status_command(args);
    }

    if let Some(Commands::VolteDiscover(args)) = &cli.command {
        return handle_volte_discover_command(args);
    }

    if let Some(Commands::VolteRegister(args)) = &cli.command {
        return handle_volte_register_command(args);
    }

    if let Some(Commands::VolteCall(args)) = &cli.command {
        return handle_volte_call_command(args);
    }

    if let Some(Commands::VolteListen(args)) = &cli.command {
        return handle_volte_listen_command(args);
    }

    if let Some(Commands::VolteBridge(args)) = &cli.command {
        return handle_volte_bridge_command(args, &cli);
    }

    if let Some(Commands::VolteDiscoverLines(args)) = &cli.command {
        return handle_volte_discover_lines_command(args, &cli);
    }

    if let Some(Commands::VolteCarrierAgent(args)) = &cli.command {
        return handle_volte_carrier_agent_command(args, &cli);
    }

    if let Some(Commands::VolteCleanup(args)) = &cli.command {
        return handle_volte_cleanup_command(args.line);
    }

    if let Some(Commands::Config(args)) = &cli.command {
        return handle_config_command(args, &cli);
    }

    if let Some(Commands::Discover(args)) = &cli.command {
        return handle_discover_command(args, &cli);
    }

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting gsm-sip-bridge"
    );

    let config = match load_config(cli.config.as_deref().unwrap_or(std::path::Path::new(""))) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "configuration failed");
            return ExitCode::from(1);
        }
    };

    modemmanager::check_modemmanager();
    metrics::register_build_info();
    metrics::server::record_start_time();

    let rt = match runtime::build_runtime() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "runtime initialization failed");
            return ExitCode::from(1);
        }
    };

    let store = match StoreHandle::open(std::path::Path::new(&config.sms.db_path)) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "store initialization failed");
            return ExitCode::from(66);
        }
    };

    let (shutdown_tx, shutdown_rx) = runtime::shutdown_channel();
    let (control_tx, control_rx): (ControlCmdSender, _) = mpsc::channel(8);
    let socket_path = config.control.socket_path.clone();

    rt.block_on(async {
        let metrics_port = config.metrics.port;
        let agent_report_interval_seconds = config.metrics.agent_report_interval_seconds;
        let metrics_handle = tokio::spawn(async move {
            if let Err(e) =
                metrics::server::serve(metrics_port, agent_report_interval_seconds).await
            {
                tracing::error!(error = %e, "metrics server failed");
            }
        });

        tracing::info!(
            sip_server = %config.sip.server,
            sip_port = config.sip.port,
            modules_max = config.modules.max_concurrent,
            metrics_port = config.metrics.port,
            control_socket = %socket_path,
            "configuration loaded"
        );

        let single_card = match (&cli.serial, &cli.audio) {
            (Some(serial), Some(audio)) => {
                tracing::info!(
                    serial = %serial.display(),
                    audio = %audio,
                    "single-card override mode"
                );
                Some((serial.clone(), audio.clone()))
            }
            _ => None,
        };

        let (shutdown_watch_tx, shutdown_watch_rx) = watch::channel(false);

        let ctrl_server = start_control_server(&socket_path, control_tx, shutdown_watch_rx).await;

        let sip_bridge = SipBridge::new(&config);
        let sms_handler = SmsHandler::new(&config.sms, store.sender());
        let card_pool = CardPool::new(config, store, sip_bridge, sms_handler);

        let pool_handle = tokio::spawn(async move {
            card_pool.run(single_card, shutdown_rx, control_rx).await;
        });

        runtime::wait_for_shutdown(shutdown_tx).await;

        let _ = shutdown_watch_tx.send(true);
        ctrl_server.abort();
        pool_handle.abort();
        metrics_handle.abort();
    });

    tracing::info!("shutdown complete");
    ExitCode::SUCCESS
}

fn handle_card_command(args: &gsm_sip_bridge::cli::CardArgs, cli: &Cli) -> ExitCode {
    let socket_path = match cli.config.as_deref() {
        None => gsm_sip_bridge::config::DEFAULT_CONTROL_SOCKET.to_string(),
        Some(p) => match load_config(p) {
            Ok(c) => c.control.socket_path,
            Err(_) => gsm_sip_bridge::config::DEFAULT_CONTROL_SOCKET.to_string(),
        },
    };

    let cmd = match build_control_cmd(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    match client::send_cmd(&socket_path, &cmd) {
        Ok(resp) => print_resp(resp),
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn build_ims_register_config(
    args: &gsm_sip_bridge::cli::ImsRegisterArgs,
) -> gsm_sip_bridge::ims::ImsRegisterConfig {
    gsm_sip_bridge::ims::ImsRegisterConfig {
        modem_port: args.modem.clone(),
        pcscf_addr: args.pcscf,
        pcscf_port: args.pcscf_port,
        mcc: args.mcc.clone(),
        mnc: args.mnc.clone(),
        imsi: args.imsi.clone(),
        imei: args.imei.clone(),
        use_tcp: args.tcp,
        sec_agree: args.sec_agree,
        msisdn: args.msisdn.clone(),
        access_network_info: gsm_sip_bridge::ims::ACCESS_NETWORK_WLAN.to_string(),
    }
}

fn handle_ims_register_command(args: &gsm_sip_bridge::cli::ImsRegisterArgs) -> ExitCode {
    use gsm_sip_bridge::ims::{run_register, RegisterOutcome};

    let cfg = build_ims_register_config(args);

    match run_register(&cfg) {
        Ok(RegisterOutcome::Success { status, headers }) => {
            println!("REGISTER succeeded: {status} OK");
            for (k, v) in headers {
                println!("  {k}: {v}");
            }
            ExitCode::SUCCESS
        }
        Ok(RegisterOutcome::Rejected { status, reason }) => {
            eprintln!("REGISTER rejected: {status} {reason}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn handle_ims_call_command(args: &gsm_sip_bridge::cli::ImsCallArgs) -> ExitCode {
    use gsm_sip_bridge::ims::call::{run_call, CallConfig, CallOutcome};
    use std::time::Duration;

    let cfg = CallConfig {
        register: build_ims_register_config(&args.register),
        callee: args.to.clone(),
        record_path: args.record.clone(),
        record_sent_path: args.record_sent.clone(),
        ring_timeout: Duration::from_secs(args.ring_timeout_secs),
        call_duration: Duration::from_secs(args.call_duration_secs),
        // `ims-call` keeps sending the tone pattern, unchanged: echo is opt-in
        // so the VoWiFi diagnostic behaves exactly as it did (FR-020).
        echo: None,
        one_way_threshold_percent:
            gsm_sip_bridge::ims::media_stats::DEFAULT_ONE_WAY_THRESHOLD_PERCENT,
        // Historical ordering, so the VoWiFi diagnostic's offer is unchanged
        // (FR-020). Carriers on that path require wideband anyway.
        codec_offer: gsm_sip_bridge::ims::sdp::CodecOffer::legacy(amr_safe::is_available()),
    };

    match run_call(&cfg) {
        Ok(CallOutcome::Answered {
            recorded_path,
            recorded_samples,
            sent_path,
            sent_samples,
            ..
        }) => {
            println!(
                "call answered — recorded {recorded_samples} received samples to {}",
                recorded_path.display()
            );
            if let Some(sent_path) = sent_path {
                println!(
                    "  and {sent_samples} sent samples to {}",
                    sent_path.display()
                );
            }
            ExitCode::SUCCESS
        }
        Ok(CallOutcome::NotAnswered { status, reason }) => {
            eprintln!("call not answered: {status} {reason}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Shared by the three `vowifi-*` subcommands: `--config` is mandatory for
/// these (unlike the daemon path, which tolerates a missing path via
/// `cli.config.as_deref().unwrap_or(...)` for its own defaulting), and
/// `[vowifi].enabled` must be `true` — this is the guard that stops an
/// operator who hasn't provisioned VoWiFi from accidentally starting one of
/// these agents (see `config::VowifiConfig::enabled` docs).
fn load_vowifi_config(cli: &Cli) -> Result<gsm_sip_bridge::config::AppConfig, ExitCode> {
    let Some(path) = cli.config.as_deref() else {
        eprintln!("error: --config is required for vowifi-* subcommands");
        return Err(ExitCode::FAILURE);
    };
    let config = load_config(path).map_err(|e| {
        eprintln!("error: {e}");
        ExitCode::FAILURE
    })?;
    if !config.vowifi.enabled {
        eprintln!("error: [vowifi].enabled is false in the config file");
        return Err(ExitCode::FAILURE);
    }
    Ok(config)
}

/// Loads `--line N`'s fully-derived `VowifiConfig` from the `discover`
/// subcommand's line-resolution file — see
/// `specs/013-multi-card-vowifi/contracts/agent-topology-contract.md`.
/// `--line` is required: every line, including a single-SIM setup, is
/// resolved by `discover` first (`docker/entrypoint.sh` always runs it
/// before starting this agent). Deliberately does NOT re-run discovery
/// itself: doing so would re-probe modems a sibling
/// `vowifi-usim-bridge`/other line's agent may already have open
/// (research.md item 3).
fn handle_vowifi_ims_agent_command(cli: &Cli, line: Option<u32>) -> ExitCode {
    let config = match load_vowifi_config(cli) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let Some(index) = line else {
        eprintln!(
            "error: vowifi-ims-agent requires --line N (run `gsm-sip-bridge discover` first)"
        );
        return ExitCode::FAILURE;
    };
    let (card_id, line_config) = match load_line_config(index) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };
    gsm_sip_bridge::ims::agent::run(&card_id, &line_config, &config)
}

/// Reads the `discover` subcommand's line-resolution file and returns line
/// `index`'s card id and fully-derived `VowifiConfig`.
fn load_line_config(index: u32) -> Result<(String, gsm_sip_bridge::config::VowifiConfig), String> {
    let path = lines_file_path();
    let resolution = gsm_sip_bridge::vowifi::discovery::read_line_resolution(&path)?;
    resolution
        .line(index)
        .map(|l| (l.card_id.clone(), l.config.clone()))
        .ok_or_else(|| {
            format!(
                "line {index} not found in {} (run `gsm-sip-bridge discover` first; \
                 does that many usable VoWiFi lines actually exist?)",
                path.display()
            )
        })
}

fn lines_file_path() -> std::path::PathBuf {
    gsm_sip_bridge::modules::discovery::lines_file_path()
}

fn handle_vowifi_sip_agent_command(cli: &Cli) -> ExitCode {
    let config = match load_vowifi_config(cli) {
        Ok(c) => c,
        Err(code) => return code,
    };
    gsm_sip_bridge::vowifi::run(&config)
}

fn handle_vowifi_status_command(cli: &Cli) -> ExitCode {
    let config = match load_vowifi_config(cli) {
        Ok(c) => c,
        Err(code) => return code,
    };
    gsm_sip_bridge::vowifi::print_status(&config.vowifi)
}

/// Deliberately silent (no stdout/stderr on the success path) — callers
/// (`docker/entrypoint.sh`) only care about the exit code, e.g.
/// `if gsm-sip-bridge --config "$CONFIG" config vowifi-enabled; then ...`.
/// Unlike `load_vowifi_config`, does NOT require `[vowifi].enabled = true`
/// — that's exactly the thing being checked, not a precondition.
/// Loads the full config (not `load_vowifi_config`, which insists VoWiFi is
/// enabled): the whole point here is to act on `[vowifi].enabled` in *both*
/// directions — disable the modem's IMS when the bridge is on, re-enable it
/// when the bridge is off and VoLTE should work again.
fn handle_modem_ims_command(args: &gsm_sip_bridge::cli::ModemImsArgs, cli: &Cli) -> ExitCode {
    let Some(path) = cli.config.as_deref() else {
        eprintln!("modem-ims: --config is required");
        return ExitCode::FAILURE;
    };
    let config = match load_config(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("modem-ims: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Either host-driven path wants the modem's own IMS stack off — never
    // both at once (`volte::guard` refuses that), but either alone still
    // needs it (specs/020-volte-line-netns: this used to check
    // `config.vowifi.enabled` alone, which left a VoLTE-only deployment's
    // modem fighting our registration with its own internal one).
    let host_ims_wanted = config.vowifi.enabled || config.volte.enabled;
    gsm_sip_bridge::vowifi::ims_mode::run(&args.modem, host_ims_wanted)
}

fn volte_settings(
    modem: &std::path::Path,
    iface: &Option<String>,
    cid: u8,
    apn: &str,
) -> gsm_sip_bridge::volte::VolteSettings {
    gsm_sip_bridge::volte::VolteSettings {
        modem_port: modem.to_path_buf(),
        iface: iface.clone().unwrap_or_default(),
        cid,
        apn: apn.to_string(),
        pcscf: None,
        restore_cid_path: None,
    }
}

fn handle_volte_pdn_command(args: &gsm_sip_bridge::cli::VoltePdnArgs) -> ExitCode {
    use gsm_sip_bridge::cli::VoltePdnAction;
    let settings = volte_settings(&args.modem, &args.iface, args.cid, &args.apn);

    match args.action {
        VoltePdnAction::Up => match gsm_sip_bridge::volte::attach(&settings) {
            Ok(report) => {
                print!("{}", report.summary());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("volte-pdn: failed to attach the IMS PDN: {e}");
                ExitCode::FAILURE
            }
        },
        VoltePdnAction::Down => match gsm_sip_bridge::volte::detach(&settings, args.restore_cid) {
            Ok(()) => {
                println!("IMS PDN released (context {}).", args.cid);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("volte-pdn: failed to release the IMS PDN: {e}");
                ExitCode::FAILURE
            }
        },
        // `status` exits 0 whether or not a PDN exists: the state belongs in
        // the output, not the exit code.
        VoltePdnAction::Status => match gsm_sip_bridge::volte::status(&settings) {
            Ok(Some(report)) => {
                print!("{}", report.summary());
                ExitCode::SUCCESS
            }
            Ok(None) => {
                println!("No IMS PDN attached on context {}.", args.cid);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("volte-pdn: failed to read IMS PDN state: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

fn handle_volte_status_command(args: &gsm_sip_bridge::cli::VolteStatusArgs) -> ExitCode {
    // Ask the running service first. It owns the modem's AT port exclusively
    // (research R6), so reading the modem directly while it runs races it
    // mid-transaction — and the live service knows things the modem cannot,
    // like whether a call is in progress right now (FR-033). Only when no
    // service answers is a direct modem read both safe and necessary.
    if gsm_sip_bridge::volte::bridge::print_live_status() {
        return ExitCode::SUCCESS;
    }

    let settings = volte_settings(&args.modem, &args.iface, args.cid, &args.apn);
    match gsm_sip_bridge::volte::status(&settings) {
        Ok(Some(report)) => {
            print!("{}", report.summary());
            let status = gsm_sip_bridge::volte::registration::read_status(&args.status_path);
            print!(
                "{}",
                gsm_sip_bridge::volte::registration::status_summary(status.as_ref())
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

fn handle_volte_listen_command(args: &gsm_sip_bridge::cli::VolteListenArgs) -> ExitCode {
    use std::time::Duration;

    if let Err(e) = gsm_sip_bridge::volte::guard::check_no_vowifi_conflict(args.force) {
        eprintln!("volte-listen: {e}");
        return ExitCode::FAILURE;
    }
    let _lock = match gsm_sip_bridge::volte::guard::RegistrationGuard::acquire(&args.lock_path) {
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
            match gsm_sip_bridge::volte::pcscf::probe_epdg_cache(&cache).found() {
                Some(a) => (a, format!("ePDG capture at {}", cache.display())),
                None => {
                    eprintln!("volte-listen: [discovering-pcscf] no P-CSCF address available; pass --pcscf");
                    return ExitCode::FAILURE;
                }
            }
        }
    };

    let settings = gsm_sip_bridge::volte::VolteSettings {
        modem_port: args.modem.clone(),
        iface: args.iface.clone().unwrap_or_default(),
        cid: args.cid,
        apn: args.apn.clone(),
        pcscf: Some(std::net::SocketAddr::new(pcscf_addr, args.pcscf_port)),
        // This command runs its own detach, so it never needs the recorded cid.
        restore_cid_path: None,
    };
    let attach = match gsm_sip_bridge::volte::attach(&settings) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("volte-listen: [attaching] {e}");
            return ExitCode::FAILURE;
        }
    };

    let plmn = match gsm_sip_bridge::modules::at_commander::AtCommander::open(&args.modem)
        .and_then(|mut at| gsm_sip_bridge::vowifi::plmn::derive_plmn(&mut at))
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("volte-listen: [attaching] could not derive the home PLMN: {e}");
            return ExitCode::FAILURE;
        }
    };

    let reg_cfg = gsm_sip_bridge::ims::ImsRegisterConfig {
        modem_port: args.modem.clone(),
        pcscf_addr,
        pcscf_port: args.pcscf_port,
        mcc: plmn.mcc,
        mnc: plmn.mnc,
        imsi: None,
        imei: None,
        use_tcp: true,
        sec_agree: true,
        msisdn: args.msisdn.clone(),
        access_network_info: gsm_sip_bridge::volte::read_access_network_info(&args.modem),
    };

    println!(
        "Registering, then listening {}s for anything the network delivers.\n\
         DIAL THE SIM NOW — the call will be declined with a busy response, not answered.",
        args.listen_secs
    );

    let result =
        gsm_sip_bridge::ims::agent::probe_inbound(&reg_cfg, Duration::from_secs(args.listen_secs));

    if !args.keep_pdn {
        if let Err(e) = gsm_sip_bridge::volte::detach(&settings, attach.displaced_cid) {
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

fn handle_volte_call_command(args: &gsm_sip_bridge::cli::VolteCallArgs) -> ExitCode {
    use gsm_sip_bridge::ims::call::{run_call, CallConfig, CallOutcome, EchoSettings};
    use std::time::Duration;

    // Refuse before anything touches the modem, so a refusal leaves the system
    // exactly as it was (FR-022).
    if let Err(e) = gsm_sip_bridge::volte::guard::check_no_vowifi_conflict(args.force) {
        eprintln!("volte-call: {e}");
        return ExitCode::FAILURE;
    }
    let _lock = match gsm_sip_bridge::volte::guard::RegistrationGuard::acquire(&args.lock_path) {
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
            match gsm_sip_bridge::volte::pcscf::probe_epdg_cache(&cache).found() {
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

    let settings = gsm_sip_bridge::volte::VolteSettings {
        modem_port: args.modem.clone(),
        iface: args.iface.clone().unwrap_or_default(),
        cid: args.cid,
        apn: args.apn.clone(),
        pcscf: Some(std::net::SocketAddr::new(pcscf_addr, args.pcscf_port)),
        // This command runs its own detach, so it never needs the recorded cid.
        restore_cid_path: None,
    };

    let attach = match gsm_sip_bridge::volte::attach(&settings) {
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

    let plmn = match gsm_sip_bridge::modules::at_commander::AtCommander::open(&args.modem)
        .and_then(|mut at| gsm_sip_bridge::vowifi::plmn::derive_plmn(&mut at))
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("volte-call: [attaching] could not derive the home PLMN: {e}");
            return ExitCode::FAILURE;
        }
    };

    let cfg = CallConfig {
        register: gsm_sip_bridge::ims::ImsRegisterConfig {
            modem_port: args.modem.clone(),
            pcscf_addr,
            pcscf_port: args.pcscf_port,
            mcc: plmn.mcc.clone(),
            mnc: plmn.mnc.clone(),
            imsi: None,
            imei: None,
            use_tcp: true,
            sec_agree: true,
            msisdn: args.msisdn.clone(),
            access_network_info: gsm_sip_bridge::volte::read_access_network_info(&args.modem),
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
        codec_offer: gsm_sip_bridge::ims::sdp::CodecOffer::preferring_wideband(
            amr_safe::is_available(),
        ),
    };

    println!(
        "Placing a call to {}. The answering party will hear their OWN VOICE returned — \
         have them use a handset, not a speakerphone.",
        args.callee
    );

    let result = run_call(&cfg);

    if !args.keep_pdn {
        if let Err(e) = gsm_sip_bridge::volte::detach(&settings, attach.displaced_cid) {
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
fn render_call_report(
    media: &gsm_sip_bridge::ims::call::MediaReport,
    end_reason: gsm_sip_bridge::ims::call::EndReason,
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

fn handle_volte_register_command(args: &gsm_sip_bridge::cli::VolteRegisterArgs) -> ExitCode {
    use gsm_sip_bridge::ims::RegisterOutcome;

    // Refuse to displace a live VoWiFi registration. Checked before anything
    // touches the modem, so a refusal leaves the system exactly as it was.
    if let Err(e) = gsm_sip_bridge::volte::guard::check_no_vowifi_conflict(args.force) {
        eprintln!("volte-register: {e}");
        return ExitCode::FAILURE;
    }
    let _lock = match gsm_sip_bridge::volte::guard::RegistrationGuard::acquire(&args.lock_path) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("volte-register: {e}");
            return ExitCode::FAILURE;
        }
    };

    // P-CSCF resolution order: explicit flag, then the address captured by the
    // VoWiFi/ePDG path. Automatic discovery is not consulted here because it
    // is known not to yield an address on the tested carrier and would only
    // add latency before a failure the operator can already act on.
    let (pcscf_addr, pcscf_source) = match args.pcscf {
        Some(addr) => (addr, "--pcscf".to_string()),
        None => {
            let cache = std::path::PathBuf::from(&args.pcscf_source_path);
            match gsm_sip_bridge::volte::pcscf::probe_epdg_cache(&cache).found() {
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

    let settings = gsm_sip_bridge::volte::VolteSettings {
        modem_port: args.modem.clone(),
        iface: args.iface.clone().unwrap_or_default(),
        cid: args.cid,
        apn: args.apn.clone(),
        pcscf: Some(std::net::SocketAddr::new(pcscf_addr, args.pcscf_port)),
        // With --keep-pdn this process does not detach; an external teardown
        // does, and reads this file to restore the displaced context. Without
        // --keep-pdn it detaches itself and the path is simply unset.
        restore_cid_path: args.restore_cid_path.clone(),
    };

    // Stage 1: the network attachment. Reported separately so a failure here
    // is never mistaken for a credential problem (FR-015).
    let attach = match gsm_sip_bridge::volte::attach(&settings) {
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
    let plmn = match gsm_sip_bridge::modules::at_commander::AtCommander::open(&args.modem)
        .and_then(|mut at| gsm_sip_bridge::vowifi::plmn::derive_plmn(&mut at))
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
    let reg_cfg = gsm_sip_bridge::ims::ImsRegisterConfig {
        modem_port: args.modem.clone(),
        pcscf_addr,
        pcscf_port: args.pcscf_port,
        mcc: plmn.mcc.clone(),
        mnc: plmn.mnc.clone(),
        imsi: None,
        imei: None,
        use_tcp: args.tcp,
        sec_agree: args.sec_agree,
        msisdn: args.msisdn.clone(),
        access_network_info: gsm_sip_bridge::volte::read_access_network_info(&args.modem),
    };

    // Staying up and renewing is the default; --once is the one-shot
    // diagnostic. A rejected first attempt never enters the renewal loop.
    let result = gsm_sip_bridge::volte::registration::run(
        &reg_cfg,
        // Lets the renewal loop re-establish the PDN when it drops; without
        // it a dropped attachment is unrecoverable.
        Some(&settings),
        args.once,
        &args.status_path,
        gsm_sip_bridge::ims::DEFAULT_EXPIRES,
    );

    if !args.keep_pdn {
        if let Err(e) = gsm_sip_bridge::volte::detach(&settings, attach.displaced_cid) {
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

fn handle_volte_discover_command(args: &gsm_sip_bridge::cli::VolteDiscoverArgs) -> ExitCode {
    use gsm_sip_bridge::cli::VolteDiscoverMethod;
    use gsm_sip_bridge::volte::pcscf::{self, DiscoveryInputs, DiscoveryMethod};

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
        _ => match gsm_sip_bridge::modules::at_commander::AtCommander::open(&args.modem)
            .and_then(|mut at| gsm_sip_bridge::vowifi::plmn::derive_plmn(&mut at))
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
fn handle_volte_bridge_command(
    args: &gsm_sip_bridge::cli::VolteBridgeArgs,
    cli: &gsm_sip_bridge::cli::Cli,
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
    // `docker/entrypoint.sh` inside that line's namespace.
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

    gsm_sip_bridge::volte::bridge::run(
        gsm_sip_bridge::volte::bridge::ServiceConfig {
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
    args: &gsm_sip_bridge::cli::VolteBridgeArgs,
    modem: &std::path::Path,
) -> Result<Vec<gsm_sip_bridge::volte::bridge::BridgeLine>, String> {
    use gsm_sip_bridge::volte::discovery;
    let explicit = args.pcscf.map(|a| a.to_string());
    let Some(pcscf) = resolve_line_pcscf(explicit, args.pcscf_port, &args.pcscf_source_path) else {
        return Err("[discovering-pcscf] no P-CSCF address available; pass --pcscf".to_string());
    };
    let card_id = args
        .card_id
        .clone()
        .unwrap_or_else(|| gsm_sip_bridge::volte::bridge::DEFAULT_CARD_ID.to_string());
    let settings = gsm_sip_bridge::volte::VolteSettings {
        modem_port: modem.to_path_buf(),
        iface: args.iface.clone().unwrap_or_default(),
        cid: args.cid,
        apn: args.apn.clone(),
        pcscf: Some(pcscf),
        restore_cid_path: args.restore_cid_path.clone(),
    };
    Ok(vec![gsm_sip_bridge::volte::bridge::BridgeLine {
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
fn volte_bridge_manifest_lines(
    volte: &gsm_sip_bridge::config::VolteConfig,
    pcscf_port: u16,
) -> Result<Vec<gsm_sip_bridge::volte::bridge::BridgeLine>, String> {
    use gsm_sip_bridge::volte::discovery;

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
                "no P-CSCF available for this line (none configured and none captured by the \
                 ePDG path); skipping it"
            );
            continue;
        };
        let settings = gsm_sip_bridge::volte::VolteSettings {
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
        lines.push(gsm_sip_bridge::volte::bridge::BridgeLine {
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
/// Run once, up front, by `docker/entrypoint.sh` before any per-line
/// namespace or process exists.
fn handle_volte_discover_lines_command(
    args: &gsm_sip_bridge::cli::VolteDiscoverLinesArgs,
    cli: &gsm_sip_bridge::cli::Cli,
) -> ExitCode {
    use gsm_sip_bridge::volte::discovery;

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
    let modems = match gsm_sip_bridge::modules::discovery::scan_all_preferring(&preferred) {
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

    // stderr, not stdout: `docker/entrypoint.sh` captures this command's
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
        print_volte_discover_lines_shell_env(&table.lines);
    }

    ExitCode::SUCCESS
}

/// Bash indexed-array output for `docker/entrypoint.sh`'s VoLTE per-line
/// loop to `eval` — mirrors `print_discover_shell_env`'s array convention
/// exactly (`LINE_CARD_ID=(...)`, indexed by position, not per-index scalar
/// variables) so both subsystems' entrypoint loops read the same shape.
fn print_volte_discover_lines_shell_env(
    lines: &[gsm_sip_bridge::volte::discovery::ResolvedVolteLine],
) {
    fn arr<T: ToString>(vals: impl Iterator<Item = T>) -> String {
        format!(
            "({})",
            vals.map(|v| shell_quote(&v.to_string()))
                .collect::<Vec<_>>()
                .join(" ")
        )
    }

    println!("VOLTE_LINE_COUNT={}", lines.len());
    println!(
        "VOLTE_LINE_CARD_ID={}",
        arr(lines.iter().map(|l| l.card_id.clone()))
    );
    println!(
        "VOLTE_LINE_MODEM_PORT={}",
        arr(lines.iter().map(|l| l.modem_port.display().to_string()))
    );
    println!(
        "VOLTE_LINE_IFACE={}",
        arr(lines.iter().map(|l| l.iface.clone()))
    );
    println!(
        "VOLTE_LINE_NETNS={}",
        arr(lines.iter().map(|l| l.netns.clone()))
    );
    println!(
        "VOLTE_LINE_VETH_CARRIER_IFACE={}",
        arr(lines.iter().map(|l| l.veth_carrier_iface.clone()))
    );
    println!(
        "VOLTE_LINE_VETH_TELEPHONY_IFACE={}",
        arr(lines.iter().map(|l| l.veth_telephony_iface.clone()))
    );
    println!(
        "VOLTE_LINE_VETH_CARRIER_ADDR={}",
        arr(lines.iter().map(|l| l.veth_carrier_addr.clone()))
    );
    println!(
        "VOLTE_LINE_VETH_TELEPHONY_ADDR={}",
        arr(lines.iter().map(|l| l.veth_telephony_addr.clone()))
    );
}

/// The per-line carrier-facing half (specs/020-volte-line-netns) — reads its
/// settings from the manifest `volte-discover-lines` wrote, attaches this
/// line's IMS PDN, registers, and answers calls until the registration ends.
/// One-shot: does not retry internally (`docker/entrypoint.sh` restarts it on
/// exit, mirroring `vowifi-ims-agent`'s supervision).
fn handle_volte_carrier_agent_command(
    args: &gsm_sip_bridge::cli::VolteCarrierAgentArgs,
    cli: &gsm_sip_bridge::cli::Cli,
) -> ExitCode {
    use gsm_sip_bridge::volte::discovery;

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

    let settings = gsm_sip_bridge::volte::VolteSettings {
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
    let line = gsm_sip_bridge::volte::bridge::BridgeLine {
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
    let modem_lock = std::sync::Arc::new(std::sync::Mutex::new(()));
    {
        let modem_port = modem_port.clone();
        let lock = modem_lock.clone();
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
                gsm_sip_bridge::volte::sms::run_modem_reader(modem_port, control_addr, lock)
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
    gsm_sip_bridge::volte::carrier_agent::run(&line, &app_config, modem_lock, None);

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
    gsm_sip_bridge::volte::pcscf::probe_epdg_cache(&cache)
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
fn handle_volte_cleanup_command(line: Option<u32>) -> ExitCode {
    use gsm_sip_bridge::volte::discovery;
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
        let settings = gsm_sip_bridge::volte::VolteSettings {
            modem_port: std::path::PathBuf::from(&entry.modem_port),
            iface: entry.iface.clone(),
            cid: entry.cid,
            // `detach` uses only the modem port, interface, cid and restore-cid.
            apn: String::new(),
            pcscf: None,
            restore_cid_path: None,
        };
        match gsm_sip_bridge::volte::detach(&settings, restore_cid) {
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

fn handle_config_command(args: &gsm_sip_bridge::cli::ConfigArgs, cli: &Cli) -> ExitCode {
    use gsm_sip_bridge::cli::ConfigSubcommand;
    match &args.subcommand {
        ConfigSubcommand::VowifiEnabled => {
            let Some(path) = cli.config.as_deref() else {
                return ExitCode::FAILURE;
            };
            match load_config(path) {
                Ok(config) if config.vowifi.enabled => ExitCode::SUCCESS,
                _ => ExitCode::FAILURE,
            }
        }
        ConfigSubcommand::VolteEnabled => {
            let Some(path) = cli.config.as_deref() else {
                return ExitCode::FAILURE;
            };
            match load_config(path) {
                Ok(config) if config.volte.enabled => ExitCode::SUCCESS,
                _ => ExitCode::FAILURE,
            }
        }
        ConfigSubcommand::VolteShellEnv => {
            let Some(path) = cli.config.as_deref() else {
                eprintln!("config volte-shell-env: --config is required");
                return ExitCode::FAILURE;
            };
            let config = match load_config(path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("config volte-shell-env: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // Global-only: per-line values (modem_port/iface/cid/apn/pcscf/
            // pcscf_port) live in `[[volte.line]]`, read directly by
            // `volte-bridge` from config.toml — there is nothing to derive
            // as a shell var here anymore.
            let v = &config.volte;
            let q = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
            println!("VOLTE_ENABLED={}", if v.enabled { 1 } else { 0 });
            println!("VOLTE_PCSCF_SOURCE_PATH={}", q(&v.pcscf_source_path));
            println!("VOLTE_STATUS_PATH={}", q(&v.status_path));
            println!("VOLTE_LOCK_PATH={}", q(&v.lock_path));
            println!(
                "VOLTE_BRIDGE_INBOUND={}",
                if v.bridge_inbound { 1 } else { 0 }
            );
            println!("VOLTE_MAX_LINES={}", v.max_lines);
            ExitCode::SUCCESS
        }
        ConfigSubcommand::VowifiShellEnv => {
            let Some(path) = cli.config.as_deref() else {
                eprintln!("config vowifi-shell-env: --config is required");
                return ExitCode::FAILURE;
            };
            match load_config(path) {
                Ok(config) => {
                    print_vowifi_shell_env(&config);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("config vowifi-shell-env: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

/// Single-quotes `s` for safe use as a POSIX shell word, escaping any
/// embedded single quotes (`'` -> `'\''`).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn print_vowifi_shell_env(config: &gsm_sip_bridge::config::AppConfig) {
    // Global-only: per-line values (mcc/mnc/modem_port/netns/veth
    // names+addrs/strongswan iface+if_id/vpcd_port) come from
    // `discover --shell-env` instead — see `print_discover_shell_env`.
    let v = &config.vowifi;
    let lines: Vec<(&str, String)> = vec![
        ("APN", v.apn.clone()),
        ("EPDG_FQDN", v.epdg_fqdn.clone()),
        ("EPDG_IP", v.epdg_ip.clone().unwrap_or_default()),
        ("SRC_ADDR", v.src_addr.clone().unwrap_or_default()),
        ("KEEPALIVE_INTERVAL", v.keepalive_interval_sec.to_string()),
        ("TUNNEL_ENGINE", v.tunnel_engine.clone()),
        ("VPCD_HOST", v.vpcd_host.clone()),
        ("VPCD_PORT", v.vpcd_port.to_string()),
        ("METRICS_PORT", config.metrics.port.to_string()),
    ];
    for (key, value) in lines {
        println!("{key}={}", shell_quote(&value));
    }
}

/// `gsm-sip-bridge discover` (specs/013-multi-card-vowifi,
/// contracts/discover-cli-contract.md): runs the shared scan + VoWiFi role
/// assignment/line-table resolution exactly once, writes it to `--out` (JSON,
/// consumed by `main()`'s daemon-startup path via
/// `modules::discovery::scan_modules`'s own exclusion read and by
/// `--line`-selecting `vowifi-ims-agent`/`vowifi-status`), and optionally
/// prints `entrypoint.sh`-`eval`-able shell output.
fn handle_discover_command(args: &gsm_sip_bridge::cli::DiscoverArgs, cli: &Cli) -> ExitCode {
    let out_path = args.out.clone().unwrap_or_else(lines_file_path);

    if args.from_file {
        let resolution =
            gsm_sip_bridge::vowifi::discovery::read_line_resolution(&out_path).unwrap_or_default();
        if args.shell_env {
            print_discover_shell_env(&resolution);
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
        gsm_sip_bridge::vowifi::discovery::LineResolution::default()
    } else {
        let overrides = gsm_sip_bridge::vowifi::discovery::effective_line_overrides(&config.vowifi);
        // A device with several AT-capable interfaces means an override's
        // named port isn't necessarily the one the plain first-match probe
        // would settle on (found live-testing an EC200 that answers AT on
        // more than one ttyUSB) — pass every configured port as a
        // preference so probing tries it first on that device.
        let preferred_ports: Vec<std::path::PathBuf> = overrides
            .iter()
            .filter_map(|o| o.modem_port.as_deref().map(std::path::PathBuf::from))
            .collect();
        let modems = match gsm_sip_bridge::modules::discovery::scan_all_preferring(&preferred_ports)
        {
            Ok(m) => m,
            Err(e) => {
                eprintln!("error: modem discovery failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        let assignment =
            gsm_sip_bridge::vowifi::discovery::RoleAssignment::from_probed(&modems, &overrides);
        let result = gsm_sip_bridge::vowifi::discovery::resolve_lines(&assignment, &config.vowifi);
        for failed in &result.failed {
            tracing::error!(
                card_id = %failed.card_id,
                reason = %failed.reason,
                "VoWiFi line discovery: modem not usable as a line"
            );
        }
        if result.lines.is_empty() {
            // The spec's clarification: degrade, don't fail — the caller
            // (entrypoint.sh) still starts the circuit-switched daemon.
            tracing::error!(
                "[vowifi].enabled is true but no usable VoWiFi line was discovered; \
                 the VoWiFi subsystem will not start this run"
            );
        }
        gsm_sip_bridge::vowifi::discovery::LineResolution::from_result(&assignment.vowifi, &result)
    };

    if let Err(e) = write_line_resolution(&out_path, &resolution) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    if args.shell_env {
        print_discover_shell_env(&resolution);
    }
    ExitCode::SUCCESS
}

fn write_line_resolution(
    path: &std::path::Path,
    resolution: &gsm_sip_bridge::vowifi::discovery::LineResolution,
) -> Result<(), String> {
    let json = serde_json::to_string_pretty(resolution)
        .map_err(|e| format!("failed to serialize line resolution: {e}"))?;
    std::fs::write(path, json).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

fn print_discover_shell_env(resolution: &gsm_sip_bridge::vowifi::discovery::LineResolution) {
    fn arr<T: ToString>(vals: impl Iterator<Item = T>) -> String {
        format!(
            "({})",
            vals.map(|v| shell_quote(&v.to_string()))
                .collect::<Vec<_>>()
                .join(" ")
        )
    }

    println!("LINE_COUNT={}", resolution.lines.len());
    println!(
        "LINE_CARD_ID={}",
        arr(resolution.lines.iter().map(|l| l.card_id.clone()))
    );
    println!(
        "LINE_MODEM_PORT={}",
        arr(resolution.lines.iter().map(|l| l.modem_port.clone()))
    );
    println!(
        "LINE_NETNS={}",
        arr(resolution.lines.iter().map(|l| l.netns.clone()))
    );
    println!(
        "LINE_CONTROL_PORT={}",
        arr(resolution.lines.iter().map(|l| l.control_port))
    );
    println!(
        "LINE_VETH_LOCAL_ADDR={}",
        arr(resolution.lines.iter().map(|l| l.veth_local_addr.clone()))
    );
    println!(
        "LINE_VETH_PEER_ADDR={}",
        arr(resolution.lines.iter().map(|l| l.veth_peer_addr.clone()))
    );
    println!(
        "LINE_VPCD_PORT={}",
        arr(resolution.lines.iter().map(|l| l.vpcd_port))
    );
    println!(
        "LINE_STRONGSWAN_IF_ID={}",
        arr(resolution.lines.iter().map(|l| l.strongswan_if_id))
    );
    println!(
        "LINE_STRONGSWAN_TUN_IFACE={}",
        arr(resolution
            .lines
            .iter()
            .map(|l| l.strongswan_tun_iface.clone()))
    );
    println!(
        "LINE_PCSCF_SOURCE_PATH={}",
        arr(resolution.lines.iter().map(|l| l.pcscf_source_path.clone()))
    );
    println!(
        "LINE_VETH_SIP_IFACE={}",
        arr(resolution
            .lines
            .iter()
            .map(|l| l.config.veth_sip_iface.clone()))
    );
    println!(
        "LINE_VETH_IMS_IFACE={}",
        arr(resolution
            .lines
            .iter()
            .map(|l| l.config.veth_ims_iface.clone()))
    );
    println!(
        "LINE_MCC={}",
        arr(resolution.lines.iter().map(|l| l.mcc.clone()))
    );
    println!(
        "LINE_MNC={}",
        arr(resolution.lines.iter().map(|l| l.mnc.clone()))
    );
    println!(
        "LINE_IMSI={}",
        arr(resolution
            .lines
            .iter()
            .map(|l| l.config.imsi_override.clone().unwrap_or_default()))
    );
    println!(
        "CS_EXCLUDED_PORTS={}",
        arr(resolution.circuit_switched_excluded_ports.iter().cloned())
    );
}

fn build_control_cmd(args: &gsm_sip_bridge::cli::CardArgs) -> Result<ControlCmd, String> {
    use gsm_sip_bridge::cli::CardSubcommand;
    match &args.subcommand {
        CardSubcommand::Restart { slot } => Ok(ControlCmd::CardRestart { slot: *slot }),
        CardSubcommand::SetMode { slot, mode } => Ok(ControlCmd::SetMode {
            slot: *slot,
            mode: mode.clone(),
        }),
        CardSubcommand::GetMode { slot } => Ok(ControlCmd::GetMode { slot: *slot }),
        CardSubcommand::List => Ok(ControlCmd::ListSlots),
    }
}

fn print_resp(resp: ControlResp) -> ExitCode {
    match resp {
        ControlResp::Ok => {
            println!("ok");
            ExitCode::SUCCESS
        }
        ControlResp::OkMode { mode } => {
            println!("mode: {mode}");
            ExitCode::SUCCESS
        }
        ControlResp::OkSlots { slots } => {
            if slots.is_empty() {
                println!("no slots registered");
            } else {
                println!("{:<6} {:<14} {:<20} network", "slot", "state", "phone");
                println!("{}", "-".repeat(60));
                for s in slots {
                    println!(
                        "{:<6} {:<14} {:<20} {}",
                        s.slot, s.state, s.phone, s.network
                    );
                }
            }
            ExitCode::SUCCESS
        }
        ControlResp::Err { error } => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
