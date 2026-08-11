//! The no-subcommand default: the long-running circuit-switched GSM->SIP
//! daemon (`CardPool` + SIP bridge + SMS handler + metrics/control servers).

use crate::alerts::discord::DiscordClient;
use crate::cli::Cli;
use crate::config::load_config;
use crate::config::secret::Secret;
use crate::config::AppConfig;
use crate::control::server::start_control_server;
use crate::metrics;
use crate::modules::{CardPool, ControlCmdSender};
use crate::observability::modemmanager;
use crate::runtime;
use crate::sip::SipBridge;
use crate::sms::SmsHandler;
use crate::store::StoreHandle;
use std::process::ExitCode;
use tokio::sync::{mpsc, watch};

/// Pure startup-time decisions derived from config and the CLI single-card
/// override, extracted from [`run`] so they're testable without a live tokio
/// runtime, real OS signals, or hardware (specs/026-disable-circuit-switched).
/// Mirrors the seam `commands::healthcheck::evaluate` already uses for the
/// same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartupPlan {
    /// Whether to construct and run `CardPool` this run (FR-005..FR-010).
    pub circuit_switched: bool,
    /// FR-023: neither `[cs]`, `[vowifi]`, nor `[volte]` carries a call this
    /// run — degenerate but valid (metrics/history only), not fatal.
    pub warn_no_call_path: bool,
    /// FR-024: `[sms].enabled` is true but nothing will ever poll for SMS —
    /// the circuit-switched path is off and no VoWiFi/VoLTE line is
    /// configured to supply messages either. A config-time proxy for "no
    /// line configured" (neither subsystem enabled), not a discovery-time
    /// check — deliberately conservative, since this is a warning, not a
    /// blocking error.
    pub warn_sms_orphaned: bool,
    /// FR-026: `--serial`/`--audio` given while `[cs].enabled` is false. The
    /// override does not resurrect the circuit-switched path.
    pub warn_cli_override_ignored: bool,
}

/// Builds the plan. `single_card_requested` is `cli.serial` and `cli.audio`
/// both being present — the same condition `run` already uses to enter
/// single-card override mode.
pub fn plan_startup(config: &AppConfig, single_card_requested: bool) -> StartupPlan {
    let circuit_switched = config.cs.enabled;
    let any_call_path = circuit_switched || config.vowifi.enabled || config.volte.enabled;
    StartupPlan {
        circuit_switched,
        warn_no_call_path: !any_call_path,
        warn_sms_orphaned: !circuit_switched
            && config.sms.enabled
            && !config.vowifi.enabled
            && !config.volte.enabled,
        warn_cli_override_ignored: !circuit_switched && single_card_requested,
    }
}

/// Emits the startup log lines `plan` implies. Split out from
/// [`plan_startup`] (a pure function) so the log content is directly
/// testable via a captured `tracing` subscriber, the pattern
/// `tests/test_logging.rs` already establishes.
pub fn log_startup_plan(plan: &StartupPlan) {
    tracing::info!(
        cs_enabled = plan.circuit_switched,
        "circuit-switched path status"
    );
    if plan.warn_no_call_path {
        tracing::warn!(
            "no call path is active ([cs].enabled=false, [vowifi].enabled=false, \
             [volte].enabled=false) — this process serves metrics and stored history only, \
             and will establish no telephone-facing registration"
        );
    }
    if plan.warn_sms_orphaned {
        tracing::warn!(
            "[sms].enabled=true but [cs].enabled=false and no VoWiFi/VoLTE line is \
             configured — message forwarding has no active source"
        );
    }
    if plan.warn_cli_override_ignored {
        tracing::warn!(
            "--serial/--audio given but [cs].enabled=false — the circuit-switched path \
             stays disabled and the override is ignored"
        );
    }
}

/// Sets the metrics that must be present in **both** states, unconditionally
/// — unlike `MODULES_ACTIVE`/`MODULES_FAILED`/etc, which are `Lazy` and only
/// ever dereferenced from inside `CardPool`, and so are never registered at
/// all when the circuit-switched path is off (FR-021a). `CS_ENABLED` is the
/// deliberate exception (FR-021b).
pub fn apply_startup_metrics(plan: &StartupPlan) {
    metrics::CS_ENABLED.set(if plan.circuit_switched { 1.0 } else { 0.0 });
}

pub fn run(cli: &Cli) -> ExitCode {
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

        // specs/022-discord-critical-alerts (Greptile P1/P2 fix): alert
        // evaluation lives in metrics::ingest, keyed to real AgentReport
        // arrival rather than an external Prometheus scrape — this must be
        // wired up before start_control_server below starts accepting the
        // reports that trigger it.
        match DiscordClient::new(
            Secret::new(String::new()),
            crate::alerts::instance_label(&config.alerts),
        ) {
            Ok(client) => metrics::ingest::init_alerts(
                config.alerts.clone(),
                client,
                crate::alerts::line_phone_map(&config),
            ),
            Err(e) => {
                tracing::error!(error = %e, "failed to create critical-alerts Discord client")
            }
        }

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

        // specs/026-disable-circuit-switched: decided once, up front, so
        // every consequence (which task gets spawned, which logs/warnings
        // fire, the CS_ENABLED gauge) reads from the same plan rather than
        // re-deriving `config.cs.enabled` in several places.
        let plan = plan_startup(&config, single_card.is_some());
        log_startup_plan(&plan);
        apply_startup_metrics(&plan);

        let (shutdown_watch_tx, shutdown_watch_rx) = watch::channel(false);

        let ctrl_server = start_control_server(&socket_path, control_tx, shutdown_watch_rx).await;

        // Either branch yields a `JoinHandle<()>` that runs until aborted at
        // shutdown below — `control::disabled::run` takes `CardPool::run`'s
        // place as the control channel's sole consumer so a card command
        // gets a clear "disabled" reply instead of the ambiguous generic one
        // a dropped receiver would produce (contracts/control-protocol.md).
        let pool_handle = if plan.circuit_switched {
            let sip_bridge = SipBridge::new(&config);
            let sms_handler = SmsHandler::new(&config.sms, store.sender());
            let card_pool = CardPool::new(config, store, sip_bridge, sms_handler);
            tokio::spawn(async move {
                card_pool.run(single_card, shutdown_rx, control_rx).await;
            })
        } else {
            tokio::spawn(crate::control::disabled::run(control_rx))
        };

        runtime::wait_for_shutdown(shutdown_tx).await;

        let _ = shutdown_watch_tx.send(true);
        ctrl_server.abort();
        pool_handle.abort();
        metrics_handle.abort();
    });

    tracing::info!("shutdown complete");
    ExitCode::SUCCESS
}
