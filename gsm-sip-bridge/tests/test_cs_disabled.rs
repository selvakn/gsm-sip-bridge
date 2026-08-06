//! specs/026-disable-circuit-switched: the circuit-switched path can be
//! turned off via `[cs].enabled = false` while the daemon process keeps
//! hosting the metrics endpoint, control socket, and message store.
//!
//! `commands::daemon::run` itself blocks on a real OS signal
//! (`runtime::wait_for_signal`) and can't be driven directly from a test
//! without either sending a real signal to the test process (which would
//! also affect every other test running in the same process) or hardware
//! this environment doesn't have. So these tests exercise the pieces `run`
//! is built from — `plan_startup`/`log_startup_plan`/`apply_startup_metrics`
//! and `control::disabled::run` — the same seam
//! `commands::healthcheck::evaluate` already uses for the same reason.
//! Together with the config tests in `test_config.rs`, this covers every
//! decision `run`'s gate makes; the gate's own one-line `if` is verified by
//! code review and the manual hardware check in
//! `specs/026-disable-circuit-switched/quickstart.md`.

use gsm_sip_bridge::commands::daemon::{apply_startup_metrics, log_startup_plan, plan_startup};
use gsm_sip_bridge::config::load_config;
use gsm_sip_bridge::control::disabled;
use gsm_sip_bridge::control::protocol::{AgentKind, ControlCmd};
use gsm_sip_bridge::control::server::start_control_server;
use gsm_sip_bridge::metrics::ingest::evaluate_liveness;
use gsm_sip_bridge::observability::reporter::Reporter;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::sync::oneshot;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter};

fn write_config(content: &str) -> NamedTempFile {
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f
}

fn load(extra: &str, password_var: &str) -> gsm_sip_bridge::config::AppConfig {
    std::env::set_var(password_var, "p");
    let f = write_config(&format!(
        r#"
[sip]
server = "127.0.0.1"
username = "user"
password = "env:{password_var}"

{extra}
"#
    ));
    load_config(f.path()).unwrap()
}

// --------------------------------------------------------- plan_startup ---

#[test]
fn plan_startup_gates_circuit_switched_on_cs_enabled() {
    let cfg_on = load("[cs]\nenabled = true", "TEST_PLAN_ON_PASSWORD");
    let cfg_off = load("[cs]\nenabled = false", "TEST_PLAN_OFF_PASSWORD");

    assert!(plan_startup(&cfg_on, false).circuit_switched);
    assert!(!plan_startup(&cfg_off, false).circuit_switched);
}

/// User Story 2 / FR-003: with the flag left at its default (on) and VoWiFi
/// also on, both subsystems initialise together exactly as they do today —
/// VoWiFi being enabled must not implicitly suppress the daemon-level
/// decision to run `CardPool`. Config-level acceptance of this combination
/// is covered by `test_config.rs::cs_vowifi_volte_every_combination_is_accepted`;
/// this pins the specific daemon-wiring decision `plan_startup` makes from
/// it.
#[test]
fn plan_startup_runs_circuit_switched_alongside_vowifi_when_both_enabled() {
    let cfg_explicit = load(
        "[cs]\nenabled = true\n[vowifi]\nenabled = true",
        "TEST_PLAN_BOTH_EXPLICIT_PASSWORD",
    );
    let cfg_by_omission = load(
        "[vowifi]\nenabled = true",
        "TEST_PLAN_BOTH_OMITTED_PASSWORD",
    );

    assert!(plan_startup(&cfg_explicit, false).circuit_switched);
    assert!(plan_startup(&cfg_by_omission, false).circuit_switched);
}

/// FR-023: degenerate but valid — metrics/history only, not fatal.
#[test]
fn plan_startup_warns_when_no_call_path_is_active() {
    let cfg = load(
        "[cs]\nenabled = false\n[vowifi]\nenabled = false\n[volte]\nenabled = false",
        "TEST_PLAN_NOCALL_PASSWORD",
    );
    let plan = plan_startup(&cfg, false);
    assert!(plan.warn_no_call_path);
}

#[test]
fn plan_startup_does_not_warn_no_call_path_when_vowifi_covers_it() {
    let cfg = load(
        "[cs]\nenabled = false\n[vowifi]\nenabled = true",
        "TEST_PLAN_VOWIFI_COVERS_PASSWORD",
    );
    let plan = plan_startup(&cfg, false);
    assert!(!plan.warn_no_call_path);
}

#[test]
fn plan_startup_does_not_warn_no_call_path_when_cs_alone_is_enabled() {
    let cfg = load("[cs]\nenabled = true", "TEST_PLAN_CS_ALONE_PASSWORD");
    let plan = plan_startup(&cfg, false);
    assert!(!plan.warn_no_call_path);
}

