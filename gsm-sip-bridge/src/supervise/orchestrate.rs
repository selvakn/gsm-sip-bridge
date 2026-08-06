//! Top-level container orchestration (specs/021-entrypoint-supervise-rust
//! Phase 4) — what `docker/entrypoint.sh` used to do itself in bash. 1:1 port
//! of its startup sequencing (discover once up front, mutual-exclusion gate,
//! circuit-switched daemon, VoWiFi per-line loop, clean shutdown), now
//! calling this binary's own already-tested Rust modules in-process instead
//! of shelling out to bash functions.
//!
//! Threading note: functions that need to background their own supervision
//! loop (matching the original script's `(...) &`) take `&Arc<dyn
//! CommandRunner>` so they can `Arc::clone` it into a genuinely `'static`
//! `std::thread::spawn` closure; functions that only ever run synchronously
//! within an already-running supervisor thread (the tested Phase 1-3
//! modules: `line_supervisor`, `sim_recovery`, `epdg_iface`, `vpcd`, `render`)
//! keep taking a plain `&dyn CommandRunner`, unchanged.

use super::engines::{SharedCharon, StrongswanEngine, SwuEngine};
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

/// Assets of the **one** charon daemon shared by every strongswan-engine line.
///
/// These were per line (`/etc/strongswan-line-N.conf`, `/tmp/charon-N.log`,
/// `/var/run/charon-N.vici`, `/etc/swanctl/conf.d-N/`) back when each line ran
/// its own daemon — an arrangement that silently broke every line but one,
/// because N charons in one netns all wildcard-bind UDP 500/4500 and only one
/// of them receives. See [`SharedCharon`] for the full account.
const SHARED_STRONGSWAN_CONF: &str = "/etc/strongswan-shared.conf";
const SHARED_SWANCTL_CONF: &str = "/etc/swanctl/swanctl.conf";
const SHARED_SWANCTL_CONF_DIR: &str = "/etc/swanctl/conf.d";
const SHARED_CHARON_LOG: &str = "/tmp/charon.log";
const SHARED_VICI_SOCKET: &str = "/var/run/charon.vici";
/// Overwritten at startup from the resolved line set — the osmocom P-CSCF
/// plugin enables its attribute request per *connection name*, so the image's
/// static copy cannot know them. See `render::render_pcscf_plugin_conf`.
const PCSCF_PLUGIN_CONF: &str = "/etc/strongswan.d/charon/p-cscf.conf";

/// specs/027-discover-retry-health: how long `spawn_discover_retry` keeps
/// re-checking a configured line that was missing on the first `discover`
/// pass before giving up on it for this run. Sized to absorb ordinary USB/
/// modem enumeration delay (the observed real-world case — an EC20 modem
/// that hadn't finished enumerating when the first pass ran), not to wait
/// out a genuinely disconnected device indefinitely.
const DISCOVER_RETRY_WINDOW: Duration = Duration::from_secs(180);
/// How often `spawn_discover_retry` re-runs `discover` while a configured
/// line is still missing.
const DISCOVER_RETRY_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// This line's swanctl connection and child name.
///
/// Unique per line because every line's connection lives in one shared charon:
/// a shared name would make `--initiate --child` fire every line and
/// `--terminate --ike` tear every line down, and would make the shared charon
/// log's `<name|N>` prefix impossible to attribute back to a line.
///
/// Whatever this returns must also be what `render_pcscf_plugin_conf` enables,
/// or the line silently never gets a P-CSCF.
fn vowifi_conn_name(idx: u32) -> String {
    format!("ims{idx}")
}

/// The context every per-line startup step needs, threaded as one value.
///
/// These were previously spelled out on each of the four
/// `start_vowifi_line*` / `start_line_tail` signatures, all of which carried
/// `#[allow(clippy::too_many_arguments)]` to say so. Bundling them separates
/// what is *ambient* (the runner, the binary and config paths, the shared
/// started-state and shutdown gate, the alert sink, the shared charon) from
/// what actually varies per call (which line, which PLMN, which namespace) —
/// the latter is what a reader needs to see at a call site, and it was buried
/// among the former.
struct LineStartup<'a> {
    runner: &'a Arc<dyn CommandRunner>,
    bin: &'a str,
    config_path: &'a str,
    config: &'a AppConfig,
    started: &'a Arc<Mutex<StartedState>>,
    shutting_down: &'a Arc<RwLock<bool>>,
    alert_ctx: Option<&'a AlertContext>,
    /// The charon daemon every strongswan-engine line shares.
    shared_charon: &'a Arc<SharedCharon>,
}

