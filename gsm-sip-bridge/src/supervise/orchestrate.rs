//! Top-level container orchestration (specs/021-entrypoint-supervise-rust
//! Phase 4) — what `docker/entrypoint.sh` used to do itself in bash. 1:1 port
//! of its startup sequencing (discover once up front, mutual-exclusion gate,
//! circuit-switched daemon, VoWiFi per-line loop, clean shutdown), now
//! calling this binary's own already-tested Rust modules in-process instead
//! of shelling out to bash functions.
//!
//! Threading note: functions that need to background their own supervision
//! loop (matching the current script's `(...) &`) take `&Arc<dyn
//! CommandRunner>` so they can `Arc::clone` it into a genuinely `'static`
//! `std::thread::spawn` closure; functions that only ever run synchronously
//! within an already-running supervisor thread (the tested Phase 1-3
//! modules: `line_supervisor`, `sim_recovery`, `epdg_iface`, `vpcd`, `render`)
//! keep taking a plain `&dyn CommandRunner`, unchanged.

use super::engines::{StrongswanEngine, SwuEngine};
use super::line_supervisor::{self, TunnelEngine};
use super::runner::{ChildHandle, ChildSpec, CommandRunner, RealCommandRunner};
use super::shutdown::{self, StartedState};
use super::{daemon_supervisor, epdg_iface, sim_recovery, vpcd};
use crate::alerts::{self, discord::DiscordClient, AlertContext};
use crate::config::secret::Secret;
use crate::config::AppConfig;
use crate::vowifi::discovery::LineResolutionEntry;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// specs/022-discord-critical-alerts FR-004/research.md R2 (T011/T015): the
/// pure decision of whether a `sim_recovery::Action` should raise or clear a
/// module-lifecycle alert for this line, given whether one is already
/// outstanding. `given_up_alerted` is updated in place; the caller only
/// needs to act when this returns `Some`. Separate from `csim_fails`
/// (`sim_recovery::IncidentCounters`), which resets every incident
/// (including a give-up) — this flag alone must persist across incidents so
/// a line stuck permanently CSIM-failing doesn't re-alert every cycle
/// (FR-013).
fn sim_alert_transition(
    action: sim_recovery::Action,
    given_up_alerted: &mut bool,
) -> Option<alerts::CriticalEventKind> {
    match action {
        sim_recovery::Action::GiveUpForThisIncident if !*given_up_alerted => {
            *given_up_alerted = true;
            Some(alerts::CriticalEventKind::Failure)
        }
        sim_recovery::Action::None if *given_up_alerted => {
            *given_up_alerted = false;
            Some(alerts::CriticalEventKind::Recovered)
        }
        _ => None,
    }
}

fn gsm_sip_bridge_bin() -> String {
    std::env::var("GSM_SIP_BRIDGE_BIN")
        .unwrap_or_else(|_| "/usr/local/bin/gsm-sip-bridge".to_string())
}