/// FR-024: SMS forwarding enabled, path off, nothing configured to supply
/// messages.
#[test]
fn plan_startup_warns_when_sms_would_be_orphaned() {
    let cfg = load(
        "[cs]\nenabled = false\n[sms]\nenabled = true\n[vowifi]\nenabled = false\n[volte]\nenabled = false",
        "TEST_PLAN_SMS_ORPHAN_PASSWORD",
    );
    let plan = plan_startup(&cfg, false);
    assert!(plan.warn_sms_orphaned);
}

#[test]
fn plan_startup_does_not_warn_sms_orphaned_when_vowifi_can_supply_it() {
    let cfg = load(
        "[cs]\nenabled = false\n[sms]\nenabled = true\n[vowifi]\nenabled = true",
        "TEST_PLAN_SMS_COVERED_PASSWORD",
    );
    let plan = plan_startup(&cfg, false);
    assert!(!plan.warn_sms_orphaned);
}

#[test]
fn plan_startup_does_not_warn_sms_orphaned_when_sms_itself_is_disabled() {
    let cfg = load(
        "[cs]\nenabled = false\n[sms]\nenabled = false",
        "TEST_PLAN_SMS_OFF_PASSWORD",
    );
    let plan = plan_startup(&cfg, false);
    assert!(!plan.warn_sms_orphaned);
}

/// FR-026: a single-card CLI override does not resurrect the path.
#[test]
fn plan_startup_warns_when_cli_override_given_with_cs_disabled() {
    let cfg = load("[cs]\nenabled = false", "TEST_PLAN_CLI_OVERRIDE_PASSWORD");
    let plan = plan_startup(&cfg, true);
    assert!(plan.warn_cli_override_ignored);
    assert!(!plan.circuit_switched, "the override must not re-enable it");
}

#[test]
fn plan_startup_does_not_warn_cli_override_when_cs_enabled() {
    let cfg = load("[cs]\nenabled = true", "TEST_PLAN_CLI_OK_PASSWORD");
    let plan = plan_startup(&cfg, true);
    assert!(!plan.warn_cli_override_ignored);
}

// ------------------------------------------------------ log_startup_plan --

