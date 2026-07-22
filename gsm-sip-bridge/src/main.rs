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
    };

    match run_call(&cfg) {
        Ok(CallOutcome::Answered {
            recorded_path,
            recorded_samples,
            sent_path,
            sent_samples,
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

/// Without `--line`, behaves exactly as before this feature (the single
/// `[vowifi]` config section — FR-020). With `--line N`, loads that line's
/// fully-derived `VowifiConfig` from the `discover` subcommand's
/// line-resolution file instead — see
/// `specs/013-multi-card-vowifi/contracts/agent-topology-contract.md`.
/// Deliberately does NOT re-run discovery itself: doing so would re-probe
/// modems a sibling `vowifi-usim-bridge`/other line's agent may already have
/// open (research.md item 3).
fn handle_vowifi_ims_agent_command(cli: &Cli, line: Option<u32>) -> ExitCode {
    let config = match load_vowifi_config(cli) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let Some(index) = line else {
        return gsm_sip_bridge::ims::agent::run(
            gsm_sip_bridge::vowifi::LEGACY_LINE_CARD_ID,
            &config.vowifi,
            &config,
        );
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
    gsm_sip_bridge::vowifi::ims_mode::run(&args.modem, config.vowifi.enabled)
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
    let settings = volte_settings(&args.modem, &args.iface, args.cid, &args.apn);
    match gsm_sip_bridge::volte::status(&settings) {
        Ok(Some(report)) => {
            print!("{}", report.summary());
            // Registration state is reported here once registration over LTE
            // lands (US3/US4); it is blocked on obtaining a P-CSCF address
            // (specs/015-volte-host-ims, Gate G3).
            println!("  registration   : not implemented (blocked on Gate G3)");
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
    let v = &config.vowifi;
    let lines: Vec<(&str, String)> = vec![
        ("MCC", v.mcc.clone()),
        ("MNC", v.mnc.clone()),
        ("APN", v.apn.clone()),
        ("MODEM_PORT", v.modem_port.clone()),
        ("NETNS", v.netns.clone()),
        ("EPDG_FQDN", v.epdg_fqdn.clone()),
        ("EPDG_IP", v.epdg_ip.clone().unwrap_or_default()),
        ("SRC_ADDR", v.src_addr.clone().unwrap_or_default()),
        ("KEEPALIVE_INTERVAL", v.keepalive_interval_sec.to_string()),
        ("VETH_SIP", v.veth_sip_iface.clone()),
        ("VETH_IMS", v.veth_ims_iface.clone()),
        ("VETH_IMS_ADDR", format!("{}/30", v.veth_local_addr)),
        ("VETH_SIP_ADDR", format!("{}/30", v.veth_peer_addr)),
        ("TUNNEL_ENGINE", v.tunnel_engine.clone()),
        ("STRONGSWAN_TUN_IFACE", v.strongswan_tun_iface.clone()),
        ("STRONGSWAN_IF_ID", v.strongswan_if_id.to_string()),
        ("VPCD_HOST", v.vpcd_host.clone()),
        ("VPCD_PORT", v.vpcd_port.to_string()),
        ("IMSI", v.imsi_override.clone().unwrap_or_default()),
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
        gsm_sip_bridge::vowifi::discovery::LineResolution::from_result(
            &assignment.circuit_switched,
            &result,
        )
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