/// Entry point for `gsm-sip-bridge supervise`.
pub fn run(config_path: &Path) -> std::process::ExitCode {
    use std::process::ExitCode;

    let runner: Arc<dyn CommandRunner> = Arc::new(RealCommandRunner::new());
    let bin = gsm_sip_bridge_bin();
    let config_path_str = config_path.to_string_lossy().to_string();

    let config = match crate::config::load_config(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[supervise] FATAL: {e}");
            return ExitCode::FAILURE;
        }
    };

    // specs/022-discord-critical-alerts (research.md R3): `run` has no
    // ambient Tokio context (invoked straight from `main.rs` as a blocking
    // subcommand), so it builds its own small dedicated runtime once here —
    // the same pattern `vowifi::mod`'s accept loop already establishes for
    // firing the async `DiscordClient` from synchronous code. `run` never
    // returns during normal operation (it supervises for the container's
    // lifetime), so `_alerts_runtime` binding here for the rest of this
    // function's body is equivalent to it living exactly as long as the
    // process does — every `AlertContext::fire` spawns onto its `Handle`.
    let _alerts_runtime = tokio::runtime::Runtime::new();
    let alert_ctx = match &_alerts_runtime {
        Ok(rt) => match DiscordClient::new(Secret::new(String::new())) {
            Ok(client) => Some(AlertContext::new(
                client,
                config.alerts.clone(),
                rt.handle().clone(),
            )),
            Err(e) => {
                eprintln!("[supervise] failed to create critical-alerts Discord client: {e}");
                None
            }
        },
        Err(e) => {
            eprintln!("[supervise] failed to build alerts runtime: {e}");
            None
        }
    };

    let started = Arc::new(Mutex::new(StartedState::default()));
    // Greptile P1 (x2): build_shutdown_plan snapshots StartedState exactly
    // once, then execute_shutdown_plan signals only the handles that were in
    // that snapshot. Every supervision loop below runs forever on its own
    // thread and, left unchecked, would happily spawn a *replacement*
    // process the moment it notices its current one died — including dying
    // from the shutdown plan's own kill signal — leaving that replacement
    // completely outside the plan, never signaled.
    //
    // A plain `AtomicBool`, checked once before each spawn, closes the
    // common case but not all of it: `tick_steady_state`'s `Recovered`
    // outcome can itself spawn a replacement (`engine.restart_process()`)
    // and that whole choreography — signal, wait for the old process, a
    // deliberate 2s sleep for the vici socket, spawn, load-all, initiate —
    // takes multiple real seconds. A loop that checked the flag (false) right
    // before starting that sequence, then finishes it and registers the new
    // handle *after* shutdown has already set the flag and taken its
    // snapshot, still escapes it — found live by Greptile, and correctly:
    // this window is on the order of seconds, not nanoseconds, precisely
    // because of the sleep(2) this same feature added earlier.
    //
    // `RwLock<bool>` makes "check the flag, then maybe spawn and register
    // the handle" one atomic critical section instead of two separate
    // checks: every loop takes a *read* guard across that whole sequence
    // (many lines' recoveries can run in parallel, as before); shutdown
    // takes a *write* guard — which blocks until every outstanding read
    // guard is released — before setting the flag and taking the
    // StartedState snapshot, guaranteeing no recovery is still in flight
    // (and therefore nothing it might still register) at snapshot time.
    let shutting_down = Arc::new(RwLock::new(false));

    // --- 1. Discover once, up front (specs/013-multi-card-vowifi) ---------
    // Resolved BEFORE the circuit-switched daemon supervisor starts below —
    // see docker/entrypoint.sh's own extensive comment on why (both would
    // otherwise probe the same candidate modem's serial port at once).
    let mut vowifi_lines: Vec<LineResolutionEntry> = Vec::new();
    if config.vowifi.enabled {
        match runner.run(&[&bin, "--config", &config_path_str, "discover"]) {
            Ok(o) if o.status.success() => {}
            _ => {
                eprintln!(
                    "[supervise] FATAL: 'discover' failed — see error above (bad config.toml?)"
                );
                return ExitCode::FAILURE;
            }
        }
        let lines_file = crate::modules::discovery::lines_file_path();
        match crate::vowifi::discovery::read_line_resolution(&lines_file) {
            Ok(resolution) => {
                println!(
                    "[supervise] discover: LINE_COUNT={}",
                    resolution.lines.len()
                );
                vowifi_lines = resolution.lines;
            }
            Err(e) => {
                eprintln!("[supervise] FATAL: could not read line resolution: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    // --- VoLTE mutual exclusion --------------------------------------------
    if config.volte.enabled && config.vowifi.enabled {
        eprintln!("[supervise] FATAL: [volte].enabled and [vowifi].enabled are both true. They register the");
        eprintln!("[supervise]        same IMPU with the same instance-id, so each would tear the other's");
        eprintln!("[supervise]        binding down. Enable exactly one.");
        return ExitCode::FAILURE;
    }

    // --- 2. Circuit-switched GSM-to-SIP daemon (always attempted) ---------
    {
        let runner = Arc::clone(&runner);
        let bin = bin.clone();
        let cfg = config_path_str.clone();
        let started = Arc::clone(&started);
        let shutting_down = Arc::clone(&shutting_down);
        std::thread::spawn(move || loop {
            // Held across spawn + registering the handle in StartedState —
            // not just a one-off check — so shutdown's write-lock can't
            // proceed to snapshot StartedState while this critical section
            // is in flight (see shutting_down's own doc comment above).
            let guard = shutting_down.read().unwrap();
            if *guard {
                return;
            }
            match runner.spawn(ChildSpec::new([bin.as_str(), "--config", cfg.as_str()])) {
                Ok(handle) => {
                    started.lock().unwrap().daemon_supervisor = Some(handle);
                    drop(guard);
                    // Poll is_alive() rather than block on wait(): this
                    // handle is stored in StartedState precisely so
                    // execute_shutdown_plan's KillChild step can signal it
                    // later from the shutdown thread — a blocking wait()
                    // here would (per RealCommandRunner::wait()'s own doc
                    // comment) remove it from the tracked table for this
                    // process's entire lifetime, silently defeating that
                    // signal, exactly like the vowifi-usim-bridge holder and
                    // VoLTE supervision loops fixed earlier in this feature.
                    while runner.is_alive(handle) {
                        runner.sleep(Duration::from_secs(1));
                    }
                    println!("[supervise] gsm-sip-bridge daemon exited; restarting in 5s");
                }
                Err(e) => {
                    drop(guard);
                    eprintln!("[supervise] failed to spawn daemon: {e}")
                }
            }
            runner.sleep(daemon_supervisor::RESTART_DELAY);
        });
    }

    // --- 3. Inbound VoWiFi-to-SIP bridge -----------------------------------
    if config.vowifi.enabled {
        if vowifi_lines.is_empty() {
            eprintln!(
                "[supervise] PROMINENT ERROR: [vowifi].enabled is true but no usable VoWiFi line was \
                 discovered (no AT-capable modem with a ready SIM found, or all candidates are already \
                 serving the circuit-switched bridge) — the VoWiFi subsystem will NOT start this run. \
                 The circuit-switched daemon is unaffected and keeps running."
            );
        } else {
            println!(
                "[supervise] [vowifi].enabled — starting {} VoWiFi line(s) (engine: {})",
                vowifi_lines.len(),
                config.vowifi.tunnel_engine
            );

            if let Err(msg) =
                check_pcsc_engine_compatibility(&vowifi_lines, &config.vowifi.tunnel_engine)
            {
                eprintln!("[supervise] FATAL: {msg}");
                return ExitCode::FAILURE;
            }

            if runner
                .run(&["ip", "netns", "add", "__probe"])
                .map(|o| !o.status.success())
                .unwrap_or(true)
            {
                eprintln!(
                    "[supervise] FATAL: cannot create network namespaces — add cap_add: SYS_ADMIN (and NET_ADMIN)"
                );
                return ExitCode::FAILURE;
            }
            let _ = runner.run(&["ip", "netns", "del", "__probe"]);

            if config.vowifi.tunnel_engine == "strongswan" {
                // pcscd is shared by both kinds of line: a modem-backed line
                // reaches its SIM through the vpcd *virtual* reader that
                // `vowifi-usim-bridge` drives, while a `pcsc_reader` line
                // (specs/023-omnikey-pcsc-vowifi) uses a real CCID reader
                // pcscd picks up straight from USB. Only the former needs
                // vpcd, so an all-pcsc deployment must not be held hostage to
                // a virtual reader nothing will ever connect to — provisioning
                // it anyway made a free-standing card-reader deployment die on
                // an unrelated vpcd port bind, and left eap-sim-pcsc iterating
                // over empty vpcd slots ("SCardConnect: No smart card
                // inserted") before reaching the real one.
                let needs_vpcd = needs_vpcd_reader(&vowifi_lines);
                if needs_vpcd {
                    vpcd::write_vpcd_reader_conf(runner.as_ref(), config.vowifi.vpcd_port);
                }
                let Some(pcscd_handle) = vpcd::spawn_pcscd(runner.as_ref()) else {
                    eprintln!("[supervise] FATAL: failed to start pcscd");
                    return ExitCode::FAILURE;
                };
                started.lock().unwrap().pcscd = Some(pcscd_handle);

                if needs_vpcd {
                    println!(
                        "[supervise] started shared pcscd; one vpcd reader, slots from {}",
                        config.vowifi.vpcd_port
                    );
                    match vpcd::wait_for_vpcd_ready(
                        runner.as_ref(),
                        pcscd_handle,
                        &config.vowifi.vpcd_host,
                        config.vowifi.vpcd_port,
                    ) {
                        vpcd::ReadyOutcome::Ready => println!(
                            "[supervise] vpcd reader ready on {}:{}",
                            config.vowifi.vpcd_host, config.vowifi.vpcd_port
                        ),
                        other => {
                            eprintln!(
                                "[supervise] FATAL: pcscd's vpcd reader never came up on {}:{} ({other:?}). \
                                 If pcscd logged 'Address in use', another process holds that port — pick a \
                                 [vowifi].vpcd_port below the ephemeral range.",
                                config.vowifi.vpcd_host, config.vowifi.vpcd_port
                            );
                            return ExitCode::FAILURE;
                        }
                    }
                } else {
                    println!(
                        "[supervise] started shared pcscd; no vpcd reader \
                         (all {} line(s) are card-reader-backed)",
                        vowifi_lines.len()
                    );
                }
            }

            for line in &vowifi_lines {
                let runner = Arc::clone(&runner);
                let bin = bin.clone();
                let cfg = config_path_str.clone();
                let started = Arc::clone(&started);
                let shutting_down = Arc::clone(&shutting_down);
                let config = config.clone();
                let line = line.clone();
                let alert_ctx = alert_ctx.clone();
                std::thread::spawn(move || {
                    start_vowifi_line(
                        &runner,
                        &bin,
                        &cfg,
                        &config,
                        &line,
                        &started,
                        &shutting_down,
                        alert_ctx.as_ref(),
                    );
                });
            }

            // Agent B: one shared process for every line's veth pair.
            {
                let runner = Arc::clone(&runner);
                let bin = bin.clone();
                let cfg = config_path_str.clone();
                let started = Arc::clone(&started);
                let shutting_down = Arc::clone(&shutting_down);
                std::thread::spawn(move || loop {
                    let guard = shutting_down.read().unwrap();
                    if *guard {
                        return;
                    }
                    match runner.spawn(ChildSpec::new([
                        bin.as_str(),
                        "--config",
                        cfg.as_str(),
                        "vowifi-sip-agent",
                    ])) {
                        Ok(handle) => {
                            started.lock().unwrap().sip_agent_supervisor = Some(handle);
                            drop(guard);
                            // See the daemon_supervisor loop above: poll
                            // is_alive(), don't block on wait(), so this
                            // handle (signaled by execute_shutdown_plan via
                            // `sip_agent_supervisor`) stays signalable for
                            // as long as the process is actually alive.
                            while runner.is_alive(handle) {
                                runner.sleep(Duration::from_secs(1));
                            }
                            println!("[supervise] vowifi-sip-agent exited; restarting in 5s");
                        }
                        Err(e) => {
                            drop(guard);
                            eprintln!("[supervise] failed to spawn vowifi-sip-agent: {e}")
                        }
                    }
                    runner.sleep(Duration::from_secs(5));
                });
            }
        }
    } else {
        println!("[supervise] [vowifi].enabled is not true — VoWiFi bridge not started");
    }

    // --- 4. Host-side IMS over LTE ------------------------------------------
    if config.volte.enabled {
        super::orchestrate_volte::start(
            Arc::clone(&runner),
            bin.clone(),
            config_path_str.clone(),
            config.clone(),
            Arc::clone(&started),
            Arc::clone(&shutting_down),
        );
    } else {
        println!("[supervise] [volte].enabled is not true — VoLTE not started");
    }

    // --- 5. Block until SIGINT/SIGTERM, then run the shutdown plan --------
    let rt = match crate::runtime::build_runtime() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[supervise] FATAL: failed to build runtime for signal handling: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(crate::runtime::wait_for_signal());

    // Acquiring the write lock blocks until every currently-held read guard
    // (every supervision loop's in-flight "maybe spawn and register a
    // handle" critical section, including a whole tick_steady_state
    // recovery choreography) has been released — so by the time this
    // returns, nothing still running can register anything into
    // StartedState that the snapshot below won't already see, and any loop
    // that reads the flag afterward will see `true` and skip spawning.
    *shutting_down.write().unwrap() = true;

    println!("[supervise] shutting down ...");
    let state = started.lock().unwrap();
    let plan = shutdown::build_shutdown_plan(&state, &config_path_str);
    shutdown::execute_shutdown_plan(&plan, runner.as_ref());

    ExitCode::SUCCESS
}

/// Reads this line's IMSI: an override from `[[vowifi.line]]` if configured,
/// else `vowifi-imsi --modem`.
fn resolve_imsi(
    runner: &dyn CommandRunner,
    bin: &str,
    line: &LineResolutionEntry,
) -> Option<String> {
    if let Some(imsi) = &line.config.imsi_override {
        if !imsi.is_empty() {
            return Some(imsi.clone());
        }
    }
    let out = runner
        .run(&[bin, "vowifi-imsi", "--modem", &line.modem_port])
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Derives mcc/mnc from the SIM via `vowifi-plmn` when not already set.
fn resolve_mcc_mnc(
    runner: &dyn CommandRunner,
    bin: &str,
    line: &LineResolutionEntry,
) -> Option<(String, String)> {
    if !line.mcc.is_empty() && !line.mnc.is_empty() {
        return Some((line.mcc.clone(), line.mnc.clone()));
    }
    let out = runner
        .run(&[bin, "vowifi-plmn", "--modem", &line.modem_port])
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let mut parts = s.split_whitespace();
    let mcc = parts.next()?.to_string();
    let mnc = parts.next()?.to_string();
    Some((mcc, mnc))
}

fn resolve_epdg_ip(
    runner: &dyn CommandRunner,
    epdg_fqdn_override: &str,
    epdg_ip_override: Option<&str>,
    mcc: &str,
    mnc: &str,
) -> Option<String> {
    if let Some(ip) = epdg_ip_override {
        if !ip.is_empty() {
            return Some(ip.to_string());
        }
    }
    let fqdn = if !epdg_fqdn_override.is_empty() {
        epdg_fqdn_override.to_string()
    } else {
        format!("epdg.epc.mnc{mnc}.mcc{mcc}.pub.3gppnetwork.org")
    };
    let out = runner.run(&["dig", "+short", &fqdn, "A"]).ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .map(str::to_string)
}

/// Whether this line table needs pcscd's vpcd *virtual* reader provisioned.
/// Only a modem-backed line does — it reaches its SIM through the vpcd slot
/// that `vowifi-usim-bridge` drives over AT+CSIM. A `pcsc_reader` line
/// (specs/023-omnikey-pcsc-vowifi) uses a real CCID reader that pcscd picks
/// up from USB by itself, so an all-card-reader deployment needs no vpcd at
/// all and must not fail startup when one can't be bound.
fn needs_vpcd_reader(lines: &[LineResolutionEntry]) -> bool {
    lines.iter().any(|l| !l.pcsc_reader)
}

/// Fail fast on an unsupported combination (specs/023-omnikey-pcsc-vowifi
/// FR-008): the swu engine's Rust dialer only ever talks AT+CSIM to a modem
/// — there is no code path that could serve a pcsc_reader line under it, so
/// this is a pure config-validation concern, checked before any per-line
/// process is spawned rather than left to fail confusingly deep in the
/// swu-specific startup path. `Err` names the offending line's index/card_id.
fn check_pcsc_engine_compatibility(
    lines: &[LineResolutionEntry],
    tunnel_engine: &str,
) -> Result<(), String> {
    if tunnel_engine == "strongswan" {
        return Ok(());
    }
    if let Some(bad) = lines.iter().find(|l| l.pcsc_reader) {
        return Err(format!(
            "line {} ({}) has pcsc_reader = true, but [vowifi].tunnel_engine = {tunnel_engine:?} \
             — a card-reader-backed line requires tunnel_engine = \"strongswan\" (the swu engine \
             has no PC/SC support)",
            bad.index, bad.card_id
        ));
    }
    Ok(())
}

/// One VoWiFi line's full startup — 1:1 port of `start_line_strongswan`/
/// `start_line_swu`'s shared prelude (modem presence, IMS mode reconcile,
/// mcc/mnc derivation), then dispatches to the engine-specific rest.
#[allow(clippy::too_many_arguments)]
fn start_vowifi_line(
    runner: &Arc<dyn CommandRunner>,
    bin: &str,
    config_path: &str,
    config: &AppConfig,
    line: &LineResolutionEntry,
    started: &Arc<Mutex<StartedState>>,
    shutting_down: &Arc<RwLock<bool>>,
    alert_ctx: Option<&AlertContext>,
) {
    let idx = line.index;
    let modem = line.modem_port.clone();
    if line.pcsc_reader {
        println!(
            "[supervise] line {idx} ({}): pcsc_reader (no modem)",
            line.card_id
        );
    } else {
        println!("[supervise] line {idx} ({}): modem={modem}", line.card_id);

        if !std::path::Path::new(&modem).exists() {
            eprintln!("[supervise] line {idx}: FATAL: modem port {modem} not present in container; skipping this line");
            return;
        }

        if runner
            .run(&[bin, "--config", config_path, "modem-ims", "--modem", &modem])
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!(
                "[supervise] line {idx}: FATAL: could not reconcile modem IMS mode; skipping this line"
            );
            return;
        }
    }

    let Some((mcc, mnc)) = resolve_mcc_mnc(runner.as_ref(), bin, line) else {
        eprintln!("[supervise] line {idx}: FATAL: could not derive MCC/MNC; skipping this line");
        return;
    };

    if config.vowifi.tunnel_engine == "strongswan" {
        start_vowifi_line_strongswan(
            runner,
            bin,
            config_path,
            config,
            line,
            &mcc,
            &mnc,
            started,
            shutting_down,
            alert_ctx,
        );
    } else {
        start_vowifi_line_swu(
            runner,
            bin,
            config_path,
            config,
            line,
            &mcc,
            &mnc,
            started,
            shutting_down,
            alert_ctx,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn start_vowifi_line_strongswan(
    runner: &Arc<dyn CommandRunner>,
    bin: &str,
    config_path: &str,
    config: &AppConfig,
    line: &LineResolutionEntry,
    mcc: &str,
    mnc: &str,
    started: &Arc<Mutex<StartedState>>,
    shutting_down: &Arc<RwLock<bool>>,
    alert_ctx: Option<&AlertContext>,
) {
    let idx = line.index;
    let modem = line.modem_port.clone();
    let netns = line.netns.clone();
    let tun_iface = line.strongswan_tun_iface.clone();
    let if_id = line.strongswan_if_id.to_string();
    let charon_log = PathBuf::from(format!("/tmp/charon-{idx}.log"));
    let vici_socket = format!("/var/run/charon-{idx}.vici");

    let Some(epdg_ip) = resolve_epdg_ip(
        runner.as_ref(),
        &config.vowifi.epdg_fqdn,
        config.vowifi.epdg_ip.as_deref(),
        mcc,
        mnc,
    ) else {
        eprintln!(
            "[supervise] line {idx}: FATAL: could not resolve ePDG address; skipping this line"
        );
        return;
    };

    epdg_iface::ensure_epdg_interface(runner.as_ref(), &netns, &tun_iface, &if_id);
    started.lock().unwrap().started_netns.push(netns.clone());

    let Some(imsi) = resolve_imsi(runner.as_ref(), bin, line) else {
        eprintln!("[supervise] line {idx}: FATAL: failed to read IMSI; skipping this line");
        return;
    };

    // Render strongswan.conf / swanctl.conf(s) — 1:1 with `gsm-sip-bridge render`.
    let strongswan_conf_path = format!("/etc/strongswan-line-{idx}.conf");
    let strongswan_conf =
        super::render::render_strongswan_conf(idx, &vici_socket, &charon_log.to_string_lossy());
    let _ = runner.write_file(Path::new(&strongswan_conf_path), &strongswan_conf);

    let swanctl_conf_dir = format!("/etc/swanctl/conf.d-{idx}");
    let _ = runner.run(&["mkdir", "-p", &swanctl_conf_dir]);
    let swanctl_top_conf_path = format!("/etc/swanctl-line-{idx}.conf");
    let swanctl_top_conf = super::render::render_swanctl_top_conf(&swanctl_conf_dir);
    let _ = runner.write_file(Path::new(&swanctl_top_conf_path), &swanctl_top_conf);

    let updown_path = format!("/etc/strongswan.d/ims-updown-{idx}.sh");
    let updown_script = super::render::render_updown_script(&netns, &tun_iface);
    let _ = runner.write_file(Path::new(&updown_path), &updown_script);
    let _ = runner.run(&["chmod", "+x", &updown_path]);

    let epdg_template = runner
        .read_file(Path::new("/etc/strongswan.d/swanctl-epdg.conf.template"))
        .unwrap_or_default();
    let src_addr = config.vowifi.src_addr.clone();
    let params = super::render::SwanctlEpdgParams {
        imsi: &imsi,
        mcc,
        mnc,
        epdg_ip: &epdg_ip,
        if_id: &if_id,
        updown_script: &updown_path,
        src_addr: src_addr.as_deref(),
    };
    let swanctl_epdg = super::render::render_swanctl_epdg(&epdg_template, &params);
    let _ = runner.write_file(
        Path::new(&format!("{swanctl_conf_dir}/epdg.conf")),
        &swanctl_epdg,
    );

    // vowifi-usim-bridge, supervised on its own thread (so a crash/restart
    // never blocks this line's establish/steady-state loops below). Not
    // needed for a pcsc_reader line: pcscd already reaches the physical
    // reader directly via the ccid driver, with no modem/AT+CSIM bridge in
    // the path at all (specs/023-omnikey-pcsc-vowifi).
    let usim_holder: Arc<Mutex<Option<ChildHandle>>> = Arc::new(Mutex::new(None));
    if !line.pcsc_reader {
        let runner = Arc::clone(runner);
        let bin = bin.to_string();
        let config_path = config_path.to_string();
        let modem = modem.clone();
        let vpcd_host = config.vowifi.vpcd_host.clone();
        let vpcd_port = line.vpcd_port;
        let usim_holder = Arc::clone(&usim_holder);
        let started = Arc::clone(started);
        let shutting_down = Arc::clone(shutting_down);
        std::thread::spawn(move || loop {
            let guard = shutting_down.read().unwrap();
            if *guard {
                return;
            }
            match runner.spawn(ChildSpec::new([
                bin.as_str(),
                "--config",
                config_path.as_str(),
                "vowifi-usim-bridge",
                "--modem",
                modem.as_str(),
                "--vpcd-host",
                vpcd_host.as_str(),
                "--vpcd-port",
                &vpcd_port.to_string(),
            ])) {
                Ok(h) => {
                    *usim_holder.lock().unwrap() = Some(h);
                    started.lock().unwrap().vowifi_child_handles.push(h);
                    drop(guard);
                    // Poll is_alive() rather than blocking on wait(): a real
                    // review finding caught that RealCommandRunner::wait()
                    // removes the handle from the tracked table BEFORE
                    // blocking (by design, to close a PID-reuse race — see
                    // its own doc comment), which made this handle
                    // permanently un-signalable for the holder's entire
                    // lifetime. sim_recovery::reset_modem_sim needs to send
                    // it SIGSTOP/SIGCONT via this exact handle (read out of
                    // `usim_holder` from a different thread) while it's
                    // still running, so a blocking wait() here silently
                    // defeated that synchronization — the mocked test never
                    // caught it because MockCommandRunner::wait() doesn't
                    // remove the entry the way the real one does.
                    while runner.is_alive(h) {
                        runner.sleep(Duration::from_secs(1));
                    }
                    println!("[supervise] line {idx}: vowifi-usim-bridge exited; restarting in 5s");
                }
                Err(e) => {
                    drop(guard);
                    eprintln!("[supervise] line {idx}: failed to spawn vowifi-usim-bridge: {e}")
                }
            }
            runner.sleep(Duration::from_secs(5));
        });
    }

    let engine = StrongswanEngine {
        idx,
        strongswan_conf: strongswan_conf_path,
        swanctl_top_conf: swanctl_top_conf_path,
        charon_log: charon_log.clone(),
        netns: netns.clone(),
        tun_iface: tun_iface.clone(),
        if_id: if_id.clone(),
        charon_handle: RefCell::new(None),
    };
    let _ = runner.write_file(&charon_log, "");
    let _ = runner.run(&["rm", "-f", "/var/run/charon.pid"]);
    {
        // Greptile P1 (round 3, same design gap): the RwLock guard added for
        // recovery/steady-state covered restarts, but this line's very
        // *first* charon spawn — reached via its own top-level background
        // thread during initial startup — had no guard at all. If shutdown
        // begins while a line is still starting up for the first time, this
        // spawn could register its handle into StartedState after
        // shutdown's snapshot was already taken, exactly like the recovery
        // case, just at a different call site.
        let guard = shutting_down.read().unwrap();
        if *guard {
            println!("[supervise] line {idx}: shutting down before startup finished; abandoning");
            return;
        }
        match runner.spawn(
            ChildSpec::new([
                "env",
                &format!("STRONGSWAN_CONF={}", engine.strongswan_conf),
                "/usr/libexec/ipsec/charon",
            ])
            .capture_stdout_to(charon_log.clone()),
        ) {
            Ok(h) => {
                *engine.charon_handle.borrow_mut() = Some(h);
                started.lock().unwrap().vowifi_child_handles.push(h);
            }
            Err(e) => {
                eprintln!("[supervise] line {idx}: FATAL: failed to spawn charon: {e}");
                return;
            }
        }
    }

    runner.sleep(Duration::from_secs(2));
    let env = format!("STRONGSWAN_CONF={}", engine.strongswan_conf);
    let _ = runner.run(&[
        "env",
        &env,
        "swanctl",
        "--load-all",
        "--file",
        &engine.swanctl_top_conf,
    ]);
    let _ = runner.spawn_detached(ChildSpec::new([
        "env",
        &env,
        "swanctl",
        "--initiate",
        "--child",
        "ims",
    ]));

    println!("[supervise] line {idx}: waiting for the strongSwan tunnel (CHILD_SA + P-CSCF assignment) ...");
    let mut attempt = 0u32;
    let mut stuck = false;
    let pcscf = loop {
        match line_supervisor::tick_establishing(&engine, runner.as_ref(), &mut attempt, &mut stuck)
        {
            line_supervisor::EstablishOutcome::Established { pcscf } => break Some(pcscf),
            line_supervisor::EstablishOutcome::FatalProcessDied => {
                eprintln!("[supervise] line {idx}: FATAL: charon exited before establishing the tunnel; skipping this line");
                break None;
            }
            line_supervisor::EstablishOutcome::FatalTimedOut => break None, // unreachable for strongswan
            line_supervisor::EstablishOutcome::StillEstablishing => {
                runner.sleep(line_supervisor::ESTABLISH_POLL_INTERVAL);
            }
        }
    };
    let Some(pcscf) = pcscf else { return };

    println!("[supervise] line {idx}: tunnel UP. P-CSCF: {pcscf}");
    let _ = runner.write_file(Path::new(&line.pcscf_source_path), &pcscf);

    start_line_tail(
        runner,
        bin,
        config_path,
        idx,
        &netns,
        line,
        &usim_holder,
        started,
        pcscf.clone(),
        shutting_down,
        alert_ctx,
    );

    // Steady-state supervision loop — runs for the container's lifetime.
    let mut current_pcscf = pcscf;
    loop {
        runner.sleep(line_supervisor::STEADY_STATE_POLL_INTERVAL);
        // Held across the whole tick_steady_state call, not just a one-off
        // check before it: Greptile P1 (round 2) caught that a plain
        // check-then-call still lets a Recovered outcome's respawn
        // (engine.restart_process(), e.g. on ProcessDied/ViciBroken —
        // including when THIS line's charon was the one shutdown's own
        // KillChild step just signaled) register its new handle *after*
        // shutdown has already taken its StartedState snapshot. That
        // choreography (signal, wait, a deliberate 2s sleep, spawn,
        // load-all, initiate) takes multiple real seconds, so the escape
        // window was seconds wide, not the negligible one a bare flag check
        // would suggest. Holding the read guard through the handle
        // registration below closes it: shutdown's write-lock can't proceed
        // to snapshot StartedState until this whole section releases it.
        let guard = shutting_down.read().unwrap();
        if *guard {
            return;
        }
        match line_supervisor::tick_steady_state(&engine, runner.as_ref(), &current_pcscf) {
            line_supervisor::SteadyOutcome::StillUp => {
                drop(guard);
            }
            line_supervisor::SteadyOutcome::PcscfChanged { new_pcscf } => {
                drop(guard);
                println!("[supervise] line {idx}: P-CSCF changed ({current_pcscf} -> {new_pcscf}); refreshing");
                let _ = runner.write_file(Path::new(&line.pcscf_source_path), &new_pcscf);
                current_pcscf = new_pcscf;
                let _ = runner.run(&["pkill", "-f", &format!("vowifi-ims-agent --line {idx}$")]);
            }
            line_supervisor::SteadyOutcome::Recovered { reason } => {
                if let Some(new_handle) = *engine.charon_handle.borrow() {
                    started
                        .lock()
                        .unwrap()
                        .vowifi_child_handles
                        .push(new_handle);
                }
                drop(guard);
                if line_supervisor::recovery_restarts_agent(reason) {
                    let _ =
                        runner.run(&["pkill", "-f", &format!("vowifi-ims-agent --line {idx}$")]);
                }
            }
        }
    }
}

/// Veth pair + this line's vowifi-ims-agent (with per-incident USIM
/// auto-recovery) + the idle-tunnel keepalive — 1:1 port of
/// `start_line_tail` and its keepalive sibling. Both supervision loops run
/// on their own background threads and this function returns immediately,
/// matching the bash original backgrounding both with `&`.
#[allow(clippy::too_many_arguments)]
fn start_line_tail(
    runner: &Arc<dyn CommandRunner>,
    bin: &str,
    config_path: &str,
    idx: u32,
    netns: &str,
    line: &LineResolutionEntry,
    usim_holder: &Arc<Mutex<Option<ChildHandle>>>,
    started: &Arc<Mutex<StartedState>>,
    initial_pcscf: String,
    shutting_down: &Arc<RwLock<bool>>,
    alert_ctx: Option<&AlertContext>,
) {
    let veth_sip = line.config.veth_sip_iface.clone();
    let veth_ims = line.config.veth_ims_iface.clone();
    let veth_sip_addr = format!("{}/30", line.veth_peer_addr);
    let veth_ims_addr = format!("{}/30", line.veth_local_addr);

    println!("[supervise] line {idx}: creating veth pair ({veth_sip} <-> {veth_ims} in netns {netns})...");
    if !runner
        .run_in_netns(netns, &["ip", "link", "show", &veth_ims])
        .map(|o| o.status.success())
        .unwrap_or(false)
        && runner
            .run(&["ip", "link", "show", &veth_sip])
            .map(|o| o.status.success())
            .unwrap_or(false)
    {
        let _ = runner.run(&["ip", "link", "delete", &veth_sip]);
    }
    if !runner
        .run(&["ip", "link", "show", &veth_sip])
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        let _ = runner.run(&[
            "ip", "link", "add", &veth_sip, "type", "veth", "peer", "name", &veth_ims, "netns",
            netns,
        ]);
    }
    let _ = runner.run(&["ip", "addr", "replace", &veth_sip_addr, "dev", &veth_sip]);
    let _ = runner.run(&["ip", "link", "set", &veth_sip, "up"]);
    let _ = runner.run_in_netns(
        netns,
        &["ip", "addr", "replace", &veth_ims_addr, "dev", &veth_ims],
    );
    let _ = runner.run_in_netns(netns, &["ip", "link", "set", &veth_ims, "up"]);

    // vowifi-ims-agent, supervised, with per-incident CSIM-failure counting.
    {
        let runner = Arc::clone(runner);
        let bin = bin.to_string();
        let config_path = config_path.to_string();
        let netns = netns.to_string();
        let modem = line.modem_port.clone();
        let usim_holder = Arc::clone(usim_holder);
        let started = Arc::clone(started);
        let shutting_down = Arc::clone(shutting_down);
        let alert_ctx = alert_ctx.cloned();
        let line_id = line.card_id.clone();
        std::thread::spawn(move || {
            let mut csim_fails = sim_recovery::IncidentCounters::default();
            // specs/022-discord-critical-alerts FR-004 (research.md R2):
            // `Action::GiveUpForThisIncident` already existed but was never
            // acted on here — the loop just kept retrying forever with no
            // observable signal. Tracked separately from `csim_fails`
            // itself, which resets every incident (including a give-up), so
            // this flag alone decides whether a Recovered notice is owed.
            let mut given_up_alerted = false;
            let agent_log = PathBuf::from(format!("/tmp/ims-agent-{idx}.out"));
            loop {
                let guard = shutting_down.read().unwrap();
                if *guard {
                    return;
                }
                let _ = runner.write_file(&agent_log, "");
                let handle = runner.spawn(
                    ChildSpec::new([
                        "ip",
                        "netns",
                        "exec",
                        &netns,
                        &bin,
                        "--config",
                        &config_path,
                        "vowifi-ims-agent",
                        "--line",
                        &idx.to_string(),
                    ])
                    .capture_stdout_to(agent_log.clone()),
                );
                let Ok(handle) = handle else {
                    drop(guard);
                    eprintln!("[supervise] line {idx}: failed to spawn vowifi-ims-agent");
                    runner.sleep(Duration::from_secs(5));
                    continue;
                };
                started.lock().unwrap().vowifi_child_handles.push(handle);
                drop(guard);
                // See the daemon_supervisor loop earlier in this file: poll
                // is_alive(), don't block on wait(), so this handle
                // (signaled by execute_shutdown_plan via
                // `vowifi_child_handles`) stays signalable for as long as
                // the process is actually alive.
                while runner.is_alive(handle) {
                    runner.sleep(Duration::from_secs(1));
                }
                println!("[supervise] line {idx}: vowifi-ims-agent exited; restarting in 5s");

                let log_content = runner.read_file(&agent_log).unwrap_or_default();
                let outcome = if sim_recovery::has_csim_failure(&log_content) {
                    sim_recovery::AgentExitOutcome::CsimFailure
                } else {
                    sim_recovery::AgentExitOutcome::Other
                };
                let action = csim_fails.observe(outcome);
                if action == sim_recovery::Action::ResetSim {
                    let holder = *usim_holder.lock().unwrap();
                    let reset_log = PathBuf::from(format!("/tmp/sim-reset-{idx}.log"));
                    sim_recovery::reset_modem_sim(
                        runner.as_ref(),
                        Path::new(&modem),
                        &reset_log,
                        holder,
                    );
                }
                if action == sim_recovery::Action::GiveUpForThisIncident {
                    tracing::error!(
                        line = idx,
                        "SIM recovery exhausted (MAX_SIM_RESETS reached this incident); \
                         giving up on this reset cycle, will keep retrying the agent"
                    );
                }
                if let Some(kind) = sim_alert_transition(action, &mut given_up_alerted) {
                    if let Some(ctx) = &alert_ctx {
                        let description = match kind {
                            alerts::CriticalEventKind::Failure => {
                                "VoWiFi line's SIM recovery exhausted (max resets reached this incident)"
                            }
                            alerts::CriticalEventKind::Recovered => "VoWiFi line's SIM recovered",
                        };
                        ctx.fire(alerts::CriticalEvent {
                            category: alerts::AlertCategory::ModuleLifecycle,
                            unit_id: Some(line_id.clone()),
                            description: description.to_string(),
                            at: chrono::Utc::now(),
                            kind,
                        });
                    }
                }

                runner.sleep(Duration::from_secs(5));
            }
        });
    }

    // Idle-tunnel keepalive (TCP connect, not ICMP — operators filter ICMP
    // over the tunnel). Re-reads the P-CSCF source file every cycle so it
    // keeps pinging the right address after the steady-state loop refreshes
    // it.
    {
        let runner = Arc::clone(runner);
        let netns = netns.to_string();
        let pcscf_path = line.pcscf_source_path.clone();
        let interval = Duration::from_secs(
            /* [vowifi].keepalive_interval_sec, read by the caller and
            threaded through would be cleaner, but this loop only needs a
            reasonable default matching entrypoint.sh's own $KEEPALIVE_INTERVAL
            fallback */
            30,
        );
        std::thread::spawn(move || {
            let _ = initial_pcscf; // first cycle re-reads the file anyway
            loop {
                let pcscf_now = runner.read_file(Path::new(&pcscf_path)).unwrap_or_default();
                let pcscf_now = pcscf_now.trim();
                if !pcscf_now.is_empty() {
                    let _ = runner.run_in_netns(
                        &netns,
                        &[
                            "bash",
                            "-c",
                            &format!("timeout 3 bash -c '>/dev/tcp/{pcscf_now}/5060'"),
                        ],
                    );
                }
                runner.sleep(interval);
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn start_vowifi_line_swu(
    runner: &Arc<dyn CommandRunner>,
    bin: &str,
    config_path: &str,
    config: &AppConfig,
    line: &LineResolutionEntry,
    mcc: &str,
    mnc: &str,
    started: &Arc<Mutex<StartedState>>,
    shutting_down: &Arc<RwLock<bool>>,
    alert_ctx: Option<&AlertContext>,
) {
    let idx = line.index;
    let modem = line.modem_port.clone();
    let netns = line.netns.clone();

    if !std::path::Path::new("/dev/net/tun").exists() {
        eprintln!("[supervise] line {idx}: FATAL: /dev/net/tun missing; skipping this line");
        return;
    }

    let log_file = PathBuf::from(format!("/tmp/swu-{idx}.log"));
    let _ = runner.write_file(&log_file, "");

    let engine = SwuEngine {
        idx,
        modem: modem.clone(),
        apn: config.vowifi.apn.clone(),
        mcc: mcc.to_string(),
        mnc: mnc.to_string(),
        netns: netns.clone(),
        src_addr: config.vowifi.src_addr.clone(),
        log_file: log_file.clone(),
        dialer_handle: RefCell::new(None),
    };
    // Same guard as strongswan's initial charon spawn above (Greptile P1,
    // round 3): this line's very first dialer spawn had no guard at all.
    let guard = shutting_down.read().unwrap();
    if *guard {
        println!("[supervise] line {idx}: shutting down before startup finished; abandoning");
        return;
    }
    engine.restart_process(runner.as_ref()); // first spawn, same path as later respawns
    if let Some(h) = *engine.dialer_handle.borrow() {
        started.lock().unwrap().vowifi_child_handles.push(h);
        drop(guard);
    } else {
        drop(guard);
        eprintln!("[supervise] line {idx}: FATAL: failed to spawn swu dialer");
        return;
    }

    println!(
        "[supervise] line {idx}: waiting for tunnel (P-CSCF assignment + netns/tun setup) ..."
    );
    let mut attempt = 0u32;
    let mut stuck = false;
    let pcscf = loop {
        match line_supervisor::tick_establishing(&engine, runner.as_ref(), &mut attempt, &mut stuck)
        {
            line_supervisor::EstablishOutcome::Established { pcscf } => break Some(pcscf),
            line_supervisor::EstablishOutcome::FatalProcessDied => {
                eprintln!("[supervise] line {idx}: FATAL: dialer exited before establishing the tunnel; skipping this line");
                break None;
            }
            line_supervisor::EstablishOutcome::FatalTimedOut => {
                eprintln!("[supervise] line {idx}: FATAL: tunnel did not reach STATE CONNECTED within 180s; skipping this line");
                break None;
            }
            line_supervisor::EstablishOutcome::StillEstablishing => {
                runner.sleep(line_supervisor::ESTABLISH_POLL_INTERVAL);
            }
        }
    };
    let Some(pcscf) = pcscf else { return };

    println!("[supervise] line {idx}: tunnel UP. P-CSCF: {pcscf}");
    let _ = runner.write_file(Path::new(&line.pcscf_source_path), &pcscf);
    started.lock().unwrap().started_netns.push(netns.clone());

    let usim_holder: Arc<Mutex<Option<ChildHandle>>> = Arc::new(Mutex::new(None));
    start_line_tail(
        runner,
        bin,
        config_path,
        idx,
        &netns,
        line,
        &usim_holder,
        started,
        pcscf.clone(),
        shutting_down,
        alert_ctx,
    );

    let mut current_pcscf = pcscf;
    loop {
        runner.sleep(Duration::from_secs(5));
        // See the strongswan steady-state loop's identical pattern: the
        // guard is held across the whole tick_steady_state call, not just a
        // one-off check before it, because the Recovered branch below can
        // itself respawn the swu dialer via tick_steady_state's own internal
        // engine.restart_process() call — the escape window this closes is
        // the same one Greptile found on the strongswan side.
        let guard = shutting_down.read().unwrap();
        if *guard {
            return;
        }
        match line_supervisor::tick_steady_state(&engine, runner.as_ref(), &current_pcscf) {
            line_supervisor::SteadyOutcome::StillUp => {
                drop(guard);
            }
            line_supervisor::SteadyOutcome::PcscfChanged { new_pcscf } => {
                drop(guard);
                current_pcscf = new_pcscf;
            }
            line_supervisor::SteadyOutcome::Recovered { .. } => {
                if let Some(h) = *engine.dialer_handle.borrow() {
                    started.lock().unwrap().vowifi_child_handles.push(h);
                }
                drop(guard);
                let _ = runner.run(&["pkill", "-f", &format!("vowifi-ims-agent --line {idx}$")]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sim_alert_transition_fires_failure_once_on_give_up() {
        let mut alerted = false;
        assert_eq!(
            sim_alert_transition(sim_recovery::Action::GiveUpForThisIncident, &mut alerted),
            Some(alerts::CriticalEventKind::Failure)
        );
        assert!(alerted);
    }

    #[test]
    fn sim_alert_transition_does_not_repeat_failure_while_still_given_up() {
        let mut alerted = true;
        assert_eq!(
            sim_alert_transition(sim_recovery::Action::GiveUpForThisIncident, &mut alerted),
            None,
            "must not re-alert every incident while permanently stuck (FR-013)"
        );
        assert!(alerted);
    }

    #[test]
    fn sim_alert_transition_fires_recovered_once_after_give_up() {
        let mut alerted = true;
        assert_eq!(
            sim_alert_transition(sim_recovery::Action::None, &mut alerted),
            Some(alerts::CriticalEventKind::Recovered)
        );
        assert!(!alerted);
    }

    #[test]
    fn sim_alert_transition_none_when_never_alerted() {
        let mut alerted = false;
        assert_eq!(
            sim_alert_transition(sim_recovery::Action::None, &mut alerted),
            None
        );
        assert_eq!(
            sim_alert_transition(sim_recovery::Action::ResetSim, &mut alerted),
            None,
            "a plain reset with no prior give-up is not itself a recovery notice"
        );
    }

    use crate::config::VowifiConfig;
    use crate::supervise::runner::MockCommandRunner;

    fn test_config() -> AppConfig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[sip]\nserver = \"sip.example.com\"\nusername = \"user\"\npassword = \"pass\"\n",
        )
        .unwrap();
        let mut config = crate::config::load_config(&path).unwrap();
        // Bypass live DNS resolution in every test below — irrelevant to what
        // each test is actually checking.
        config.vowifi.epdg_ip = Some("192.0.2.1".to_string());
        config
    }

    fn pcsc_line(index: u32) -> LineResolutionEntry {
        let mut config = VowifiConfig::default();
        config.imsi_override = Some("404940123456789".to_string());
        LineResolutionEntry {
            index,
            card_id: format!("pcsc{index}"),
            modem_port: String::new(),
            netns: format!("ims{index}"),
            control_port: 7050,
            veth_local_addr: "10.99.0.1".to_string(),
            veth_peer_addr: "10.99.0.2".to_string(),
            vpcd_port: 15963,
            strongswan_if_id: 23,
            strongswan_tun_iface: "tun23-0".to_string(),
            pcscf_source_path: "/tmp/pcscf-test".to_string(),
            mcc: "404".to_string(),
            mnc: "043".to_string(),
            pcsc_reader: true,
            config,
        }
    }

    fn modem_line(index: u32, modem_port: &str) -> LineResolutionEntry {
        LineResolutionEntry {
            index,
            card_id: format!("ec20-{index}"),
            modem_port: modem_port.to_string(),
            netns: format!("ims{index}"),
            control_port: 7050,
            veth_local_addr: "10.99.0.1".to_string(),
            veth_peer_addr: "10.99.0.2".to_string(),
            vpcd_port: 15963,
            strongswan_if_id: 23,
            strongswan_tun_iface: "tun23-0".to_string(),
            pcscf_source_path: "/tmp/pcscf-test".to_string(),
            mcc: "404".to_string(),
            mnc: "094".to_string(),
            pcsc_reader: false,
            config: VowifiConfig::default(),
        }
    }

    /// Polls `cond` until it's true or `timeout` elapses — needed only for
    /// effects produced by a background thread this module intentionally
    /// never joins (e.g. the vowifi-usim-bridge supervision loop), never for
    /// timing-sensitive production logic itself.
    fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let start = std::time::Instant::now();
        loop {
            if cond() {
                return true;
            }
            if start.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn spawned_argv_containing(mock: &MockCommandRunner, needle: &str) -> bool {
        mock.spawn_specs
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.argv.iter().any(|a| a.contains(needle)))
    }

    fn ran_argv_containing(mock: &MockCommandRunner, needle: &str) -> bool {
        mock.run_calls
            .lock()
            .unwrap()
            .iter()
            .any(|argv| argv.iter().any(|a| a.contains(needle)))
    }

    #[test]
    fn start_vowifi_line_pcsc_reader_skips_modem_checks() {
        // specs/023-omnikey-pcsc-vowifi US1 (T007): a pcsc_reader line never
        // runs the modem-path-existence check or `modem-ims --modem`.
        // charon is made to "die" immediately so tick_establishing's
        // otherwise-infinite loop returns FatalProcessDied on its first
        // check — the function under test then returns deterministically.
        let mock = Arc::new(MockCommandRunner::new());
        mock.set_born_dead_if_argv_contains("charon");
        let runner: Arc<dyn CommandRunner> = mock.clone();
        let config = test_config();
        let line = pcsc_line(0);
        let started = Arc::new(Mutex::new(StartedState::default()));
        let shutting_down = Arc::new(RwLock::new(false));

        start_vowifi_line(
            &runner,
            "gsm-sip-bridge",
            "/tmp/cfg.toml",
            &config,
            &line,
            &started,
            &shutting_down,
            None,
        );

        assert!(
            !ran_argv_containing(&mock, "modem-ims"),
            "modem-ims reconcile must never run for a pcsc_reader line"
        );
        // Reaching a charon spawn attempt proves execution got past the
        // (skipped) modem-existence check and modem-ims call rather than
        // bailing out before either — the only other way this spawn could
        // be reached.
        assert!(
            spawned_argv_containing(&mock, "charon"),
            "expected execution to reach the strongswan engine dispatch"
        );
    }

    #[test]
    fn start_vowifi_line_strongswan_skips_usim_bridge_only_for_pcsc() {
        // specs/023-omnikey-pcsc-vowifi US1 (T008): pcsc_reader never spawns
        // vowifi-usim-bridge; an equivalent modem-backed line still does
        // (regression).
        let mock = Arc::new(MockCommandRunner::new());
        mock.set_born_dead_if_argv_contains("charon");
        let runner: Arc<dyn CommandRunner> = mock.clone();
        let config = test_config();
        let line = pcsc_line(0);
        let started = Arc::new(Mutex::new(StartedState::default()));
        let shutting_down = Arc::new(RwLock::new(false));

        start_vowifi_line_strongswan(
            &runner,
            "gsm-sip-bridge",
            "/tmp/cfg.toml",
            &config,
            &line,
            &line.mcc.clone(),
            &line.mnc.clone(),
            &started,
            &shutting_down,
            None,
        );
        assert!(
            !spawned_argv_containing(&mock, "vowifi-usim-bridge"),
            "a pcsc_reader line must never spawn vowifi-usim-bridge"
        );

        // Regression: an otherwise-equivalent modem-backed line still spawns
        // it, on its own background thread — poll briefly for that thread to
        // record its spawn.
        let mock2 = Arc::new(MockCommandRunner::new());
        mock2.set_born_dead_if_argv_contains("charon");
        let runner2: Arc<dyn CommandRunner> = mock2.clone();
        let dir = tempfile::tempdir().unwrap();
        let fake_modem = dir.path().join("ttyFAKE");
        std::fs::write(&fake_modem, "").unwrap();
        let modem_port = fake_modem.to_string_lossy().to_string();
        {
            use std::os::unix::process::ExitStatusExt;
            mock2.set_run_output(
                &format!("gsm-sip-bridge vowifi-imsi --modem {modem_port}"),
                std::process::Output {
                    status: std::process::ExitStatus::from_raw(0),
                    stdout: b"404011111111111\n".to_vec(),
                    stderr: Vec::new(),
                },
            );
        }
        let modem = modem_line(0, &modem_port);
        let started2 = Arc::new(Mutex::new(StartedState::default()));
        let shutting_down2 = Arc::new(RwLock::new(false));
        start_vowifi_line_strongswan(
            &runner2,
            "gsm-sip-bridge",
            "/tmp/cfg.toml",
            &config,
            &modem,
            &modem.mcc.clone(),
            &modem.mnc.clone(),
            &started2,
            &shutting_down2,
            None,
        );
        assert!(
            wait_until(Duration::from_millis(500), || spawned_argv_containing(
                &mock2,
                "vowifi-usim-bridge"
            )),
            "a modem-backed line must still spawn vowifi-usim-bridge"
        );
    }

    #[test]
    fn pcsc_engine_compatibility_ok_under_strongswan() {
        let lines = vec![pcsc_line(0)];
        assert!(check_pcsc_engine_compatibility(&lines, "strongswan").is_ok());
    }

    #[test]
    fn pcsc_engine_compatibility_ok_under_swu_without_pcsc_lines() {
        let lines = vec![modem_line(0, "/dev/ttyUSB0")];
        assert!(check_pcsc_engine_compatibility(&lines, "swu").is_ok());
    }

    #[test]
    fn pcsc_engine_compatibility_rejects_pcsc_line_under_swu() {
        // specs/023-omnikey-pcsc-vowifi US3 (T022): fails, naming the
        // offending line, before any per-line process would be spawned —
        // `run()` itself calls this check ahead of the per-line spawn loop
        // and hardcodes `RealCommandRunner`, so this is the injectable seam
        // that actually captures the decision logic under test.
        let lines = vec![modem_line(0, "/dev/ttyUSB0"), pcsc_line(1)];
        let err = check_pcsc_engine_compatibility(&lines, "swu").unwrap_err();
        assert!(err.contains('1'), "unexpected error: {err}");
        assert!(err.contains("pcsc1"), "unexpected error: {err}");
    }

    #[test]
    fn an_all_card_reader_deployment_needs_no_vpcd_reader() {
        // Caught live on 2026-07-28: a pcsc-only deployment died at startup
        // with "pcscd's vpcd reader never came up" because vpcd was
        // provisioned unconditionally under the strongswan engine, even
        // though no line would ever connect to it.
        assert!(!needs_vpcd_reader(&[pcsc_line(0)]));
        assert!(!needs_vpcd_reader(&[pcsc_line(0), pcsc_line(1)]));
    }

    #[test]
    fn any_modem_backed_line_still_needs_the_vpcd_reader() {
        assert!(needs_vpcd_reader(&[modem_line(0, "/dev/ttyUSB0")]));
        // Mixed deployment: the modem line's SIM still reaches charon only
        // through vpcd, so one modem line is enough to require it.
        assert!(needs_vpcd_reader(&[
            modem_line(0, "/dev/ttyUSB0"),
            pcsc_line(1)
        ]));
    }
}