#[derive(Clone)]
struct CaptureWriter {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl CaptureWriter {
    fn new() -> Self {
        Self {
            buf: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn output(&self) -> String {
        String::from_utf8_lossy(&self.buf.lock().unwrap()).to_string()
    }
}

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// FR-004: the effective `[cs].enabled` value is visible in the log at a
/// level that doesn't require debug logging.
#[test]
fn log_startup_plan_reports_cs_enabled_at_info_level() {
    let capture = CaptureWriter::new();
    let subscriber = tracing_subscriber::registry()
        .with(EnvFilter::new("info"))
        .with(fmt::layer().with_writer(capture.clone()).with_ansi(false));
    let _guard = tracing::subscriber::set_default(subscriber);

    let cfg = load("[cs]\nenabled = false", "TEST_LOG_CS_PASSWORD");
    log_startup_plan(&plan_startup(&cfg, false));

    let output = capture.output();
    assert!(
        output.contains("cs_enabled=false") || output.contains("cs_enabled = false"),
        "got: {output}"
    );
}

/// FR-009b: when the no-call-path warning fires, it's visible without debug
/// logging and doesn't require reading source to understand why.
#[test]
fn log_startup_plan_warns_prominently_with_no_call_path() {
    let capture = CaptureWriter::new();
    let subscriber = tracing_subscriber::registry()
        .with(EnvFilter::new("info"))
        .with(fmt::layer().with_writer(capture.clone()).with_ansi(false));
    let _guard = tracing::subscriber::set_default(subscriber);

    let cfg = load(
        "[cs]\nenabled = false\n[vowifi]\nenabled = false\n[volte]\nenabled = false",
        "TEST_LOG_NOCALL_PASSWORD",
    );
    log_startup_plan(&plan_startup(&cfg, false));

    let output = capture.output();
    assert!(output.contains("no call path is active"), "got: {output}");
}

// -------------------------------------------------- apply_startup_metrics -

/// FR-021b: the status gauge is present in both states — this is the whole
/// point, since it's what lets a scraper distinguish "deliberately
/// disabled" from "process down or scrape broken".
///
/// FR-021a: the circuit-switched series (`modules_active`, `modules_failed`,
/// `module_init_total`, `module_retries_total`, `scheduled_restart_total`)
/// stay *unregistered* — they are `Lazy` and only ever dereferenced inside
/// `CardPool`, which this test never touches. This assertion is only valid
/// because nothing else in this binary touches those statics either; if a
/// future test in this file starts calling them directly, this test would
/// need splitting into its own binary to stay meaningful.
///
/// Both the disabled and enabled cases are asserted in one test function,
/// not two — `CS_ENABLED` is process-global state, and `cargo test` runs
/// test functions within one binary concurrently by default. Two separate
/// tests each doing set-then-immediately-scrape would race on which one's
/// `set()` lands first, making either assertion flaky depending on
/// scheduling. One sequential function has no such window.
#[test]
fn apply_startup_metrics_sets_the_gauge_and_leaves_cs_series_unregistered() {
    let scrape = || {
        prometheus::TextEncoder::new()
            .encode_to_string(&prometheus::gather())
            .unwrap()
    };

    let cfg_off = load("[cs]\nenabled = false", "TEST_METRICS_OFF_PASSWORD");
    apply_startup_metrics(&plan_startup(&cfg_off, false));
    let output = scrape();
    assert!(
        output.contains("gsm_sip_bridge_cs_enabled 0"),
        "got: {output}"
    );
    for absent in [
        "gsm_sip_bridge_modules_active",
        "gsm_sip_bridge_modules_failed",
        "gsm_sip_bridge_module_init_total",
        "gsm_sip_bridge_module_retries_total",
        "gsm_sip_bridge_scheduled_restart_total",
    ] {
        assert!(
            !output.contains(absent),
            "{absent} must be absent, not zero, when the circuit-switched path is disabled — got:\n{output}"
        );
    }

    let cfg_on = load("[cs]\nenabled = true", "TEST_METRICS_ON_PASSWORD");
    apply_startup_metrics(&plan_startup(&cfg_on, false));
    let output = scrape();
    assert!(
        output.contains("gsm_sip_bridge_cs_enabled 1"),
        "got: {output}"
    );
}

// -------------------------------------------------------- control::disabled

/// FR-019, FR-020: every card-targeting command gets an `Err` naming the
/// flag — not a hang, not an ambiguous generic message.
#[tokio::test]
async fn disabled_responder_refuses_every_card_command_naming_the_flag() {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let responder = tokio::spawn(disabled::run(rx));

    for cmd in [
        ControlCmd::ListSlots,
        ControlCmd::CardRestart {
            slot: 0,
            mode: "full".to_string(),
        },
        ControlCmd::SetMode {
            slot: 0,
            mode: "auto".to_string(),
        },
        ControlCmd::GetMode { slot: 0 },
    ] {
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send((cmd, resp_tx)).await.unwrap();
        let resp = resp_rx.await.unwrap();
        match resp {
            gsm_sip_bridge::control::protocol::ControlResp::Err { error } => {
                assert!(error.contains("[cs].enabled"), "got: {error}");
            }
            other => panic!("expected Err naming the flag, got {other:?}"),
        }
    }

    drop(tx);
    responder.await.unwrap();
}

/// FR-014, FR-015: a VoWiFi/VoLTE agent's `Observe` report still reaches
/// `metrics::ingest` with the circuit-switched path off — end to end through
/// a real control socket and the real `control::disabled::run` responder,
/// not just `apply_report` called directly. `control::server::handle_connection`
/// routes `Observe` straight to `metrics::ingest::apply_report` before a
/// command ever reaches the channel `disabled::run` drains (see
/// `ControlCmd::Observe`'s own doc comment) — this test pins that the
/// disabled responder sitting on the other end changes nothing about that.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observe_reports_still_reach_metrics_ingest_with_the_responder_running() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir
        .path()
        .join("cs-disabled-observe-test.sock")
        .to_str()
        .unwrap()
        .to_string();

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let server_handle = start_control_server(&socket_path, cmd_tx, shutdown_rx).await;
    // The production disabled-path consumer — real code, not an unconsumed
    // channel — proves Observe's routing is independent of what (if
    // anything) is on the other end of the card-command channel.
    let responder_handle = tokio::spawn(disabled::run(cmd_rx));

    let module_id = "cs-disabled-observe-agent".to_string();
    let report_interval = Duration::from_millis(80);
    let reporter = Reporter::spawn(
        socket_path.clone(),
        AgentKind::Sip,
        module_id.clone(),
        report_interval,
    );
    let staleness_threshold = report_interval * 3;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if evaluate_liveness(staleness_threshold)
            .into_iter()
            .any(|s| s.agent == AgentKind::Sip && s.module_id == module_id && s.up)
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("Observe report never reached metrics::ingest through the disabled responder");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    drop(reporter);
    let _ = shutdown_tx.send(true);
    server_handle.abort();
    responder_handle.abort();
}

/// FR-019: `ListSlots` specifically must be an `Err`, not an empty
/// `OkSlots` — "disabled" must be distinguishable from "enabled but no
/// cards found".
#[tokio::test]
async fn disabled_responder_list_slots_is_err_not_empty_ok() {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let responder = tokio::spawn(disabled::run(rx));

    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send((ControlCmd::ListSlots, resp_tx)).await.unwrap();
    let resp = resp_rx.await.unwrap();

    assert!(
        matches!(
            resp,
            gsm_sip_bridge::control::protocol::ControlResp::Err { .. }
        ),
        "got: {resp:?}"
    );

    drop(tx);
    responder.await.unwrap();
}