/// Starts the whole inbound VoWiFi-to-SIP bridge for a non-empty, already
/// fully-resolved line set: pcscd/vpcd, XFRM reclaim, the shared charon's
/// rendered assets (including `PCSCF_PLUGIN_CONF`, which must list every
/// line's connection name *before* charon starts — see that const's doc
/// comment), every line's own supervision thread, and the shared Agent B
/// (`vowifi-sip-agent`) process.
///
/// Extracted out of `run`'s section 3 (specs/027-discover-retry-health) so
/// it can be called a second time, later, from `spawn_discover_retry`'s
/// success path — but only when nothing has started it yet this run (see
/// that function's doc comment for why this is never called twice).
///
/// Returns `Err` for the same fatal conditions section 3 used to `return
/// ExitCode::FAILURE` for directly; the synchronous startup caller maps that
/// straight back to a process exit, while the retry caller only has a
/// background thread to log from and gives up on starting the subsystem
/// this run instead.
#[allow(clippy::too_many_arguments)]
fn start_vowifi_subsystem(
    vowifi_lines: &[LineResolutionEntry],
    runner: &Arc<dyn CommandRunner>,
    bin: &str,
    config_path_str: &str,
    config: &AppConfig,
    started: &Arc<Mutex<StartedState>>,
    shutting_down: &Arc<RwLock<bool>>,
    alert_ctx: Option<&AlertContext>,
) -> Result<(), String> {
    println!(
        "[supervise] [vowifi].enabled — starting {} VoWiFi line(s) (engine: {})",
        vowifi_lines.len(),
        config.vowifi.tunnel_engine
    );

    check_pcsc_engine_compatibility(vowifi_lines, &config.vowifi.tunnel_engine)?;

    if runner
        .run(&["ip", "netns", "add", "__probe"])
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        return Err(
            "cannot create network namespaces — add cap_add: SYS_ADMIN (and NET_ADMIN)".to_string(),
        );
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
        let needs_vpcd = needs_vpcd_reader(vowifi_lines);
        if needs_vpcd {
            vpcd::write_vpcd_reader_conf(runner.as_ref(), config.vowifi.vpcd_port);
        }
        let Some(pcscd_handle) = vpcd::spawn_pcscd(runner.as_ref()) else {
            return Err("failed to start pcscd".to_string());
        };
        let pcscd_handle = Arc::new(pcscd_handle);
        started.lock().unwrap().pcscd = Some(pcscd_handle.clone());

        if needs_vpcd {
            println!(
                "[supervise] started shared pcscd; one vpcd reader, slots from {}",
                config.vowifi.vpcd_port
            );
            match vpcd::wait_for_vpcd_ready(
                runner.as_ref(),
                &pcscd_handle,
                &config.vowifi.vpcd_host,
                config.vowifi.vpcd_port,
            ) {
                vpcd::ReadyOutcome::Ready => println!(
                    "[supervise] vpcd reader ready on {}:{}",
                    config.vowifi.vpcd_host, config.vowifi.vpcd_port
                ),
                other => {
                    return Err(format!(
                        "pcscd's vpcd reader never came up on {}:{} ({other:?}). \
                         If pcscd logged 'Address in use', another process holds that port — pick a \
                         [vowifi].vpcd_port below the ephemeral range.",
                        config.vowifi.vpcd_host, config.vowifi.vpcd_port
                    ));
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

    // Before anything creates an interface or starts charon: clear
    // XFRM state left by a previous run of this container. It outlives
    // the container and keeps these lines' if_ids claimed, which makes
    // their tunnel interfaces impossible to create — the line then
    // re-establishes its tunnel every steady-state tick, forever.
    // Guarded so a host running unrelated IPsec is never touched.
    let our_if_ids: std::collections::BTreeSet<u32> =
        vowifi_lines.iter().map(|l| l.strongswan_if_id).collect();
    epdg_iface::reclaim_stale_xfrm(runner.as_ref(), &our_if_ids);

    // Render the shared charon's assets once, before any line starts.
    // Every line then drops its own connection file into the shared
    // conf.d and loads the union.
    let shared_charon = Arc::new(SharedCharon::new(
        SHARED_STRONGSWAN_CONF.to_string(),
        SHARED_SWANCTL_CONF.to_string(),
        PathBuf::from(SHARED_CHARON_LOG),
    ));
    let _ = runner.write_file(
        Path::new(SHARED_STRONGSWAN_CONF),
        &super::render::render_strongswan_conf(SHARED_VICI_SOCKET, SHARED_CHARON_LOG),
    );
    let _ = runner.run(&["mkdir", "-p", SHARED_SWANCTL_CONF_DIR]);
    // Drop any connection file left by a previous run of this
    // container: `docker restart` keeps the filesystem, and a stale
    // `epdg-N.conf` for a line that no longer exists would be loaded
    // right back in by the directory-wide `--load-all`.
    let _ = runner.run(&[
        "sh",
        "-c",
        &format!("rm -f {SHARED_SWANCTL_CONF_DIR}/*.conf"),
    ]);
    let _ = runner.write_file(
        Path::new(SHARED_SWANCTL_CONF),
        &super::render::render_swanctl_top_conf(SHARED_SWANCTL_CONF_DIR),
    );
    // Must be written before charon starts: the plugin reads this once
    // at load time, and it enables the P-CSCF request per connection
    // *name*, so it has to name every line the daemon will serve.
    let conn_names: Vec<String> = vowifi_lines
        .iter()
        .map(|l| vowifi_conn_name(l.index))
        .collect();
    let _ = runner.write_file(
        Path::new(PCSCF_PLUGIN_CONF),
        &super::render::render_pcscf_plugin_conf(&conn_names),
    );

    for line in vowifi_lines {
        let runner = Arc::clone(runner);
        let bin = bin.to_string();
        let cfg = config_path_str.to_string();
        let started = Arc::clone(started);
        let shutting_down = Arc::clone(shutting_down);
        let config = config.clone();
        let line = line.clone();
        let alert_ctx = alert_ctx.cloned();
        let shared_charon = Arc::clone(&shared_charon);
        std::thread::spawn(move || {
            start_vowifi_line(
                &LineStartup {
                    runner: &runner,
                    bin: &bin,
                    config_path: &cfg,
                    config: &config,
                    started: &started,
                    shutting_down: &shutting_down,
                    alert_ctx: alert_ctx.as_ref(),
                    shared_charon: &shared_charon,
                },
                &line,
            );
        });
    }

    // Agent B: one shared process for every line's veth pair.
    {
        let runner = Arc::clone(runner);
        let bin = bin.to_string();
        let cfg = config_path_str.to_string();
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
                cfg.as_str(),
                "vowifi-sip-agent",
            ])) {
                Ok(handle) => {
                    let handle = Arc::new(handle);
                    started.lock().unwrap().sip_agent_supervisor = Some(handle.clone());
                    drop(guard);
                    // See the daemon_supervisor loop above: poll
                    // is_alive(), don't block on wait(), so this
                    // handle (signaled by execute_shutdown_plan via
                    // `sip_agent_supervisor`) stays signalable for
                    // as long as the process is actually alive.
                    while runner.is_alive(&handle) {
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

    Ok(())
}

/// specs/027-discover-retry-health US1/US3: only ever called when the first
/// `discover` pass resolved zero VoWiFi lines (so nothing has started —
/// `start_vowifi_subsystem` was never called this run, per `run`'s only call
/// site for this function). Background thread that re-runs `discover` on
/// `DISCOVER_RETRY_POLL_INTERVAL` for as long as any configured override (a
/// `modem_port`/`modem_serial` line) is still unresolved:
///
/// - `DISCOVER_RETRY_WINDOW` governs only *when the one-time `Failure`
///   alert fires* for an override, not how long this function keeps
///   watching it — a spec edge case (spec.md) requires a line to still be
///   able to recover, with a paired `Recovered` alert, even after it was
///   already declared failed. Since re-scanning costs one `discover`
///   subprocess + file read per tick, watching indefinitely is cheap; only
///   *starting* the subsystem has a real safety window (see below).
/// - As soon as *any* override resolves, `start_vowifi_subsystem` runs once
///   with whatever the resolution now contains (including an override that
///   resolved only after its own alert already fired — that gets its
///   `Recovered` notice here) — exactly the normal startup path, just
///   delayed. From that point on this function stops entirely: hot-adding a
///   *further* late line to an already-started charon isn't safe (see
///   `start_vowifi_subsystem`'s doc comment and R3 in research.md), so any
///   override still unresolved at that instant is left exactly as it is —
///   covered by Foundational's immediate `not_found` detection and,  if its
///   own alert already fired, by that alert — but not retried further.
///
/// Pure decision core of one retry tick, kept separate from
/// `spawn_discover_retry`'s threading/file-IO so it's testable without real
/// sleeping: given which of `pending`'s identifiers are still missing after
/// a fresh `discover` pass, which have already had a `Failure` alert fired
/// (`alerted`), and the current time, returns `(newly_resolved,
/// newly_expired)`. An identifier that resolved this tick is reported as
/// resolved even if its deadline also happens to have passed — resolving
/// always wins over expiring. An identifier already in `alerted` is never
/// reported as newly-expired again (the alert already fired once).
fn discover_retry_tick(
    pending: &std::collections::HashMap<String, std::time::Instant>,
    alerted: &std::collections::HashSet<String>,
    still_missing: &std::collections::HashSet<String>,
    now: std::time::Instant,
) -> (Vec<String>, Vec<String>) {
    let resolved: Vec<String> = pending
        .keys()
        .filter(|id| !still_missing.contains(id.as_str()))
        .cloned()
        .collect();
    let expired: Vec<String> = pending
        .iter()
        .filter(|(id, deadline)| {
            !resolved.contains(id) && now >= **deadline && !alerted.contains(id.as_str())
        })
        .map(|(id, _)| id.clone())
        .collect();
    (resolved, expired)
}

fn spawn_discover_retry(
    runner: &Arc<dyn CommandRunner>,
    bin: &str,
    config_path_str: &str,
    config: &AppConfig,
    started: &Arc<Mutex<StartedState>>,
    shutting_down: &Arc<RwLock<bool>>,
    alert_ctx: Option<&AlertContext>,
) {
    let lines_file = crate::modules::discovery::lines_file_path();
    let initial_failed = crate::vowifi::discovery::read_line_resolution(&lines_file)
        .map(|r| r.failed)
        .unwrap_or_default();
    let deadline = std::time::Instant::now() + DISCOVER_RETRY_WINDOW;
    let mut pending: std::collections::HashMap<String, std::time::Instant> = initial_failed
        .into_iter()
        .filter(|f| f.reason == "not_found")
        .map(|f| (f.card_id, deadline))
        .collect();
    if pending.is_empty() {
        return;
    }

    let runner = Arc::clone(runner);
    let bin = bin.to_string();
    let config_path_str = config_path_str.to_string();
    let config = config.clone();
    let started = Arc::clone(started);
    let shutting_down = Arc::clone(shutting_down);
    let alert_ctx = alert_ctx.cloned();
    std::thread::spawn(move || {
        // Ids that already had a `Failure` alert dispatched for them — never
        // removed from `pending` on expiry (only on resolve), so a later
        // tick can still notice recovery and pair it with a `Recovered`
        // alert (`alerted.remove` below).
        let mut alerted: std::collections::HashSet<String> = std::collections::HashSet::new();
        while !pending.is_empty() {
            if *shutting_down.read().unwrap() {
                return;
            }
            runner.sleep(DISCOVER_RETRY_POLL_INTERVAL);
            if *shutting_down.read().unwrap() {
                return;
            }

            match runner.run(&[&bin, "--config", &config_path_str, "discover"]) {
                Ok(o) if o.status.success() => {}
                _ => {
                    eprintln!(
                        "[supervise] line discovery: retry re-run of 'discover' failed; trying again in {DISCOVER_RETRY_POLL_INTERVAL:?}"
                    );
                    continue;
                }
            }
            let Ok(resolution) = crate::vowifi::discovery::read_line_resolution(&lines_file) else {
                continue;
            };
            let still_missing: std::collections::HashSet<String> = resolution
                .failed
                .iter()
                .filter(|f| f.reason == "not_found")
                .map(|f| f.card_id.clone())
                .collect();
            let (resolved_now, newly_expired) = discover_retry_tick(
                &pending,
                &alerted,
                &still_missing,
                std::time::Instant::now(),
            );

            for id in &newly_expired {
                println!(
                    "[supervise] line discovery: configured line {id} still not found after {DISCOVER_RETRY_WINDOW:?} — alerting (still watching in case it recovers)"
                );
                alerted.insert(id.clone());
                if let Some(ctx) = &alert_ctx {
                    ctx.fire(alerts::CriticalEvent {
                        category: alerts::AlertCategory::LineDiscoveryFailed,
                        unit_id: Some(id.clone()),
                        description: format!(
                            "configured VoWiFi line {id} was not found after {DISCOVER_RETRY_WINDOW:?} of retrying discovery"
                        ),
                        at: chrono::Utc::now(),
                        kind: alerts::CriticalEventKind::Failure,
                    });
                }
            }

            if !resolved_now.is_empty() {
                println!(
                    "[supervise] line discovery: {} previously-missing configured line(s) now found ({}) — starting the VoWiFi subsystem",
                    resolved_now.len(),
                    resolved_now.join(", ")
                );
                if let Err(msg) = start_vowifi_subsystem(
                    &resolution.lines,
                    &runner,
                    &bin,
                    &config_path_str,
                    &config,
                    &started,
                    &shutting_down,
                    alert_ctx.as_ref(),
                ) {
                    eprintln!(
                        "[supervise] FATAL: retried VoWiFi line(s) found but failed to start: {msg}"
                    );
                }
                for id in &resolved_now {
                    pending.remove(id);
                    if alerted.remove(id) {
                        if let Some(ctx) = &alert_ctx {
                            ctx.fire(alerts::CriticalEvent {
                                category: alerts::AlertCategory::LineDiscoveryFailed,
                                unit_id: Some(id.clone()),
                                description: format!(
                                    "configured VoWiFi line {id} was found and started after previously being reported as not found"
                                ),
                                at: chrono::Utc::now(),
                                kind: alerts::CriticalEventKind::Recovered,
                            });
                        }
                    }
                }
                // Whatever's still in `pending` at this point can never be
                // hot-added (charon just started) — stop watching entirely.
                return;
            }
        }
    });
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
    // see this function's own comment below on why (both would
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
                    // Shared, not copied: this thread polls it for liveness
                    // while the shutdown plan holds the other claim so it can
                    // signal the same child from the shutdown thread.
                    let handle = Arc::new(handle);
                    started.lock().unwrap().daemon_supervisor = Some(handle.clone());
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
                    while runner.is_alive(&handle) {
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
            // specs/027-discover-retry-health: nothing has started yet this
            // run (no charon, no pcscd), so retrying the still-missing
            // configured overrides and starting the subsystem late if one
            // resolves is safe — see `spawn_discover_retry` and R3 in
            // research.md for why this is deliberately *not* attempted once
            // any line has already started.
            spawn_discover_retry(
                &runner,
                &bin,
                &config_path_str,
                &config,
                &started,
                &shutting_down,
                alert_ctx.as_ref(),
            );
        } else if let Err(msg) = start_vowifi_subsystem(
            &vowifi_lines,
            &runner,
            &bin,
            &config_path_str,
            &config,
            &started,
            &shutting_down,
            alert_ctx.as_ref(),
        ) {
            eprintln!("[supervise] FATAL: {msg}");
            return ExitCode::FAILURE;
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
///
/// A `pcsc_reader` line has no modem port to pass, but the two files the
/// derivation needs (EF_IMSI, EF_AD) are on the card either way, so it goes
/// over PC/SC instead — keyed by the line's IMSI, which is exactly how
/// `PcscTransport::connect` already picks that line's reader. That is why
/// `mcc`/`mnc` are optional on a card-reader line rather than mandatory.
fn resolve_mcc_mnc(
    runner: &dyn CommandRunner,
    bin: &str,
    line: &LineResolutionEntry,
) -> Option<(String, String)> {
    if !line.mcc.is_empty() && !line.mnc.is_empty() {
        return Some((line.mcc.clone(), line.mnc.clone()));
    }
    let out = if line.pcsc_reader {
        // Config validation guarantees a pcsc_reader line has an
        // imsi_override — it is how the line names its own physical card.
        let imsi = line.config.imsi_override.clone().unwrap_or_default();
        runner
            .run(&[bin, "vowifi-plmn", "--pcsc-imsi", &imsi])
            .ok()?
    } else {
        runner
            .run(&[bin, "vowifi-plmn", "--modem", &line.modem_port])
            .ok()?
    };
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
fn start_vowifi_line(ctx: &LineStartup, line: &LineResolutionEntry) {
    let runner = ctx.runner;
    let bin = ctx.bin;
    let config_path = ctx.config_path;
    let config = ctx.config;
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
        start_vowifi_line_strongswan(ctx, line, &mcc, &mnc);
    } else {
        start_vowifi_line_swu(ctx, line, &mcc, &mnc);
    }
}

fn start_vowifi_line_strongswan(
    ctx: &LineStartup,
    line: &LineResolutionEntry,
    mcc: &str,
    mnc: &str,
) {
    let runner = ctx.runner;
    let bin = ctx.bin;
    let config_path = ctx.config_path;
    let config = ctx.config;
    let started = ctx.started;
    let shutting_down = ctx.shutting_down;
    let idx = line.index;
    let modem = line.modem_port.clone();
    let netns = line.netns.clone();
    let tun_iface = line.strongswan_tun_iface.clone();
    let if_id = line.strongswan_if_id.to_string();
    let conn_name = vowifi_conn_name(idx);

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

    if !epdg_iface::ensure_epdg_interface(runner.as_ref(), &netns, &tun_iface, &if_id) {
        eprintln!(
            "[supervise] line {idx}: {tun_iface} is still absent from netns {netns} after \
             setup; this line's tunnel cannot carry traffic and its supervisor will keep \
             retrying (see the error above for why the interface could not be created)"
        );
    }
    started.lock().unwrap().started_netns.push(netns.clone());

    let Some(imsi) = resolve_imsi(runner.as_ref(), bin, line) else {
        eprintln!("[supervise] line {idx}: FATAL: failed to read IMSI; skipping this line");
        return;
    };

    // No per-line strongswan.conf / swanctl top conf any more: both belong to
    // the shared daemon and are rendered once in `run`. This line contributes
    // only its own connection file, below.
    let updown_path = format!("/etc/strongswan.d/ims-updown-{idx}.sh");
    let updown_script = super::render::render_updown_script(&netns, &tun_iface);
    let _ = runner.write_file(Path::new(&updown_path), &updown_script);
    let _ = runner.run(&["chmod", "+x", &updown_path]);

    let epdg_template = runner
        .read_file(Path::new("/etc/strongswan.d/swanctl-epdg.conf.template"))
        .unwrap_or_default();
    let src_addr = config.vowifi.src_addr.clone();
    let params = super::render::SwanctlEpdgParams {
        conn_name: &conn_name,
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
        Path::new(&format!("{SHARED_SWANCTL_CONF_DIR}/epdg-{idx}.conf")),
        &swanctl_epdg,
    );

    // vowifi-usim-bridge, supervised on its own thread (so a crash/restart
    // never blocks this line's establish/steady-state loops below). Not
    // needed for a pcsc_reader line: pcscd already reaches the physical
    // reader directly via the ccid driver, with no modem/AT+CSIM bridge in
    // the path at all (specs/023-omnikey-pcsc-vowifi).
    let usim_holder: Arc<Mutex<Option<Arc<ChildHandle>>>> = Arc::new(Mutex::new(None));
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
                    let h = Arc::new(h);
                    *usim_holder.lock().unwrap() = Some(h.clone());
                    started.lock().unwrap().vowifi_child_handles.push(h.clone());
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
                    while runner.is_alive(&h) {
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
        conn_name: conn_name.clone(),
        netns: netns.clone(),
        tun_iface: tun_iface.clone(),
        if_id: if_id.clone(),
        shared: Arc::clone(ctx.shared_charon),
    };
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
        // Idempotent across lines: whichever line reaches this first spawns
        // the daemon and is handed the handle to register for shutdown; every
        // other line finds it already running and gets `None` back, so the
        // daemon is registered exactly once rather than once per line.
        if let Some(h) = ctx.shared_charon.ensure_started(runner.as_ref()) {
            started.lock().unwrap().vowifi_child_handles.push(h);
        }
    }

    if !ctx.shared_charon.is_alive(runner.as_ref()) {
        eprintln!(
            "[supervise] line {idx}: FATAL: shared charon is not running; skipping this line"
        );
        return;
    }

    // Load the union of every line's connection file, then initiate only this
    // line's child. Ordering between concurrently-starting lines is safe: each
    // line loads *after* writing its own file, so its own connection is always
    // present, and a directory-wide load can never evict another line's.
    ctx.shared_charon.load_all(runner.as_ref());
    let _ = runner.spawn_detached(ChildSpec::new([
        "env",
        &format!("STRONGSWAN_CONF={SHARED_STRONGSWAN_CONF}"),
        "swanctl",
        "--initiate",
        "--child",
        &conn_name,
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

    start_line_tail(ctx, idx, &netns, line, &usim_holder, pcscf.clone());

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
                if let Some(new_handle) = engine.shared.current_handle() {
                    let mut st = started.lock().unwrap();
                    // Dedupe by identity: most recoveries are connection-
                    // scoped and leave the daemon untouched, so without this
                    // every line would re-push the same handle on every
                    // recovery tick and grow the shutdown list forever.
                    if !st
                        .vowifi_child_handles
                        .iter()
                        .any(|h| Arc::ptr_eq(h, &new_handle))
                    {
                        st.vowifi_child_handles.push(new_handle);
                    }
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
fn start_line_tail(
    ctx: &LineStartup,
    idx: u32,
    netns: &str,
    line: &LineResolutionEntry,
    usim_holder: &Arc<Mutex<Option<Arc<ChildHandle>>>>,
    initial_pcscf: String,
) {
    let runner = ctx.runner;
    let bin = ctx.bin;
    let config_path = ctx.config_path;
    let started = ctx.started;
    let shutting_down = ctx.shutting_down;
    let alert_ctx = ctx.alert_ctx;
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
                let handle = Arc::new(handle);
                started
                    .lock()
                    .unwrap()
                    .vowifi_child_handles
                    .push(handle.clone());
                drop(guard);
                // See the daemon_supervisor loop earlier in this file: poll
                // is_alive(), don't block on wait(), so this handle
                // (signaled by execute_shutdown_plan via
                // `vowifi_child_handles`) stays signalable for as long as
                // the process is actually alive.
                while runner.is_alive(&handle) {
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
                    let holder = usim_holder.lock().unwrap().clone();
                    let reset_log = PathBuf::from(format!("/tmp/sim-reset-{idx}.log"));
                    sim_recovery::reset_modem_sim(
                        runner.as_ref(),
                        Path::new(&modem),
                        &reset_log,
                        holder.as_deref(),
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
            reasonable default matching the original script's own $KEEPALIVE_INTERVAL
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

fn start_vowifi_line_swu(ctx: &LineStartup, line: &LineResolutionEntry, mcc: &str, mnc: &str) {
    let runner = ctx.runner;
    let config = ctx.config;
    let started = ctx.started;
    let shutting_down = ctx.shutting_down;
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
    if let Some(h) = engine.dialer_handle.borrow().clone() {
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

    let usim_holder: Arc<Mutex<Option<Arc<ChildHandle>>>> = Arc::new(Mutex::new(None));
    start_line_tail(ctx, idx, &netns, line, &usim_holder, pcscf.clone());

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
                if let Some(h) = engine.dialer_handle.borrow().clone() {
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
    fn discover_retry_tick_reports_an_identifier_missing_from_this_ticks_scan_as_resolved() {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let pending = std::collections::HashMap::from([("/dev/ttyUSB3".to_string(), deadline)]);
        let alerted = std::collections::HashSet::new();
        let still_missing = std::collections::HashSet::new();
        let (resolved, expired) = discover_retry_tick(
            &pending,
            &alerted,
            &still_missing,
            std::time::Instant::now(),
        );
        assert_eq!(resolved, vec!["/dev/ttyUSB3".to_string()]);
        assert!(expired.is_empty());
    }

    #[test]
    fn discover_retry_tick_reports_nothing_while_still_missing_and_within_the_window() {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let pending = std::collections::HashMap::from([("/dev/ttyUSB3".to_string(), deadline)]);
        let alerted = std::collections::HashSet::new();
        let still_missing = std::collections::HashSet::from(["/dev/ttyUSB3".to_string()]);
        let (resolved, expired) = discover_retry_tick(
            &pending,
            &alerted,
            &still_missing,
            std::time::Instant::now(),
        );
        assert!(resolved.is_empty());
        assert!(expired.is_empty());
    }

    #[test]
    fn discover_retry_tick_expires_an_identifier_whose_window_has_elapsed() {
        // Deadline already in the past relative to "now" below — simulates
        // DISCOVER_RETRY_WINDOW having elapsed without needing to actually
        // wait for it.
        let deadline = std::time::Instant::now() - Duration::from_secs(1);
        let pending = std::collections::HashMap::from([("/dev/ttyUSB3".to_string(), deadline)]);
        let alerted = std::collections::HashSet::new();
        let still_missing = std::collections::HashSet::from(["/dev/ttyUSB3".to_string()]);
        let (resolved, expired) = discover_retry_tick(
            &pending,
            &alerted,
            &still_missing,
            std::time::Instant::now(),
        );
        assert!(resolved.is_empty());
        assert_eq!(expired, vec!["/dev/ttyUSB3".to_string()]);
    }

    #[test]
    fn discover_retry_tick_resolving_wins_over_an_expired_deadline() {
        // The tick that finally finds the device is allowed to arrive on
        // the same poll as its deadline passing — it must still count as
        // resolved, not expired.
        let deadline = std::time::Instant::now() - Duration::from_secs(1);
        let pending = std::collections::HashMap::from([("/dev/ttyUSB3".to_string(), deadline)]);
        let alerted = std::collections::HashSet::new();
        let still_missing = std::collections::HashSet::new();
        let (resolved, expired) = discover_retry_tick(
            &pending,
            &alerted,
            &still_missing,
            std::time::Instant::now(),
        );
        assert_eq!(resolved, vec!["/dev/ttyUSB3".to_string()]);
        assert!(expired.is_empty());
    }

    #[test]
    fn discover_retry_tick_handles_two_configured_lines_independently() {
        // spec.md User Story 1 acceptance scenario 3: one line resolves,
        // the other is still missing and not yet expired — each identifier
        // is judged on its own.
        let now = std::time::Instant::now();
        let pending = std::collections::HashMap::from([
            ("/dev/ttyUSB3".to_string(), now + Duration::from_secs(60)),
            ("/dev/ttyUSB5".to_string(), now + Duration::from_secs(60)),
        ]);
        let alerted = std::collections::HashSet::new();
        let still_missing = std::collections::HashSet::from(["/dev/ttyUSB5".to_string()]);
        let (resolved, expired) = discover_retry_tick(&pending, &alerted, &still_missing, now);
        assert_eq!(resolved, vec!["/dev/ttyUSB3".to_string()]);
        assert!(expired.is_empty());
    }

    /// specs/027-discover-retry-health FR-009/SC-004: once an identifier's
    /// `Failure` alert has fired, later ticks must not report it as
    /// newly-expired again — that would re-fire the alert every poll while
    /// permanently stuck, exactly the noise `sim_alert_transition`'s own
    /// `given_up_alerted` flag exists to prevent for a different category.
    #[test]
    fn discover_retry_tick_does_not_re_report_an_already_alerted_identifier_as_expired() {
        let deadline = std::time::Instant::now() - Duration::from_secs(1);
        let pending = std::collections::HashMap::from([("/dev/ttyUSB3".to_string(), deadline)]);
        let alerted = std::collections::HashSet::from(["/dev/ttyUSB3".to_string()]);
        let still_missing = std::collections::HashSet::from(["/dev/ttyUSB3".to_string()]);
        let (resolved, expired) = discover_retry_tick(
            &pending,
            &alerted,
            &still_missing,
            std::time::Instant::now(),
        );
        assert!(resolved.is_empty());
        assert!(
            expired.is_empty(),
            "already-alerted identifiers must not re-expire every tick"
        );
    }

    /// The edge case from spec.md: a line can still recover *after* its
    /// `Failure` alert already fired — `discover_retry_tick` itself doesn't
    /// know about alerting, but must still report it resolved so the
    /// caller can pair a `Recovered` notice with it.
    #[test]
    fn discover_retry_tick_reports_an_already_alerted_identifier_as_resolved_once_it_recovers() {
        let deadline = std::time::Instant::now() - Duration::from_secs(1);
        let pending = std::collections::HashMap::from([("/dev/ttyUSB3".to_string(), deadline)]);
        let alerted = std::collections::HashSet::from(["/dev/ttyUSB3".to_string()]);
        let still_missing = std::collections::HashSet::new();
        let (resolved, expired) = discover_retry_tick(
            &pending,
            &alerted,
            &still_missing,
            std::time::Instant::now(),
        );
        assert_eq!(resolved, vec!["/dev/ttyUSB3".to_string()]);
        assert!(expired.is_empty());
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

    fn test_shared_charon() -> Arc<SharedCharon> {
        Arc::new(SharedCharon::new(
            SHARED_STRONGSWAN_CONF.to_string(),
            SHARED_SWANCTL_CONF.to_string(),
            PathBuf::from(SHARED_CHARON_LOG),
        ))
    }

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
        let config = VowifiConfig {
            imsi_override: Some("404940123456789".to_string()),
            ..Default::default()
        };
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

    fn ok_stdout(stdout: &str) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn resolve_mcc_mnc_prefers_an_explicitly_configured_plmn() {
        let mock = Arc::new(MockCommandRunner::new());
        let line = pcsc_line(0); // has mcc/mnc set
        assert_eq!(
            resolve_mcc_mnc(mock.as_ref(), "gsm-sip-bridge", &line),
            Some(("404".to_string(), "043".to_string()))
        );
        assert!(
            !ran_argv_containing(&mock, "vowifi-plmn"),
            "a pinned mcc/mnc must not trigger a card read at all"
        );
    }

    #[test]
    fn resolve_mcc_mnc_derives_a_pcsc_line_over_the_reader_not_a_modem() {
        // The point of the whole change: a card-reader line with mcc/mnc left
        // unset derives them from its own card, keyed by the IMSI that
        // already identifies its reader. It has no modem port to pass, so the
        // old `--modem ""` form could never have worked here.
        let mock = Arc::new(MockCommandRunner::new());
        mock.set_run_output(
            "gsm-sip-bridge vowifi-plmn --pcsc-imsi 404940123456789",
            ok_stdout("404 094\n"),
        );
        let mut line = pcsc_line(0);
        line.mcc = String::new();
        line.mnc = String::new();
        assert_eq!(
            resolve_mcc_mnc(mock.as_ref(), "gsm-sip-bridge", &line),
            Some(("404".to_string(), "094".to_string()))
        );
        assert!(
            !ran_argv_containing(&mock, "--modem"),
            "a pcsc_reader line must never be asked to derive over a modem"
        );
    }

    #[test]
    fn resolve_mcc_mnc_still_derives_a_modem_line_over_the_modem() {
        let mock = Arc::new(MockCommandRunner::new());
        mock.set_run_output(
            "gsm-sip-bridge vowifi-plmn --modem /dev/ttyUSB0",
            ok_stdout("405 840\n"),
        );
        let mut line = modem_line(0, "/dev/ttyUSB0");
        line.mcc = String::new();
        line.mnc = String::new();
        assert_eq!(
            resolve_mcc_mnc(mock.as_ref(), "gsm-sip-bridge", &line),
            Some(("405".to_string(), "840".to_string()))
        );
    }

    #[test]
    fn resolve_mcc_mnc_reports_failure_when_the_card_cannot_be_read() {
        // No seeded output -> the mock's default is a failing status, which
        // is what a reader holding no matching card looks like. The caller
        // treats None as fatal for the line rather than inventing a PLMN.
        let mock = Arc::new(MockCommandRunner::new());
        let mut line = pcsc_line(0);
        line.mcc = String::new();
        line.mnc = String::new();
        assert_eq!(
            resolve_mcc_mnc(mock.as_ref(), "gsm-sip-bridge", &line),
            None
        );
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
        let shared_charon = test_shared_charon();

        start_vowifi_line(
            &LineStartup {
                runner: &runner,
                bin: "gsm-sip-bridge",
                config_path: "/tmp/cfg.toml",
                config: &config,
                started: &started,
                shutting_down: &shutting_down,
                alert_ctx: None,
                shared_charon: &shared_charon,
            },
            &line,
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
        let shared_charon = test_shared_charon();

        start_vowifi_line_strongswan(
            &LineStartup {
                runner: &runner,
                bin: "gsm-sip-bridge",
                config_path: "/tmp/cfg.toml",
                config: &config,
                started: &started,
                shutting_down: &shutting_down,
                alert_ctx: None,
                shared_charon: &shared_charon,
            },
            &line,
            &line.mcc.clone(),
            &line.mnc.clone(),
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
        let shared_charon2 = test_shared_charon();
        start_vowifi_line_strongswan(
            &LineStartup {
                runner: &runner2,
                bin: "gsm-sip-bridge",
                config_path: "/tmp/cfg.toml",
                config: &config,
                started: &started2,
                shutting_down: &shutting_down2,
                alert_ctx: None,
                shared_charon: &shared_charon2,
            },
            &modem,
            &modem.mcc.clone(),
            &modem.mnc.clone(),
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
