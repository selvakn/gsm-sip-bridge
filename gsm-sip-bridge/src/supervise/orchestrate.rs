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
use crate::config::{AppConfig, TunnelAddressFamily};
use crate::vowifi::discovery::LineResolutionEntry;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

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
pub(crate) const SHARED_STRONGSWAN_CONF: &str = "/etc/strongswan-shared.conf";
pub(super) const SHARED_SWANCTL_CONF: &str = "/etc/swanctl/swanctl.conf";
pub(super) const SHARED_SWANCTL_CONF_DIR: &str = "/etc/swanctl/conf.d";
pub(super) const SHARED_CHARON_LOG: &str = "/tmp/charon.log";
pub(super) const SHARED_VICI_SOCKET: &str = "/var/run/charon.vici";
/// Overwritten at startup from the resolved line set — the osmocom P-CSCF
/// plugin enables its attribute request per *connection name*, so the image's
/// static copy cannot know them. See `render::render_pcscf_plugin_conf`.
///
/// `pub(super)`: specs/080-volte-pcscf-auto-prime's transient priming capture
/// (`orchestrate_prime`) renders this too, for its own one-off connection —
/// this is a real charon process reading a real, single, host-wide file, not
/// something `SharedCharon`'s per-instance paths can isolate (see that
/// feature's research.md R2).
pub(super) const PCSCF_PLUGIN_CONF: &str = "/etc/strongswan.d/charon/p-cscf.conf";

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
///
/// `pub(super)`: specs/080-volte-pcscf-auto-prime's transient priming
/// capture (`orchestrate_prime`) constructs one of these for its own
/// synthetic, one-off line, reusing `prepare_vowifi_line`/
/// `establish_line_tunnel` exactly as a real persistent line does.
pub(super) struct LineStartup<'a> {
    pub(super) runner: &'a Arc<dyn CommandRunner>,
    pub(super) bin: &'a str,
    pub(super) config_path: &'a str,
    pub(super) config: &'a AppConfig,
    pub(super) started: &'a Arc<Mutex<StartedState>>,
    pub(super) shutting_down: &'a Arc<RwLock<bool>>,
    pub(super) alert_ctx: Option<&'a AlertContext>,
    /// The charon daemon every strongswan-engine line shares.
    pub(super) shared_charon: &'a Arc<SharedCharon>,
    /// The real, container-wide shutdown flag — distinct from `shutting_down`
    /// above for callers whose `shutting_down` is a local, per-attempt flag
    /// rather than the real one (`orchestrate_prime`'s transient priming
    /// capture; see its own doc comment for why it can't just use the real
    /// flag there). `establish_line_tunnel`'s establish-loop polls this, when
    /// present, so a real shutdown can abort a stuck attempt within one poll
    /// interval instead of running out its own several-minute ceiling
    /// (Greptile PR #87 review, finding 2). `None` for every persistent-line
    /// call site, which already passes the real flag as `shutting_down`
    /// itself, so a second copy here would be redundant.
    pub(super) real_shutting_down: Option<&'a Arc<RwLock<bool>>>,
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
/// specs/033-slim-optional-swu-image: filesystem markers that tell a running
/// bridge whether the SWu/Python tunnel-engine payload is present in this
/// image. The on-demand full (`-swu`) image ships both; the default slim image
/// ships neither.
const SWU_MARKER_PATH: &str = "/etc/gsm-sip-bridge/swu-available";
const SWU_DIALER_PATH: &str = "/opt/SWu-IKEv2/swu_emulator.py";

/// True iff this image carries the SWu engine payload. Does filesystem I/O
/// (the two `exists()` probes) but takes its paths by argument rather than
/// hard-coding them, so it is unit-testable against temp files while the caller
/// passes the real image paths above.
fn swu_payload_available(marker: &Path, dialer: &Path) -> bool {
    marker.exists() && dialer.exists()
}

/// Env var that bypasses the SWu-payload check — the escape hatch for running
/// `gsm-sip-bridge supervise` natively on the host off a pjsip-linked build,
/// where the container-only marker file never exists.
const SWU_PAYLOAD_OVERRIDE_ENV: &str = "GSM_SIP_BRIDGE_SWU_PAYLOAD";

/// The actionable fatal message when a `swu`-engine config lands on an image
/// (or host) that has no SWu payload. Shared so the pre-discovery guard in
/// `run` speaks with one voice. Embeds the binary's own version so the `-swu`
/// tag is copy-pasteable.
fn swu_unavailable_message() -> &'static str {
    concat!(
        "[vowifi].tunnel_engine = \"swu\", but this image does not include the ",
        "SWu/Python engine — the default \"slim\" image ships strongSwan only. ",
        "Pull the on-demand full image (tag \"",
        env!("CARGO_PKG_VERSION"),
        "-swu\"), or set [vowifi].tunnel_engine = \"strongswan\". ",
        "(Running natively off a pjsip-linked host build, not the container? ",
        "Set GSM_SIP_BRIDGE_SWU_PAYLOAD=1 to bypass this check.)"
    )
}

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

    // Once per run, before any line starts (specs/094 contracts/logs.md).
    match idle_keepalive_interval(&config.vowifi) {
        None => tracing::info!(
            idle_keepalive = false,
            "{}",
            idle_keepalive_log_line(&config.vowifi)
        ),
        Some(interval) => tracing::info!(
            idle_keepalive = true,
            interval_sec = interval.as_secs(),
            "{}",
            idle_keepalive_log_line(&config.vowifi)
        ),
    }
    // Also once per run (specs/098 contracts/config.md).
    tracing::info!(
        dpd_delay_sec = config.vowifi.dpd_delay_sec,
        "{}",
        dpd_log_line(&config.vowifi)
    );

    // Note: the swu-on-slim payload check is NOT here — it is an image
    // property, not a line-table one, so `run` performs it before discovery
    // (see the `swu_payload_available` guard there). Sitting behind line
    // resolution would let a slim deployment with no modem attached, or one
    // reaching this via the discover-retry path (which only logs), keep
    // running as a VoWiFi-less bridge instead of exiting (specs/033, SC-005).

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

    // Before anything creates an interface, starts charon or starts pcscd:
    // clear XFRM state left by a previous run of this container. It outlives
    // the container and keeps these lines' if_ids claimed, which makes
    // their tunnel interfaces impossible to create — the line then
    // re-establishes its tunnel every steady-state tick, forever.
    // Guarded so a host running unrelated IPsec is never touched.
    let our_if_ids: std::collections::BTreeSet<u32> =
        vowifi_lines.iter().map(|l| l.strongswan_if_id).collect();
    epdg_iface::reclaim_stale_xfrm(runner.as_ref(), &our_if_ids);

    // specs/041-shutdown-resource-cleanup US2/FR-014: a run that was
    // force-killed (rather than stopped) never got to run the steps above —
    // its namespace, tunnel interface and veth end can all still be sitting
    // on the host. Reclaim them too, using each line's own resolved names
    // (a name this run's own line table could also produce is, by
    // construction, ours — research.md R7), before this run creates
    // anything of its own. Requires the per-line namespace directory to be
    // visible from the host (docker-compose.yml's /var/run/netns bind
    // mount, research.md R6); on a host without it this is a silent no-op,
    // same as finding nothing to reclaim.
    // Only the strongswan engine ever creates an XFRM tun device; a leftover
    // swu-engine namespace has nothing of that kind to delete (its tunnel is
    // a plain TUN torn down with its dialer process, not a device with a
    // claimable if_id — the defect this whole feature exists for).
    //
    // Deliberately runs before pcscd/vpcd startup below, not after (specs/041
    // follow-up, github.com/selvakn/gsm-sip-bridge/issues/53): a force-killed
    // restart can leave pcscd's own port transiently unbindable, and pcscd
    // startup used to be the very first thing this function did, so that one
    // FATAL exit meant a force-killed run's leftovers never got reclaimed
    // either — defeating the point of the reclamation this feature exists
    // for on exactly the case it was built for. Reclaiming first means that
    // even if pcscd still fails after its own retries below, this run's
    // namespace/interface leftovers are already clean for whatever tries
    // next (Docker's own restart policy, or a person re-running `docker
    // start`).
    let is_strongswan = config.vowifi.tunnel_engine == "strongswan";
    let reclaim_candidates: Vec<epdg_iface::ReclaimCandidate> = vowifi_lines
        .iter()
        .map(|l| epdg_iface::ReclaimCandidate {
            netns: l.netns.clone(),
            tun_iface: is_strongswan.then(|| l.strongswan_tun_iface.clone()),
            veth_host: Some(l.config.veth_sip_iface.clone()),
            // Proof of ownership: only this deployment puts a `tun23-N`
            // inside an `imsN`. A swu line has no such device, so it cannot
            // prove ownership and is never reclaimed (`None` vetoes it).
            owned_iface_marker: is_strongswan.then(|| l.strongswan_tun_iface.clone()),
        })
        .collect();
    epdg_iface::reclaim_leftover_lines(
        runner.as_ref(),
        &reclaim_candidates,
        epdg_iface::reclaim_leftover_enabled(),
    );

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
            println!(
                "[supervise] starting shared pcscd; one vpcd reader, slots from {}",
                config.vowifi.vpcd_port
            );
        }
        // Retries the whole spawn+ready cycle rather than failing on the
        // first hiccup (github.com/selvakn/gsm-sip-bridge/issues/53): a
        // force-killed container's old pcscd can still be holding this
        // host-shared port (`network_mode: host`) for a brief window after
        // `docker kill` returns, and a single attempt had no way to tell
        // that transient race apart from a genuinely misconfigured port.
        let pcscd_handle = match vpcd::start_pcscd_with_retries(
            runner.as_ref(),
            needs_vpcd,
            &config.vowifi.vpcd_host,
            config.vowifi.vpcd_port,
        ) {
            Ok(handle) => Arc::new(handle),
            Err(vpcd::StartPcscdError::SpawnFailed) => {
                return Err("failed to start pcscd".to_string());
            }
            Err(vpcd::StartPcscdError::NotReady(other)) => {
                return Err(format!(
                        "pcscd's vpcd reader never came up on {}:{} after {} attempt(s) ({other:?}). \
                         If pcscd logged 'Address in use', another process holds that port — pick a \
                         [vowifi].vpcd_port below the ephemeral range.",
                        config.vowifi.vpcd_host,
                        config.vowifi.vpcd_port,
                        vpcd::PCSCD_STARTUP_ATTEMPTS
                    ));
            }
        };
        started.lock().unwrap().pcscd = Some(pcscd_handle.clone());

        if needs_vpcd {
            println!(
                "[supervise] vpcd reader ready on {}:{}",
                config.vowifi.vpcd_host, config.vowifi.vpcd_port
            );
        } else {
            println!(
                "[supervise] started shared pcscd; no vpcd reader \
                 (all {} line(s) are card-reader-backed)",
                vowifi_lines.len()
            );
        }
    }

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
                    real_shutting_down: None,
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
/// Which of `pending`'s identifiers have *not* actually become a running
/// line in `resolution` yet. Membership in `resolution.lines` is the only
/// correct definition of "resolved" — checking whether `resolution.failed`
/// still reports a `not_found` entry for the id, as an earlier version of
/// this did, is not equivalent: a pending id can go from `not_found` (no
/// modem matched at all) to some *other* rejection on a later tick (found
/// on real EC20 hardware while testing this PR — the modem showed up but
/// its SIM was `sim_unreadable`), which drops it out of a reason-filtered
/// set even though it still never made it into `resolution.lines`. Treating
/// that as "resolved" started the subsystem on whatever (possibly empty)
/// set of lines *did* resolve, permanently dropped the still-broken id from
/// `pending` so it was never retried again, and could fire a false
/// `Recovered` alert for a line that was never actually started.
///
/// `pending`'s keys are *configured-override* identifiers
/// (`vowifi::discovery::override_identifier` — the override's own
/// `modem_port` or `modem_serial` string), and a resolved line reports the
/// identifier of the override that produced it in
/// `LineResolutionEntry::configured_identifier`. Both sides come from that
/// one shared function, so this is an exact string match with nothing
/// reconstructed in between.
///
/// That property is the whole point, and it was learned the hard way — two
/// successive P1 review findings on specs/027-discover-retry-health, both
/// from trying to *infer* the correspondence after the fact. Comparing a
/// pending key against `card_id` never matched at all (different identifier
/// spaces, so a recovered line stayed "missing" forever, the subsystem
/// never started, and a false discovery-failure alert eventually fired);
/// bridging them through `derive_module_id` then matched too eagerly (it
/// keeps only the last six alphanumerics, uppercased, so two distinct
/// serials sharing that suffix collapse to one `card_id` and an unrelated
/// modem resolving reads as the configured one recovering). Recording the
/// match where it is actually known removes the guesswork instead of
/// refining it.
fn still_missing_from_resolution(
    pending: &std::collections::HashMap<String, std::time::Instant>,
    resolution: &crate::vowifi::discovery::LineResolution,
) -> std::collections::HashSet<String> {
    let resolved: std::collections::HashSet<&str> = resolution
        .lines
        .iter()
        .filter_map(|l| l.configured_identifier.as_deref())
        .collect();
    pending
        .keys()
        .filter(|id| !resolved.contains(id.as_str()))
        .cloned()
        .collect()
}

/// specs/034-alert-identity: the configured phone number for a VoWiFi line
/// identified by `id` (its `modem_port`/`modem_serial` override identifier) —
/// for a line-discovery alert fired before the line ever resolved into a
/// `LineResolutionEntry`. `None` when no override carries an msisdn.
fn configured_line_msisdn(config: &crate::config::AppConfig, id: &str) -> Option<String> {
    config
        .vowifi
        .line_overrides
        .iter()
        .find(|o| crate::vowifi::discovery::override_identifier(o).as_deref() == Some(id))
        .and_then(|o| o.msisdn.clone())
}

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
            let still_missing = still_missing_from_resolution(&pending, &resolution);
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
                        phone_number: configured_line_msisdn(&config, id),
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
                                phone_number: configured_line_msisdn(&config, id),
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
        Ok(rt) => match DiscordClient::new(
            Secret::new(String::new()),
            alerts::instance_label(&config.alerts),
        ) {
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
        // specs/033: fail fast BEFORE discovery when the `swu` engine is
        // configured on a slim image that has no SWu payload. This is a
        // property of the image, not of the line table — checking it here
        // makes it unconditional: a slim deployment with no modem attached (so
        // no lines ever resolve), or one that reaches the subsystem via the
        // discover-retry path (which only logs, never exits), would otherwise
        // run indefinitely as a VoWiFi-less bridge instead of exiting (SC-005).
        // The env override is the escape hatch for a native host build, where
        // the container-only marker never exists.
        if config.vowifi.tunnel_engine == "swu"
            && std::env::var_os(SWU_PAYLOAD_OVERRIDE_ENV).is_none()
            && !swu_payload_available(Path::new(SWU_MARKER_PATH), Path::new(SWU_DIALER_PATH))
        {
            eprintln!("[supervise] FATAL: {}", swu_unavailable_message());
            return ExitCode::FAILURE;
        }

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
    // Reserve is derived from this run's own plan rather than a fixed
    // constant, so it stays correct for whatever line count actually
    // started (see `release_reserve_for`).
    let budget = shutdown::TeardownBudget::new(
        shutdown::STOP_ALLOWANCE,
        shutdown::release_reserve_for(&plan),
    );
    let outcome = shutdown::execute_shutdown_plan(&plan, runner.as_ref(), &budget);
    report_teardown_outcome(&outcome);

    ExitCode::SUCCESS
}

/// Renders [`shutdown::TeardownOutcome`] per contracts/observable-contracts.md
/// C2. Reporting only (FR-020): this never raises an alert and never changes
/// the exit code above, on either the abandoned-step or the not-released
/// case.
fn report_teardown_outcome(outcome: &shutdown::TeardownOutcome) {
    let abandoned: Vec<&str> = outcome.abandoned().collect();
    if !abandoned.is_empty() {
        println!(
            "[supervise] teardown: out of time, skipping {} remaining wait(s)/flush(es) to \
             release devices and namespaces",
            abandoned.len()
        );
    }
    let mut not_released = 0usize;
    for (label, reason) in outcome.not_released() {
        eprintln!("[supervise] teardown: could not release {label}: {reason}");
        not_released += 1;
    }
    let released = outcome.steps.len() - abandoned.len() - not_released;
    println!(
        "[supervise] teardown: complete, {released} step(s) completed, {not_released} \
         resource(s) not released, {} abandoned to the stop allowance",
        abandoned.len()
    );
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
    // In-process resolution via the system resolver (specs/033): drops the
    // `dig`/`bind-tools` runtime dependency. Takes the first A record, exactly
    // as the previous `dig +short A | first line` did.
    runner
        .resolve_host(&fqdn)
        .ok()?
        .first()
        .map(std::net::Ipv4Addr::to_string)
}

#[cfg(test)]
mod resolve_epdg_ip_tests {
    use super::*;
    use crate::supervise::runner::MockCommandRunner;
    use std::net::Ipv4Addr;

    #[test]
    fn publish_pcscf_writes_the_pool_before_the_selection() {
        let runner = MockCommandRunner::new();
        let pool = vec!["10.0.1.10".to_string(), "10.0.0.10".to_string()];
        assert!(publish_pcscf(
            &runner,
            0,
            "/tmp/pcscf-0",
            "10.0.0.10",
            &pool
        ));
        assert_eq!(
            *runner.write_order.lock().unwrap(),
            vec![
                PathBuf::from("/tmp/pcscf-0.pool"),
                PathBuf::from("/tmp/pcscf-0")
            ]
        );
        let files = runner.files.lock().unwrap();
        assert_eq!(
            files[&PathBuf::from("/tmp/pcscf-0.pool")],
            "10.0.1.10\n10.0.0.10\n"
        );
        // The selection file keeps its single-address format (FR-004).
        assert_eq!(files[&PathBuf::from("/tmp/pcscf-0")], "10.0.0.10");
    }

    fn addrs(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_reordered_pool_is_not_republished() {
        // SC-002: the carrier reorders one pool freely; that is never news.
        assert_eq!(
            steady_pool_update(
                &addrs(&["10.0.0.10", "10.0.1.10"]),
                addrs(&["10.0.1.10", "10.0.0.10"])
            ),
            None
        );
        assert_eq!(steady_pool_update(&addrs(&["10.0.0.10"]), Vec::new()), None);
    }

    #[test]
    fn an_added_address_republishes_the_pool_without_a_restart() {
        // US2 AS3: a new member only joins the candidates.
        let runner = MockCommandRunner::new();
        let mut current = addrs(&["10.0.0.10", "10.0.1.10"]);
        let mut published = current.clone();
        republish_pool_after_set_change(
            &runner,
            0,
            "/tmp/pcscf-0",
            &mut current,
            &mut published,
            addrs(&["10.0.1.10", "10.0.0.10", "10.0.2.10"]),
        );
        assert_eq!((current.len(), published.len()), (3, 3));
        assert_eq!(
            *runner.write_order.lock().unwrap(),
            vec![PathBuf::from("/tmp/pcscf-0.pool")],
            "the selection file is not rewritten"
        );
        assert!(runner.run_calls.lock().unwrap().is_empty(), "no pkill");
    }

    #[test]
    fn a_failed_pool_write_is_reported_and_retried_not_recorded() {
        // Greptile (PR #130): a write that failed must not count as published.
        let runner = MockCommandRunner::new();
        let pool_path = PathBuf::from("/tmp/pcscf-0.pool");
        runner
            .failing_writes
            .lock()
            .unwrap()
            .push(pool_path.clone());
        assert!(!publish_pcscf(
            &runner,
            0,
            "/tmp/pcscf-0",
            "10.0.0.10",
            &addrs(&["10.0.0.10", "10.0.1.10"])
        ));
        assert_eq!(
            runner.files.lock().unwrap()[&PathBuf::from("/tmp/pcscf-0")],
            "10.0.0.10",
            "the selection is still published"
        );
        let mut current = Vec::new();
        let mut published = Vec::new();
        republish_pool_after_set_change(
            &runner,
            0,
            "/tmp/pcscf-0",
            &mut current,
            &mut published,
            addrs(&["10.0.0.10", "10.0.1.10"]),
        );
        assert!(
            published.is_empty(),
            "still unpublished, so retried next tick"
        );
        assert_eq!(
            current,
            addrs(&["10.0.0.10", "10.0.1.10"]),
            "the fallbacks still feed the reachability check"
        );
        runner.failing_writes.lock().unwrap().clear();
        republish_pool_after_set_change(
            &runner,
            0,
            "/tmp/pcscf-0",
            &mut current,
            &mut published,
            addrs(&["10.0.0.10", "10.0.1.10"]),
        );
        assert_eq!(published, addrs(&["10.0.0.10", "10.0.1.10"]));
        assert_eq!(
            runner.files.lock().unwrap()[&pool_path],
            "10.0.0.10\n10.0.1.10\n"
        );
    }

    #[test]
    fn publish_pcscf_with_no_pool_publishes_the_selection_as_the_pool() {
        let runner = MockCommandRunner::new();
        assert!(publish_pcscf(&runner, 1, "/tmp/pcscf-1", "10.0.0.10", &[]));
        assert_eq!(
            runner.files.lock().unwrap()[&PathBuf::from("/tmp/pcscf-1.pool")],
            "10.0.0.10\n"
        );
    }

    #[test]
    fn explicit_ip_override_short_circuits_before_dns() {
        let runner = MockCommandRunner::new();
        // No resolve_host seeding at all — the override must win regardless.
        let got = resolve_epdg_ip(&runner, "", Some("198.51.100.7"), "404", "10");
        assert_eq!(got.as_deref(), Some("198.51.100.7"));
    }

    #[test]
    fn empty_ip_override_falls_through_to_fqdn_override_resolution() {
        let runner = MockCommandRunner::new();
        runner.set_resolve_host(
            "epdg.example.net",
            vec![Ipv4Addr::new(203, 0, 113, 5), Ipv4Addr::new(203, 0, 113, 6)],
        );
        let got = resolve_epdg_ip(&runner, "epdg.example.net", Some(""), "404", "10");
        // First A record, matching the old `dig +short | first line`.
        assert_eq!(got.as_deref(), Some("203.0.113.5"));
    }

    #[test]
    fn derived_fqdn_is_resolved_when_no_overrides() {
        let runner = MockCommandRunner::new();
        runner.set_resolve_host(
            "epdg.epc.mnc10.mcc404.pub.3gppnetwork.org",
            vec![Ipv4Addr::new(192, 0, 2, 42)],
        );
        let got = resolve_epdg_ip(&runner, "", None, "404", "10");
        assert_eq!(got.as_deref(), Some("192.0.2.42"));
    }

    #[test]
    fn no_a_record_yields_none() {
        let runner = MockCommandRunner::new();
        // Host unseeded → empty address list → unresolved.
        let got = resolve_epdg_ip(&runner, "unresolvable.example", None, "404", "10");
        assert_eq!(got, None);
    }
}

#[cfg(test)]
mod swu_payload_tests {
    use super::*;

    #[test]
    fn payload_available_only_when_both_marker_and_dialer_present() {
        let dir = std::env::temp_dir().join(format!("swu-payload-test-{}", std::process::id()));
        // Rerun-safe: a previous run that panicked mid-test would leave the
        // marker behind and make the first assertion below fail spuriously
        // forever. Start from a clean slate.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("swu-available");
        let dialer = dir.join("swu_emulator.py");

        // Neither present → slim image.
        assert!(!swu_payload_available(&marker, &dialer));

        // Only the marker → still not usable (partial/broken image).
        std::fs::write(&marker, "").unwrap();
        assert!(!swu_payload_available(&marker, &dialer));

        // Both present → full image.
        std::fs::write(&dialer, "# dialer").unwrap();
        assert!(swu_payload_available(&marker, &dialer));

        std::fs::remove_dir_all(&dir).ok();
    }
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

/// Shared prelude for one VoWiFi line — 1:1 port of `start_line_strongswan`/
/// `start_line_swu`'s common opening (modem presence, IMS mode reconcile,
/// mcc/mnc derivation). Extracted (specs/080-volte-pcscf-auto-prime) so the
/// transient priming capture (`orchestrate_prime`) can run exactly this same
/// prelude for its own synthetic line, instead of duplicating it. `None` on
/// any fatal step, already logged at the point of failure — same behavior as
/// before this was a separate function.
pub(super) fn prepare_vowifi_line(
    ctx: &LineStartup,
    line: &LineResolutionEntry,
) -> Option<(String, String)> {
    let runner = ctx.runner;
    let bin = ctx.bin;
    let config_path = ctx.config_path;
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
            return None;
        }

        if runner
            .run(&[bin, "--config", config_path, "modem-ims", "--modem", &modem])
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!(
                "[supervise] line {idx}: FATAL: could not reconcile modem IMS mode; skipping this line"
            );
            return None;
        }
    }

    let Some((mcc, mnc)) = resolve_mcc_mnc(runner.as_ref(), bin, line) else {
        eprintln!("[supervise] line {idx}: FATAL: could not derive MCC/MNC; skipping this line");
        return None;
    };

    Some((mcc, mnc))
}

/// One VoWiFi line's full startup — dispatches to the engine-specific rest
/// after `prepare_vowifi_line`'s shared prelude.
fn start_vowifi_line(ctx: &LineStartup, line: &LineResolutionEntry) {
    let config = ctx.config;
    let Some((mcc, mnc)) = prepare_vowifi_line(ctx, line) else {
        return;
    };

    if config.vowifi.tunnel_engine == "strongswan" {
        start_vowifi_line_strongswan(ctx, line, &mcc, &mnc);
    } else {
        start_vowifi_line_swu(ctx, line, &mcc, &mnc);
    }
}

/// The shared handle a line's USIM bridge process is tracked through — held
/// by its own supervision thread and, for a persistent line, also handed to
/// `start_line_tail` for per-incident recovery.
pub(super) type UsimHolder = Arc<Mutex<Option<Arc<ChildHandle>>>>;

/// Brings up this line's ePDG tunnel far enough to receive a P-CSCF from the
/// IKE_AUTH config payload — resolves the ePDG address, creates the netns +
/// XFRM tun interface, resolves the IMSI, renders and loads this line's
/// swanctl connection, spawns its USIM bridge (unless `pcsc_reader`), ensures
/// the shared charon is running, initiates the connection, and waits for
/// `Established`.
///
/// Extracted from `start_vowifi_line_strongswan` (specs/080-volte-pcscf-
/// auto-prime) so the transient priming capture (`orchestrate_prime`) can
/// call exactly this same sequence for its own synthetic line, with its own
/// `ctx` (its own local `started`/`shutting_down`, so nothing it does is
/// visible to the container-wide shutdown plan) and a bounded
/// `max_establish_attempts` — every persistent-line call site keeps passing
/// `None` (unbounded), so this is a pure extraction with no behavior change
/// for them.
///
/// Returns `None` on any fatal step (already logged at the point of
/// failure), or `Some((pcscf, usim_holder, epdg_ip))` — `usim_holder` is returned
/// because a persistent line's caller still needs it afterward, for
/// `start_line_tail`'s per-incident USIM recovery.
#[allow(clippy::too_many_lines)]
pub(super) fn establish_line_tunnel(
    ctx: &LineStartup,
    line: &LineResolutionEntry,
    mcc: &str,
    mnc: &str,
    max_establish_attempts: Option<u32>,
) -> Option<(String, UsimHolder, String)> {
    let runner = ctx.runner;
    let bin = ctx.bin;
    let config_path = ctx.config_path;
    let config = ctx.config;
    let started = ctx.started;
    let shutting_down = ctx.shutting_down;
    let real_shutting_down = ctx.real_shutting_down;
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
        return None;
    };

    if !epdg_iface::ensure_epdg_interface(runner.as_ref(), &netns, &tun_iface, &if_id) {
        eprintln!(
            "[supervise] line {idx}: {tun_iface} is still absent from netns {netns} after \
             setup; this line's tunnel cannot carry traffic and its supervisor will keep \
             retrying (see the error above for why the interface could not be created)"
        );
    }
    {
        // Held across this registration purely so a real shutdown's write-
        // lock acquisition on `shutting_down`/`real_shutting_down` can never
        // proceed while it is in flight — this line's netns/tun iface were
        // already created above regardless, so there is nothing to gate,
        // only something to synchronize (the same reasoning as the two
        // spawn-then-register sections below, which do gate on it — this
        // one runs unconditionally, matching how a failed `ensure_epdg_
        // interface` above still gets recorded so teardown can still find
        // it).
        let guard = shutting_down.read().unwrap();
        let real_guard = real_shutting_down.map(|f| f.read().unwrap());
        let mut st = started.lock().unwrap();
        st.started_netns.push(netns.clone());
        // Recorded here — the same point `started_netns.push` occupies —
        // rather than after the tunnel actually establishes: a line that
        // fails later must still be fully torn down (FR-007, spec Edge
        // Cases "Only some lines started").
        st.vowifi_lines.push(shutdown::StartedVowifiLine {
            index: idx,
            strongswan: Some(shutdown::StrongswanTeardownInfo {
                conn_name: conn_name.clone(),
                tun_iface: tun_iface.clone(),
                if_id: line.strongswan_if_id,
            }),
            netns: netns.clone(),
            veth_host: line.config.veth_sip_iface.clone(),
        });
        drop(st);
        drop(guard);
        drop(real_guard);
    }

    let Some(imsi) = resolve_imsi(runner.as_ref(), bin, line) else {
        eprintln!("[supervise] line {idx}: FATAL: failed to read IMSI; skipping this line");
        return None;
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
    let address_family = line.config.tunnel_address_family;
    println!("[supervise] line {idx}: tunnel address family: {address_family}");
    let params = super::render::SwanctlEpdgParams {
        conn_name: &conn_name,
        imsi: &imsi,
        mcc,
        mnc,
        epdg_ip: &epdg_ip,
        if_id: &if_id,
        updown_script: &updown_path,
        src_addr: src_addr.as_deref(),
        address_family,
        dpd_delay_sec: config.vowifi.dpd_delay_sec,
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
        let real_shutting_down = real_shutting_down.cloned();
        std::thread::spawn(move || loop {
            let guard = shutting_down.read().unwrap();
            if *guard {
                return;
            }
            // `shutting_down` above is priming's own *local* flag when this
            // is a transient priming capture (`orchestrate_prime`), so a
            // real container shutdown's write-lock acquisition never waits
            // on `guard` for it. Held alongside it, when present, so the
            // real one is covered too — `None` (and therefore a no-op) for
            // every persistent-line call site, which already passes the
            // real flag as `shutting_down` itself (Greptile PR #87 review,
            // same class of gap as "Detached Startup Races Shutdown", found
            // for `start_multiline`'s own loop; closed here too since this
            // function is priming's only other resource-registering path).
            let real_guard = real_shutting_down.as_ref().map(|f| f.read().unwrap());
            if real_guard.as_deref().is_some_and(|v| *v) {
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
                    drop(real_guard);
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
                    drop(real_guard);
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
        max_establish_attempts,
        // The family the connection above was rendered with, so the P-CSCF
        // this establish loop selects is one the tunnel can carry. The VoLTE
        // priming capture comes through here too; its line is a VoWiFi line
        // and carries the same `[vowifi]`-derived family.
        address_family,
        last_inbound: Default::default(),
        last_tun_rx: Default::default(),
        epdg_addr: Some(epdg_ip.clone()),
        last_ike_header: Default::default(),
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
            return None;
        }
        // See the USIM-bridge thread's own comment above for why this is
        // also needed alongside `guard`, not a replacement for it: `None`
        // (a no-op) for every persistent-line call site.
        let real_guard = real_shutting_down.map(|f| f.read().unwrap());
        if real_guard.as_deref().is_some_and(|v| *v) {
            println!("[supervise] line {idx}: shutting down before startup finished; abandoning");
            return None;
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
        return None;
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
    // Logged here too, not only once the tunnel is up: a line whose family
    // excludes every advertised P-CSCF never establishes, and this is the only
    // place that says why.
    let mut logged_ignored = Vec::new();
    let pcscf = loop {
        if ctx.real_shutting_down.is_some_and(|f| *f.read().unwrap()) {
            println!(
                "[supervise] line {idx}: real shutdown requested; abandoning establish attempt"
            );
            break None;
        }
        match line_supervisor::tick_establishing(&engine, runner.as_ref(), &mut attempt, &mut stuck)
        {
            line_supervisor::EstablishOutcome::Established { pcscf } => break Some(pcscf),
            line_supervisor::EstablishOutcome::FatalProcessDied => {
                eprintln!("[supervise] line {idx}: FATAL: charon exited before establishing the tunnel; skipping this line");
                break None;
            }
            // Reachable now that a caller can bound the attempt count
            // (specs/080-volte-pcscf-auto-prime's transient priming capture)
            // — still unreachable for every persistent-line call site, which
            // passes `max_establish_attempts: None`.
            line_supervisor::EstablishOutcome::FatalTimedOut => {
                eprintln!(
                    "[supervise] line {idx}: FATAL: gave up after {attempt} attempt(s) without establishing; skipping this line"
                );
                break None;
            }
            line_supervisor::EstablishOutcome::StillEstablishing => {
                log_ignored_pcscfs(&engine, runner.as_ref(), &mut logged_ignored);
                runner.sleep(line_supervisor::ESTABLISH_POLL_INTERVAL);
            }
        }
    };

    pcscf.map(|p| (p, usim_holder, epdg_ip))
}

fn start_vowifi_line_strongswan(
    ctx: &LineStartup,
    line: &LineResolutionEntry,
    mcc: &str,
    mnc: &str,
) {
    let runner = ctx.runner;
    let started = ctx.started;
    let shutting_down = ctx.shutting_down;
    let idx = line.index;
    let netns = line.netns.clone();

    // specs/086: the CHILD_SA gauge reads 0 from the start of supervision, not
    // from the first healthy tick, so a line that never establishes is visible.
    // If establishment gives up, this reporter is dropped, its record goes
    // stale, and the scrape zeroes the gauge -- the right answer for a line that
    // never had a tunnel.
    let child_sa_reporter = crate::observability::reporter::Reporter::spawn(
        ctx.config.control.socket_path.clone(),
        crate::control::protocol::AgentKind::Supervisor,
        line.card_id.clone(),
        Duration::from_secs(ctx.config.metrics.agent_report_interval_seconds),
    );
    // Every report carries the whole supervisor signal (CHILD_SA, flapping, last
    // IKE_SA loss cause) so ingest never has to infer one from another -- the
    // ones carrying a recovery event too, since the reporter heartbeats
    // whatever state it was last handed.
    let send_tunnel = |s: TunnelSignal, events: Vec<crate::control::protocol::ObservedEvent>| {
        child_sa_reporter.report(
            crate::control::protocol::AgentState {
                child_sa_up: Some(s.child_sa_up),
                tunnel_flapping: Some(s.flapping),
                ike_sa_loss_cause: s.loss_cause,
                ..Default::default()
            },
            events,
        );
    };
    let report_tunnel = |s: TunnelSignal| send_tunnel(s, Vec::new());
    let mut signal = TunnelSignal {
        child_sa_up: false,
        flapping: false,
        loss_cause: None,
    };
    report_tunnel(signal);

    let Some((pcscf, usim_holder, epdg_ip)) = establish_line_tunnel(ctx, line, mcc, mnc, None)
    else {
        return;
    };

    println!("[supervise] line {idx}: tunnel UP. P-CSCF: {pcscf}");
    signal.child_sa_up = true;
    report_tunnel(signal);
    // Rebuilt rather than threaded out of `establish_line_tunnel`: a plain
    // value struct over `ctx.shared_charon` (an `Arc`, cheap to re-derive),
    // and the steady-state loop below needs the exact same identifiers that
    // function already used to initiate this connection.
    let engine = StrongswanEngine {
        idx,
        conn_name: vowifi_conn_name(idx),
        netns: netns.clone(),
        tun_iface: line.strongswan_tun_iface.clone(),
        if_id: line.strongswan_if_id.to_string(),
        shared: Arc::clone(ctx.shared_charon),
        max_establish_attempts: None,
        address_family: line.config.tunnel_address_family,
        last_inbound: Default::default(),
        last_tun_rx: Default::default(),
        epdg_addr: Some(epdg_ip),
        last_ike_header: Default::default(),
    };

    let mut current_pool = engine.latest_pcscf_pool(runner.as_ref());
    // The ignored set last logged, so each one is logged once per change.
    let mut logged_ignored = Vec::new();
    log_ignored_pcscfs(&engine, runner.as_ref(), &mut logged_ignored);
    let mut published_pool = if publish_pcscf(
        runner.as_ref(),
        idx,
        &line.pcscf_source_path,
        &pcscf,
        &current_pool,
    ) {
        current_pool.clone()
    } else {
        Vec::new()
    };

    // Shared with the agent loop: set while a rebuild is syncing the P-CSCF.
    let rebuild_gate = Arc::new(RebuildGate::default());
    // Before the agent starts, so its first marks count (specs/097 R3).
    let negotiated_at = SystemTime::now();
    start_line_tail(
        ctx,
        idx,
        &netns,
        line,
        &usim_holder,
        pcscf.clone(),
        Arc::clone(&rebuild_gate),
    );

    // Steady-state supervision loop — runs for the container's lifetime.
    let mut current_pcscf = pcscf;
    // Consecutive ticks the P-CSCF has been unreachable. Owned by the loop, like
    // `current_pcscf`, so the tick function stays a decision over inputs.
    let mut tracking = line_supervisor::SteadyTracking {
        child_sa_usable: true,
        negotiated_at: Some(negotiated_at),
        ..Default::default()
    };
    // IKE_SA rebuilds inside the tunnel-failure window; latching means flapping.
    let mut rebuilds = line_supervisor::RebuildWindow::new(Duration::from_secs(
        ctx.config.alerts.tunnel_failure_thresholds.unhealthy_sec,
    ));
    let mut last_loss_cause = None;
    // The IKE_SA session already warned about for an INVALID_SELECTORS rekey
    // rejection, so the hint appears once per tunnel session (specs/092).
    let mut warned_inval_sel: Option<String> = None;
    // What the guard believes about the path to the ePDG, and the hold it can
    // put this line in (specs/100).
    let underlay_enabled = true;
    let mut underlay = super::underlay::UnderlayTracker::new(engine.log_mark(runner.as_ref()));
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
            if let Some(end) = underlay.end_for_shutdown(std::time::Instant::now()) {
                println!("{}", hold_ended_line(idx, &end));
            }
            return;
        }
        // Taken before the tick: a recovery inside it renegotiates, and only a
        // negotiation logged after this point can carry the new P-CSCF.
        let mark = engine.log_mark(runner.as_ref());
        let tick_started = std::time::Instant::now();
        let tick_started_at = SystemTime::now();
        let underlay_view = if underlay_enabled {
            let observation = observe_underlay(&engine, runner.as_ref(), &underlay, mark);
            let begin = underlay.begin_tick(&observation, tick_started, tick_started_at);
            if let Some(end) = &begin.ended {
                println!("{}", hold_ended_line(idx, end));
                // Marks the agent set while the path was down describe the
                // outage, not the tunnel (FR-011), and the one initiation a
                // hold allows has done its job (FR-010a).
                tracking.negotiated_at = Some(tick_started_at);
                tracking.held_initiate_sent = false;
            }
            if let Some(start) = begin.started {
                println!(
                    "{}",
                    hold_started_line(
                        idx,
                        start.cause,
                        engine.epdg_addr(),
                        observation.evidence.unanswered_retransmits
                    )
                );
            }
            begin.view
        } else {
            line_supervisor::UnderlayView::disabled()
        };
        // Closed before the tick, not after it: a teardown inside the tick can
        // take many seconds, and an agent that exits meanwhile must not be
        // respawned against the old P-CSCF (specs/087 R5). Released below
        // unless this tick turns out to renegotiate.
        rebuild_gate.close();
        let via_sibling_before = tracking.pcscf_via_sibling;
        let outcome = line_supervisor::tick_steady_state_underlay(
            &engine,
            runner.as_ref(),
            &current_pcscf,
            &current_pool,
            &mut tracking,
            line_supervisor::STEADY_STATE_POLL_INTERVAL,
            &|| line_in_call(runner.as_ref(), &line.pcscf_source_path),
            &|| line_pool_state(runner.as_ref(), &line.pcscf_source_path),
            &underlay_view,
        );
        if underlay_enabled {
            if let Some(start) = underlay.end_tick(&outcome, mark, tick_started, tick_started_at) {
                println!(
                    "{}",
                    hold_started_line(idx, start.cause, engine.epdg_addr(), 0)
                );
            }
        }
        report_pcscf_sibling(idx, &current_pcscf, via_sibling_before, &tracking);
        let renegotiated = matches!(
            outcome,
            line_supervisor::SteadyOutcome::Recovered { reason, .. }
                if line_supervisor::recovery_renegotiates(reason)
        );
        if !renegotiated {
            rebuild_gate.open();
        }
        if rebuilds.tick(std::time::Instant::now()) {
            println!("[supervise] line {idx}: tunnel no longer flapping");
        }
        sync_tunnel_signal(
            &mut signal,
            &tracking,
            &rebuilds,
            last_loss_cause,
            &report_tunnel,
        );
        match outcome {
            line_supervisor::SteadyOutcome::StillUp => {
                // Under the shutdown guard: a repair racing teardown would
                // recreate the pair the plan just deleted.
                repair_line_veth(runner.as_ref(), idx, &netns, line);
                drop(guard);
                republish_pool_after_set_change(
                    runner.as_ref(),
                    idx,
                    &line.pcscf_source_path,
                    &mut current_pool,
                    &mut published_pool,
                    engine.latest_pcscf_pool(runner.as_ref()),
                );
                // Every tick, not only when the published pool changes: a
                // negotiation that changes only excluded addresses leaves the
                // pool as it was.
                log_ignored_pcscfs(&engine, runner.as_ref(), &mut logged_ignored);
                // Only a dual line can be told to narrow its family.
                if engine.address_family == TunnelAddressFamily::Dual {
                    if let Some(warning) = inval_sel_warning(
                        idx,
                        engine.address_family,
                        engine.rekey_invalid_selectors_session(runner.as_ref()),
                        &mut warned_inval_sel,
                    ) {
                        println!("{warning}");
                    }
                }
            }
            // charon is still working on a half-open IKE_SA and the tick has
            // said so (specs/100). Nothing was issued, so the rebuild gate is
            // already open and the signal reported above.
            line_supervisor::SteadyOutcome::WaitingForNegotiation => drop(guard),
            // The tick queued a liveness check and said so (specs/100); the
            // recovery it is for waits for the answer.
            line_supervisor::SteadyOutcome::Deferred { .. } => drop(guard),
            line_supervisor::SteadyOutcome::Held {
                cause,
                skipped,
                initiated,
                ..
            } => {
                drop(guard);
                if initiated {
                    println!(
                        "[supervise] line {idx}: uplink down ({}); no IKE_SA, so one initiation was issued and charon's retries carry it",
                        cause.as_str()
                    );
                } else {
                    println!(
                        "[supervise] line {idx}: uplink down ({}); holding {skipped:?}",
                        cause.as_str()
                    );
                }
            }
            line_supervisor::SteadyOutcome::PcscfChanged { new_pcscf } => {
                drop(guard);
                println!("[supervise] line {idx}: P-CSCF changed ({current_pcscf} -> {new_pcscf}); refreshing");
                current_pool = engine.latest_pcscf_pool(runner.as_ref());
                log_ignored_pcscfs(&engine, runner.as_ref(), &mut logged_ignored);
                published_pool = if publish_pcscf(
                    runner.as_ref(),
                    idx,
                    &line.pcscf_source_path,
                    &new_pcscf,
                    &current_pool,
                ) {
                    current_pool.clone()
                } else {
                    Vec::new()
                };
                current_pcscf = new_pcscf;
                let _ = runner.run(&["pkill", "-f", &format!("vowifi-ims-agent --line {idx}$")]);
                tracking.negotiated_at = Some(SystemTime::now());
            }
            line_supervisor::SteadyOutcome::Recovered {
                reason,
                ike_sa_loss_cause,
            } => {
                // Recoveries used to leave no record at all, which made a
                // tunnel rebuild indistinguishable from nothing happening --
                // both in a soak and for an operator afterwards. A remedy this
                // heavy (terminate + reinitiate, and a re-registration behind
                // it) has to say so.
                match ike_sa_loss_cause {
                    Some(cause) => {
                        last_loss_cause = Some(cause);
                        println!(
                            "[supervise] line {idx}: recovered from {reason:?} (cause: {cause})"
                        )
                    }
                    None => println!("[supervise] line {idx}: recovered from {reason:?}"),
                }
                if !tracking.pool_exhausted.is_empty() {
                    println!(
                        "{}",
                        pool_exhausted_detail(
                            idx,
                            &tracking.pool_exhausted,
                            tracking.negotiated_at
                        )
                    );
                }
                if let Some(classification) = tracking.dead_peer {
                    println!("{}", dead_peer_detail(idx, &classification));
                }
                send_tunnel(
                    signal,
                    vec![tunnel_recovered_event(
                        reason,
                        &tracking.pool_exhausted,
                        tracking.dead_peer.map(|c| c.verdict),
                        SystemTime::now(),
                    )],
                );
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
                // A recovery that renegotiates the tunnel comes back with a
                // *fresh* P-CSCF pool -- the carrier assigns a different set per
                // IKE_SA, along with a new virtual IP -- so the recorded value is
                // stale once the rebuild lands, and a stale value makes the next
                // tick log `P-CSCF changed` for a rebuild we caused ourselves and
                // kill a healthy registration (Jio, 2026-08-14).
                //
                // The recovery only *starts* the negotiation (`--initiate` is
                // detached), so reading the log right here returns the previous
                // one (#106 triage). Wait for a negotiation logged after `mark`,
                // record its P-CSCF, then restart the agent exactly once. The
                // gate keeps the agent loop from respawning in between.
                //
                // A bare CHILD_SA re-initiate neither restarts the agent nor
                // can change the P-CSCF, so it syncs nothing; a route repair
                // restarts the agent but starts no negotiation, so there is
                // nothing to wait for either.
                if renegotiated {
                    let stop = || *shutting_down.read().unwrap();
                    let fresh = await_fresh_pcscf(
                        runner.as_ref(),
                        super::engines::POST_RECOVERY_PCSCF_WAIT,
                        super::engines::POST_RECOVERY_PCSCF_POLL,
                        &stop,
                        &|| engine.fresh_pcscf_since(runner.as_ref(), mark),
                    );
                    match fresh {
                        Some(p) => {
                            println!(
                                "[supervise] line {idx}: P-CSCF synced from new negotiation: {p}"
                            );
                            current_pool = engine
                                .fresh_pcscf_pool_since(runner.as_ref(), mark)
                                .unwrap_or_default();
                            log_ignored_pcscfs(&engine, runner.as_ref(), &mut logged_ignored);
                            published_pool = if publish_pcscf(
                                runner.as_ref(),
                                idx,
                                &line.pcscf_source_path,
                                &p,
                                &current_pool,
                            ) {
                                current_pool.clone()
                            } else {
                                Vec::new()
                            };
                            current_pcscf = p;
                        }
                        None if !stop() => println!(
                            "[supervise] line {idx}: no new negotiation within {}s; restarting agent with P-CSCF {current_pcscf}",
                            super::engines::POST_RECOVERY_PCSCF_WAIT.as_secs()
                        ),
                        None => {}
                    }
                    let _ =
                        runner.run(&["pkill", "-f", &format!("vowifi-ims-agent --line {idx}$")]);
                    // After the old agent is gone, not when the rebuild was
                    // issued: while it waited for the new negotiation it could
                    // still time a REGISTER out and mark, and that mark is about
                    // the old tunnel, not this one (specs/097 R3).
                    tracking.negotiated_at = Some(SystemTime::now());
                    rebuild_gate.open();
                    if rebuilds.record(std::time::Instant::now()) {
                        println!(
                            "[supervise] line {idx}: tunnel flapping ({} rebuilds within {}s)",
                            line_supervisor::FLAP_REBUILDS,
                            ctx.config.alerts.tunnel_failure_thresholds.unhealthy_sec
                        );
                    }
                    sync_tunnel_signal(
                        &mut signal,
                        &tracking,
                        &rebuilds,
                        last_loss_cause,
                        &report_tunnel,
                    );
                } else if line_supervisor::recovery_restarts_agent(reason) {
                    let _ =
                        runner.run(&["pkill", "-f", &format!("vowifi-ims-agent --line {idx}$")]);
                }
                // A recovery that left the agent alone still changes CHILD_SA
                // usability (`tracking` was updated by the tick).
                sync_tunnel_signal(
                    &mut signal,
                    &tracking,
                    &rebuilds,
                    last_loss_cause,
                    &report_tunnel,
                );
            }
        }
    }
}

/// Reads what the underlay tracker needs to judge the path to the ePDG this
/// tick (specs/100): the route, the ePDG's side of the IKE traffic, and the
/// tunnel's received-packet counter as the previous tick left it.
fn observe_underlay(
    engine: &StrongswanEngine,
    runner: &dyn CommandRunner,
    tracker: &super::underlay::UnderlayTracker,
    mark: super::engines::LogMark,
) -> super::underlay::Observation {
    use line_supervisor::TunnelEngine;
    super::underlay::Observation {
        route_ok: engine.route_ok(runner),
        evidence: engine.ike_evidence_since(runner, tracker.evidence_mark()),
        ike_life_recent: engine
            .ike_evidence_since(runner, tracker.tick_mark())
            .parsed_from_peer,
        rx_packets: engine.tun_rx_packets(),
        pending_life: tracker
            .pending_mark()
            .map(|m| engine.ike_evidence_since(runner, m).parsed_from_peer),
        mark,
    }
}

/// The line logged when a hold begins, naming the cause and its evidence
/// (specs/100 FR-020).
fn hold_started_line(
    idx: u32,
    cause: crate::control::protocol::UplinkHoldCause,
    epdg: Option<&str>,
    unanswered_retransmits: u32,
) -> String {
    use crate::control::protocol::UplinkHoldCause;
    let epdg = epdg.unwrap_or("the ePDG");
    match cause {
        UplinkHoldCause::NoRoute => format!(
            "[supervise] line {idx}: uplink down (no route to {epdg}); holding tunnel recovery until it returns"
        ),
        UplinkHoldCause::NoResponse if unanswered_retransmits > 0 => format!(
            "[supervise] line {idx}: uplink down ({epdg} not responding: {unanswered_retransmits} unanswered retransmit(s), nothing received from it); holding tunnel recovery until it answers"
        ),
        UplinkHoldCause::NoResponse => format!(
            "[supervise] line {idx}: uplink down ({epdg} not responding: its liveness check went unanswered); holding tunnel recovery until it answers"
        ),
    }
}

/// The line logged when a hold ends, with how long it lasted, what ended it
/// and the recoveries it held back (specs/100 FR-020).
fn hold_ended_line(idx: u32, end: &super::underlay::HoldEnd) -> String {
    let skipped = if end.skipped.is_empty() {
        "none".to_string()
    } else {
        end.skipped
            .iter()
            .map(|r| format!("{r:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "[supervise] line {idx}: uplink hold ({}) ended after {}s ({}); held back: {skipped}",
        end.cause.as_str(),
        end.held.as_secs(),
        end.ended_by.as_str()
    )
}

/// Whether this line's VoWiFi agent reports a call in progress, from the
/// count it mirrors beside the P-CSCF files (specs/093 contracts/files.md).
/// A missing or unreadable file is "no call": an unknown call state must
/// never hold a stuck-rekey rebuild back.
fn line_in_call(runner: &dyn CommandRunner, pcscf_source_path: &str) -> bool {
    runner
        .read_file(&crate::ims::pcscf_files::calls_path(pcscf_source_path))
        .ok()
        .and_then(|c| crate::ims::pcscf_files::parse_active_calls(&c))
        .is_some_and(|n| n > 0)
}

/// Rebuilds a line's veth pair if it has gone missing, and says so.
///
/// Runs only on a tick that found the tunnel otherwise healthy, so it never
/// competes with the tunnel-recovery paths. It cannot be folded into
/// `TunnelEngine::steady_state_health`: both engines share the veth, and the
/// pair is not part of either engine's own state. Agent B needs no restart
/// afterwards — its control-channel bind already retries on its own (#96).
fn repair_line_veth(runner: &dyn CommandRunner, idx: u32, netns: &str, line: &LineResolutionEntry) {
    let sip_addr = format!("{}/30", line.veth_peer_addr);
    let ims_addr = format!("{}/30", line.veth_local_addr);
    let veth = epdg_iface::LineVeth {
        netns,
        veth_sip: &line.config.veth_sip_iface,
        veth_ims: &line.config.veth_ims_iface,
        sip_addr: &sip_addr,
        ims_addr: &ims_addr,
    };
    if veth.is_present(runner) {
        return;
    }
    match epdg_iface::ensure_line_veth(runner, &veth) {
        Ok(()) => println!("[supervise] line {idx}: veth missing; rebuilt"),
        Err(e) => eprintln!("[supervise] line {idx}: veth missing and rebuild failed: {e}"),
    }
}

/// The per-line "tunnel rebuild in progress" gate shared by the steady loop and
/// the agent loop (specs/087, research R5). `flag` is what the agent loop waits
/// on; `spawn` makes its check-then-spawn atomic against [`Self::close`].
#[derive(Default)]
struct RebuildGate {
    flag: AtomicBool,
    spawn: Mutex<()>,
}

impl RebuildGate {
    /// Blocks only while an agent spawn is in flight, then closes the gate.
    fn close(&self) {
        let _slot = self.spawn.lock().unwrap();
        self.flag.store(true, Ordering::SeqCst);
    }

    fn open(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

/// Publishes a line's P-CSCF for its agent: the whole pool first, then the
/// single selection (specs/091 R2).
///
/// The order is the contract: an agent that reads the selection must never
/// find a pool older than it. The pool is written atomically because the agent
/// re-reads it before every registration attempt. An empty pool (an engine that
/// logged only the one address) is published as the selection alone, so a
/// pool left over from a previous run is never mistaken for this one.
///
/// Returns whether the pool was written. When it was not, the old pool file is
/// replaced by the selection alone (best effort), so the agent never pairs a
/// new selection with an earlier negotiation's pool, and the caller records
/// nothing as published so the next healthy tick retries (Greptile, PR #130).
#[must_use]
fn publish_pcscf(
    runner: &dyn CommandRunner,
    idx: u32,
    selection_path: &str,
    selection: &str,
    pool: &[String],
) -> bool {
    let pool: Vec<&str> = if pool.is_empty() {
        vec![selection]
    } else {
        pool.iter().map(String::as_str).collect()
    };
    let pool_path = crate::ims::pcscf_files::pool_path(selection_path);
    let rendered = crate::ims::pcscf_files::render_pool(&pool);
    let written = match runner.write_file_atomic(&pool_path, &rendered) {
        Ok(()) => {
            println!("[supervise] line {idx}: P-CSCF pool: {}", pool.join(", "));
            true
        }
        Err(e) => {
            eprintln!("[supervise] line {idx}: could not publish the P-CSCF pool: {e}");
            let alone = crate::ims::pcscf_files::render_pool(&[selection]);
            let _ = runner.write_file_atomic(&pool_path, &alone);
            false
        }
    };
    let _ = runner.write_file(Path::new(selection_path), selection);
    written
}

/// Says once per episode that the selection stopped answering but another pool
/// member still does, which is why no rebuild follows (specs/091 R9).
fn report_pcscf_sibling(
    idx: u32,
    selection: &str,
    before: Option<std::net::IpAddr>,
    tracking: &line_supervisor::SteadyTracking,
) {
    if let (None, Some(sibling)) = (before, tracking.pcscf_via_sibling) {
        println!(
            "[supervise] line {idx}: P-CSCF {selection} unreachable; pool member {sibling} answers"
        );
    }
}

/// The pool to republish after a healthy steady-state tick, if the advertised
/// *set* moved while the selection stayed put (specs/091 R2, US2 AS2/AS3).
///
/// A pure reorder returns `None`: the carrier reorders one pool freely, and
/// rewriting the file for it would be noise. An empty reading (the log rotated,
/// nothing parsed) is not a change either.
fn steady_pool_update(published: &[String], latest: Vec<String>) -> Option<Vec<String>> {
    let same_set = latest.len() == published.len() && latest.iter().all(|a| published.contains(a));
    (!latest.is_empty() && !same_set).then_some(latest)
}

/// Republishes only the pool after a set change on a healthy line: no agent
/// restart, and the selection file is not touched (FR-004).
///
/// Two lists, because they answer different questions (Greptile, PR #130):
/// `current_pool` is what the tunnel advertises, and feeds the reachability
/// check whether or not the file could be written; `published_pool` is what
/// the agent's file holds, and decides when to write it again.
fn republish_pool_after_set_change(
    runner: &dyn CommandRunner,
    idx: u32,
    selection_path: &str,
    current_pool: &mut Vec<String>,
    published_pool: &mut Vec<String>,
    latest: Vec<String>,
) {
    if !latest.is_empty() {
        current_pool.clone_from(&latest);
    }
    if let Some(pool) = steady_pool_update(published_pool, latest) {
        // Recorded as published only once it is: a failed write is retried on
        // the next healthy tick rather than forgotten.
        match runner.write_file_atomic(
            &crate::ims::pcscf_files::pool_path(selection_path),
            &crate::ims::pcscf_files::render_pool(&pool),
        ) {
            Ok(()) => {
                println!(
                    "[supervise] line {idx}: P-CSCF pool changed (set): {}; not restarting",
                    pool.join(", ")
                );
                *published_pool = pool;
            }
            Err(e) => {
                eprintln!("[supervise] line {idx}: could not publish the P-CSCF pool: {e}")
            }
        }
    }
}

/// Logs each P-CSCF the latest negotiation advertised that this line's
/// `tunnel_address_family` excludes (specs/092 FR-013). Safe to call every
/// tick: see [`ignored_pcscf_lines`]. Silent for a `dual` line.
fn log_ignored_pcscfs(
    engine: &StrongswanEngine,
    runner: &dyn CommandRunner,
    last_logged: &mut Vec<String>,
) {
    for line in ignored_pcscf_lines(
        engine.idx,
        engine.address_family,
        engine.ignored_pcscfs(runner),
        last_logged,
    ) {
        println!("{line}");
    }
}

/// The "ignoring P-CSCF" lines for `ignored`, or none when it equals the set
/// `last_logged` already reported. Tracked apart from the published pool, so
/// a negotiation whose admitted pool is empty or unchanged is still logged
/// once.
fn ignored_pcscf_lines(
    idx: u32,
    family: TunnelAddressFamily,
    ignored: Vec<String>,
    last_logged: &mut Vec<String>,
) -> Vec<String> {
    if ignored == *last_logged {
        return Vec::new();
    }
    let lines = ignored
        .iter()
        .map(|addr| {
            format!(
                "[supervise] line {idx}: ignoring P-CSCF {addr} (tunnel_address_family = {family})"
            )
        })
        .collect();
    *last_logged = ignored;
    lines
}

/// The hint for a `dual` line whose CHILD_SA rekey the ePDG rejected with
/// `INVALID_SELECTORS` (specs/092 FR-009a, #107), at most once per IKE_SA
/// session: `session` is the rejected session's tag, and `last_warned`
/// remembers the one already warned about. The bridge never changes the
/// setting itself.
fn inval_sel_warning(
    idx: u32,
    family: TunnelAddressFamily,
    session: Option<String>,
    last_warned: &mut Option<String>,
) -> Option<String> {
    if family != TunnelAddressFamily::Dual {
        return None;
    }
    let session = session?;
    if last_warned.as_ref() == Some(&session) {
        return None;
    }
    *last_warned = Some(session);
    Some(format!(
        "[supervise] line {idx}: ePDG rejected the CHILD_SA rekey's traffic selectors \
         (INVALID_SELECTORS); if this repeats, set tunnel_address_family = \"ipv4\" for \
         this line (see config.toml.example)"
    ))
}

/// Waits up to `wait` for a negotiation newer than a log marker to report its
/// P-CSCF, polling `fresh` every `poll` (specs/087, research R4). Reads first,
/// then sleeps, so a negotiation that is already logged costs no delay. Gives up
/// early (`None`) when `stop()` says the container is shutting down.
fn await_fresh_pcscf(
    runner: &dyn CommandRunner,
    wait: Duration,
    poll: Duration,
    stop: &dyn Fn() -> bool,
    fresh: &dyn Fn() -> Option<String>,
) -> Option<String> {
    let polls = (wait.as_millis() / poll.as_millis().max(1)) as u32;
    for i in 0..=polls {
        if stop() {
            return None;
        }
        if let Some(p) = fresh() {
            return Some(p);
        }
        if i < polls {
            runner.sleep(poll);
        }
    }
    None
}

/// Holds the agent loop's next spawn while a tunnel rebuild is in flight
/// (specs/087, research R5): without it the old agent can exit on its own
/// during the P-CSCF wait and be respawned with the stale address, and the
/// post-sync `pkill` would then restart it a second time. Bounded by `bound`,
/// so a bug elsewhere can never wedge the line, and abandoned on `stop()`.
fn wait_for_rebuild_gate(
    flag: &AtomicBool,
    runner: &dyn CommandRunner,
    bound: Duration,
    stop: &dyn Fn() -> bool,
) {
    let poll = Duration::from_secs(1);
    let polls = bound.as_secs();
    for _ in 0..polls {
        if !flag.load(Ordering::SeqCst) || stop() {
            return;
        }
        runner.sleep(poll);
    }
}

/// What the supervisor reports about one line's tunnel (specs/086, 087).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TunnelSignal {
    child_sa_up: bool,
    flapping: bool,
    loss_cause: Option<crate::control::protocol::IkeSaLossCause>,
}

/// The event that counts one steady-state recovery of a line (specs/096),
/// carrying, when the reason was an exhausted pool, the P-CSCFs and when the
/// rebuild was issued (`now`), for the daemon's alert episode (specs/097), and,
/// for a dead-peer loss, the supervisor's verdict on it (specs/098).
fn tunnel_recovered_event(
    reason: line_supervisor::DegradeReason,
    exhausted: &[line_supervisor::ExhaustedMember],
    dead_peer: Option<line_supervisor::DeadPeerVerdict>,
    now: SystemTime,
) -> crate::control::protocol::ObservedEvent {
    crate::control::protocol::ObservedEvent::TunnelRecovered {
        reason: reason.into(),
        pcscfs: exhausted.iter().map(|m| m.addr.clone()).collect(),
        rebuilt_at_unix: (!exhausted.is_empty()).then(|| {
            now.duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
        }),
        dead_peer,
    }
}

/// What the tunnel interface received after the snapshot, for the log.
fn rx_since(packets: Option<u64>) -> String {
    packets.map_or_else(
        || "tunnel counter unread".to_string(),
        |n| format!("{n} received on the tunnel since"),
    )
}

/// The verdict on a dead-peer loss and the inbound ESP it rests on, for the
/// log (specs/098 contracts/dead-peer.md).
fn dead_peer_detail(idx: u32, c: &line_supervisor::DeadPeerClassification) -> String {
    let verdict = c.verdict.as_str();
    let window = line_supervisor::DPD_GIVE_UP_WINDOW.as_secs();
    match (c.snapshot_age, c.last_in_before_loss, c.packets) {
        (None, _, _) => {
            format!(
                "[supervise] line {idx}: dead peer verdict: {verdict} (no inbound ESP snapshot)"
            )
        }
        (Some(snap), None, packets) => format!(
            "[supervise] line {idx}: dead peer verdict: {verdict} (no inbound ESP on any listed \
             CHILD_SA, {} packets, snapshot {}s old, {}; charon retransmits for {window}s)",
            packets.unwrap_or(0),
            snap.as_secs(),
            rx_since(c.rx_since_snapshot)
        ),
        (Some(snap), Some(before), packets) => format!(
            "[supervise] line {idx}: dead peer verdict: {verdict} (inbound ESP {}s before the \
             loss, {} packets, snapshot {}s old, {}; charon retransmits for {window}s)",
            before.as_secs(),
            packets.unwrap_or(0),
            snap.as_secs(),
            rx_since(c.rx_since_snapshot)
        ),
    }
}

/// What a `PcscfPoolExhausted` rebuild was decided on, for the log: each
/// member's mark and the negotiation it was compared against (FR-012).
fn pool_exhausted_detail(
    idx: u32,
    members: &[line_supervisor::ExhaustedMember],
    negotiated_at: Option<SystemTime>,
) -> String {
    let ts = |secs: u64| {
        i64::try_from(secs)
            .ok()
            .and_then(|s| chrono::DateTime::<chrono::Utc>::from_timestamp(s, 0))
            .map_or_else(|| format!("unix:{secs}"), |d| d.to_rfc3339())
    };
    let marks = members
        .iter()
        .map(|m| {
            format!(
                "{} ({}, marked {}, until {})",
                m.addr,
                m.cause,
                ts(m.marked_at_unix),
                ts(m.until_unix)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let since = negotiated_at
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or_else(|| "unknown".to_string(), |d| ts(d.as_secs()));
    format!("[supervise] line {idx}: the IMS agent marked every P-CSCF unavailable since the negotiation at {since}: {marks}")
}

/// The line's IMS agent's P-CSCF pool state (`/tmp/pcscf-N.state`), read
/// through the runner like [`line_in_call`]. `None` when it is missing or
/// unusable, which the exhaustion check treats as "not exhausted".
fn line_pool_state(
    runner: &dyn CommandRunner,
    pcscf_source_path: &str,
) -> Option<crate::ims::pcscf_files::PoolState> {
    runner
        .read_file(&crate::ims::pcscf_files::state_path(pcscf_source_path))
        .ok()
        .and_then(|c| crate::ims::pcscf_files::parse_pool_state(&c))
}

/// Reports the tunnel signal if, and only if, it differs from the last one
/// sent; the reporter heartbeats the last state, so only changes need sending.
fn sync_tunnel_signal(
    signal: &mut TunnelSignal,
    tracking: &line_supervisor::SteadyTracking,
    rebuilds: &line_supervisor::RebuildWindow,
    loss_cause: Option<crate::control::protocol::IkeSaLossCause>,
    report: &dyn Fn(TunnelSignal),
) {
    let now = TunnelSignal {
        child_sa_up: tracking.child_sa_usable,
        flapping: rebuilds.is_flapping(),
        loss_cause,
    };
    if now != *signal {
        *signal = now;
        report(now);
    }
}

/// Whether to clear a line namespace's XFRM state before each agent spawn.
///
/// Strongswan engine only: there the ePDG CHILD_SA lives in the host namespace
/// (keyed by `if_id`), so the line namespace holds nothing but the IMS agent's
/// Gm IPsec state. The SWu dialer has not been verified to keep its own tunnel
/// out of the namespace's XFRM tables, so it is left alone (research R6).
fn should_clear_gm_state(tunnel_engine: &str) -> bool {
    tunnel_engine == "strongswan"
}

/// Flushes the Gm IPsec state a previous `vowifi-ims-agent` left in `netns`,
/// returning how many states and policies were there (specs/087, FR-012).
///
/// The agent has no SIGTERM handler, so a `pkill`ed one never deletes its SAs
/// and each restart stacks another registration's worth. This runs only when no
/// agent is alive for the line, so nothing live is cleared. Everything runs
/// *inside* the namespace; the host's tunnel SAs are never touched. A failed
/// dump still flushes, since the state is unknown and flushing is harmless.
fn clear_line_gm_state(runner: &dyn CommandRunner, idx: u32, netns: &str) -> (usize, usize) {
    let count = |what: &str| {
        runner
            .run_in_netns(netns, &["ip", "xfrm", what])
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter(|l| l.starts_with("src "))
                    .count()
            })
    };
    let states = count("state");
    let policies = count("policy");
    if states == Some(0) && policies == Some(0) {
        return (0, 0);
    }
    let flush = |what: &str| {
        runner
            .run_in_netns(netns, &["ip", "xfrm", what, "flush"])
            .is_ok_and(|o| o.status.success())
    };
    let states_ok = flush("state");
    let policies_ok = flush("policy");
    for (what, ok) in [("state", states_ok), ("policy", policies_ok)] {
        if !ok {
            eprintln!(
                "[supervise] line {idx}: `ip xfrm {what} flush` failed in {netns}; stale Gm state may remain"
            );
        }
    }
    // Only what a successful flush removed counts as cleared.
    let s = if states_ok { states.unwrap_or(0) } else { 0 };
    let p = if policies_ok {
        policies.unwrap_or(0)
    } else {
        0
    };
    if s + p > 0 {
        println!(
            "[supervise] line {idx}: cleared {s} stale Gm SAs / {p} policies before agent start"
        );
    }
    (s, p)
}

/// Veth pair + this line's vowifi-ims-agent (with per-incident USIM
/// auto-recovery) + the optional idle-tunnel keepalive (off by default,
/// `[vowifi].idle_keepalive`, #120) — 1:1 port of `start_line_tail` and its
/// keepalive sibling. Both supervision loops run on their own background
/// threads and this function returns immediately, matching the bash original
/// backgrounding both with `&`.
fn start_line_tail(
    ctx: &LineStartup,
    idx: u32,
    netns: &str,
    line: &LineResolutionEntry,
    usim_holder: &Arc<Mutex<Option<Arc<ChildHandle>>>>,
    initial_pcscf: String,
    rebuild_gate: Arc<RebuildGate>,
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
    let veth = epdg_iface::LineVeth {
        netns,
        veth_sip: &veth_sip,
        veth_ims: &veth_ims,
        sip_addr: &veth_sip_addr,
        ims_addr: &veth_ims_addr,
    };
    // Non-fatal here: the steady-state loop re-checks the pair every tick and
    // rebuilds it (`repair_line_veth`), so a failure is reported, not final.
    if let Err(e) = epdg_iface::ensure_line_veth(runner.as_ref(), &veth) {
        eprintln!("[supervise] line {idx}: veth setup failed ({e}); will retry each tick");
    }

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
        let rebuild_gate = Arc::clone(&rebuild_gate);
        let clear_gm_state = should_clear_gm_state(&ctx.config.vowifi.tunnel_engine);
        let alert_ctx = alert_ctx.cloned();
        let line_id = line.card_id.clone();
        // specs/034-alert-identity: resolve this line's number before the
        // 'static thread closure, so it captures the owned `Option` rather than
        // borrowing `line` (which does not outlive the spawn).
        let line_msisdn = line.configured_msisdn();
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
            // Whether this line has already been through at least one
            // exit/restart. The recovery log below fires only *after* a
            // restart, never on the first clean startup — its whole purpose is
            // to mark the end of a visible "exited; restarting in 5s" loop
            // (e.g. the ~2.5min one right after a container recreate, while the
            // previous run's netns still holds the tunnel's XFRM if_id).
            let mut had_restart = false;
            loop {
                // Held while a tunnel rebuild is syncing the P-CSCF, so the
                // agent is not respawned against the stale address in between
                // (specs/087, R5). Before taking the shutdown guard, which it
                // must not hold while waiting.
                wait_for_rebuild_gate(
                    &rebuild_gate.flag,
                    runner.as_ref(),
                    super::engines::POST_RECOVERY_PCSCF_WAIT + Duration::from_secs(5),
                    &|| *shutting_down.read().unwrap(),
                );
                let guard = shutting_down.read().unwrap();
                if *guard {
                    return;
                }
                // Check-and-spawn is atomic against `RebuildGate::close`: the
                // slot is held until the spawn is done, so a rebuild that
                // closes the gate either runs first (we see it and wait
                // again) or after the spawn (and its `pkill` then restarts
                // this very agent). Taken after the shutdown guard, the same
                // order `close` is called in, so the two cannot deadlock.
                let spawn_slot = rebuild_gate.spawn.lock().unwrap();
                if rebuild_gate.flag.load(Ordering::SeqCst) {
                    continue;
                }
                if clear_gm_state {
                    clear_line_gm_state(runner.as_ref(), idx, &netns);
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
                drop(spawn_slot);
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
                // Log the moment the agent registers again after a restart
                // loop. The agent's own "registered, listening for inbound
                // calls" line is captured to the per-line file (`agent_log`),
                // not the supervisor's stdout, so without this the recovery is
                // invisible in the supervise log — only the "exited; restarting"
                // side of the loop ever shows there. Gated on `had_restart` (so
                // a clean first startup stays quiet) and latched by
                // `recovery_logged` (so it prints once per spawn, not every
                // poll); the file read stops once the line is seen.
                let mut recovery_logged = false;
                while runner.is_alive(&handle) {
                    if had_restart
                        && !recovery_logged
                        && runner
                            .read_file(&agent_log)
                            .unwrap_or_default()
                            .contains("registered, listening for inbound calls")
                    {
                        println!(
                            "[supervise] line {idx}: vowifi-ims-agent registered after restart loop"
                        );
                        recovery_logged = true;
                    }
                    runner.sleep(Duration::from_secs(1));
                }
                had_restart = true;

                let log_content = runner.read_file(&agent_log).unwrap_or_default();
                // A stall is checked first: a run that stalled may *also* have
                // an older `AT+CSIM failed` earlier in its log, and the stall is
                // the more specific and more recent explanation for this exit.
                let outcome = if sim_recovery::has_at_stall(&log_content) {
                    sim_recovery::AgentExitOutcome::AtStall
                } else if sim_recovery::has_csim_failure(&log_content) {
                    sim_recovery::AgentExitOutcome::CsimFailure
                } else {
                    sim_recovery::AgentExitOutcome::Other
                };
                let action = csim_fails.observe(outcome);
                let restart_delay = csim_fails.restart_delay(action);
                let reason = match outcome {
                    sim_recovery::AgentExitOutcome::AtStall => " (stalled on the modem)",
                    sim_recovery::AgentExitOutcome::CsimFailure => " (AT+CSIM failure)",
                    sim_recovery::AgentExitOutcome::Other => "",
                };
                println!(
                    "[supervise] line {idx}: vowifi-ims-agent exited{reason}; restarting in {}s",
                    restart_delay.as_secs()
                );
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
                        retry_in_secs = sim_recovery::EXHAUSTED_RESTART_DELAY.as_secs(),
                        "SIM recovery exhausted (MAX_SIM_RESETS reached this incident); \
                         backing off to a slow retry so the line still recovers by itself \
                         if the hardware does, without restarting in a tight loop"
                    );
                }
                if let Some(kind) = sim_alert_transition(action, &mut given_up_alerted) {
                    if let Some(ctx) = &alert_ctx {
                        let description = match kind {
                            alerts::CriticalEventKind::Failure => {
                                "VoWiFi line's modem recovery exhausted (max SIM resets reached \
                                 this incident); the line is now on a slow retry and will \
                                 recover by itself only if the hardware does"
                            }
                            alerts::CriticalEventKind::Recovered => "VoWiFi line's modem recovered",
                        };
                        ctx.fire(alerts::CriticalEvent {
                            category: alerts::AlertCategory::ModuleLifecycle,
                            unit_id: Some(line_id.clone()),
                            description: description.to_string(),
                            phone_number: line_msisdn.clone(),
                            at: chrono::Utc::now(),
                            kind,
                        });
                    }
                }

                runner.sleep(restart_delay);
            }
        });
    }

    // Idle-tunnel keepalive, opt-in since #120 (`[vowifi].idle_keepalive`).
    // Re-reads the P-CSCF source file every cycle so it keeps pinging the
    // right address after the steady-state loop refreshes it.
    let _ = initial_pcscf; // each cycle re-reads the file anyway
    if let Some(interval) = idle_keepalive_interval(&ctx.config.vowifi) {
        let runner = Arc::clone(runner);
        let netns = netns.to_string();
        let pcscf_path = line.pcscf_source_path.clone();
        std::thread::spawn(move || loop {
            idle_keepalive_once(runner.as_ref(), &netns, &pcscf_path);
            runner.sleep(interval);
        });
    }
}

/// How often each line's idle-tunnel keepalive runs, or `None` when it is
/// off — the default since #120. The line supervisor already probes the
/// P-CSCF every 30s, and a second stream of connects on an idle tunnel keeps
/// strongSwan's own DPD and NAT-T keepalives from ever running. Kept as an
/// escape hatch for a carrier that drops an idle bearer (#106).
fn idle_keepalive_interval(v: &crate::config::VowifiConfig) -> Option<Duration> {
    v.idle_keepalive
        .then(|| Duration::from_secs(v.keepalive_interval_sec))
}

/// The startup line saying whether the idle-tunnel keepalive is on
/// (specs/094 contracts/logs.md).
fn idle_keepalive_log_line(v: &crate::config::VowifiConfig) -> String {
    match idle_keepalive_interval(v) {
        None => "idle-tunnel keepalive off ([vowifi].idle_keepalive = false); \
                 P-CSCF reachability is probed by the line supervisor only"
            .to_string(),
        Some(interval) => format!(
            "idle-tunnel keepalive on: TCP connect to each line's P-CSCF :{} every {}s \
             ([vowifi].idle_keepalive = true)",
            line_supervisor::PCSCF_SIP_PORT,
            interval.as_secs()
        ),
    }
}

/// The startup line naming the DPD delay every strongSwan line renders, or
/// saying it is ignored on swu (specs/098 FR-006).
fn dpd_log_line(v: &crate::config::VowifiConfig) -> String {
    match (v.tunnel_engine.as_str(), v.dpd_delay_sec) {
        ("strongswan", 0) => "strongSwan DPD off ([vowifi].dpd_delay_sec = 0)".to_string(),
        ("strongswan", secs) => {
            format!("strongSwan DPD: dpd_delay = {secs}s ([vowifi].dpd_delay_sec)")
        }
        _ => "[vowifi].dpd_delay_sec ignored: the swu engine has no DPD setting".to_string(),
    }
}

/// One idle-tunnel keepalive cycle (TCP connect, not ICMP — operators filter
/// ICMP over the tunnel): if the line has a P-CSCF, connect to its SIP port
/// from inside the line's namespace and discard the result.
fn idle_keepalive_once(runner: &dyn CommandRunner, netns: &str, pcscf_path: &str) {
    let pcscf_now = runner.read_file(Path::new(pcscf_path)).unwrap_or_default();
    let pcscf_now = pcscf_now.trim();
    if pcscf_now.is_empty() {
        return;
    }
    // Uses the runner's own probe rather than shelling out to
    // `timeout 3 bash -c '>/dev/tcp/...'` (specs/039-at-stall-watchdog,
    // FR-028). That shell form left the inner `timeout` process orphaned on
    // every cycle, reparented to this process as PID 1, and never reaped: 462
    // zombies had accumulated in 3.8 hours, which at ~120 an hour eventually
    // exhausts the process table and takes out something entirely unrelated.
    // This has the same 3s connect timeout, waits for its child properly, and
    // does not interpolate a file's contents into a shell command. The result
    // is deliberately discarded *here*: this loop's job is to put traffic on
    // the tunnel so the bearer's NAT mapping stays alive, not to decide
    // anything.
    //
    // It used to be the only thing in the whole system that probed
    // reachability, and dropping the answer is what let the 2026-08-17 outage
    // run for 8 hours: this probe failed roughly 960 consecutive times and
    // told nobody. The decision now lives in
    // `line_supervisor::tick_steady_state` (`DegradeReason::PcscfUnreachable`),
    // which has the engine handle needed to actually repair the tunnel.
    let _ = runner.tcp_connect_ok_in_netns(netns, pcscf_now, line_supervisor::PCSCF_SIP_PORT);
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
    let mut current_pool = engine.latest_pcscf_pool(runner.as_ref());
    let mut published_pool = if publish_pcscf(
        runner.as_ref(),
        idx,
        &line.pcscf_source_path,
        &pcscf,
        &current_pool,
    ) {
        current_pool.clone()
    } else {
        Vec::new()
    };
    {
        let mut st = started.lock().unwrap();
        st.started_netns.push(netns.clone());
        // `strongswan: None` — this engine has no in-place terminate
        // concept and no XFRM `if_id` (`SwuEngine::terminate` is a
        // deliberate no-op); only its veth is a device this run must
        // explicitly release at stop.
        st.vowifi_lines.push(shutdown::StartedVowifiLine {
            index: idx,
            strongswan: None,
            netns: netns.clone(),
            veth_host: line.config.veth_sip_iface.clone(),
        });
    }

    let usim_holder: Arc<Mutex<Option<Arc<ChildHandle>>>> = Arc::new(Mutex::new(None));
    let negotiated_at = SystemTime::now();
    start_line_tail(
        ctx,
        idx,
        &netns,
        line,
        &usim_holder,
        pcscf.clone(),
        Arc::new(RebuildGate::default()),
    );

    let mut current_pcscf = pcscf;
    let mut tracking = line_supervisor::SteadyTracking {
        negotiated_at: Some(negotiated_at),
        ..Default::default()
    };
    loop {
        // Named, and shared with the reachability-window derivation: this loop
        // polls six times faster than the strongswan one, so any threshold
        // expressed in ticks means two different amounts of real time.
        runner.sleep(line_supervisor::SWU_STEADY_STATE_POLL_INTERVAL);
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
        let via_sibling_before = tracking.pcscf_via_sibling;
        let outcome = line_supervisor::tick_steady_state_with_pool(
            &engine,
            runner.as_ref(),
            &current_pcscf,
            &current_pool,
            &mut tracking,
            line_supervisor::SWU_STEADY_STATE_POLL_INTERVAL,
            // The swu engine reports no CHILD_SA lifetimes, so a stuck-rekey
            // rebuild never reaches this; it is here for the pool-exhaustion
            // rebuild, which waits for a call to end (specs/097 R6).
            &|| line_in_call(runner.as_ref(), &line.pcscf_source_path),
            &|| line_pool_state(runner.as_ref(), &line.pcscf_source_path),
        );
        report_pcscf_sibling(idx, &current_pcscf, via_sibling_before, &tracking);
        match outcome {
            line_supervisor::SteadyOutcome::StillUp => {
                // Under the shutdown guard: a repair racing teardown would
                // recreate the pair the plan just deleted.
                repair_line_veth(runner.as_ref(), idx, &netns, line);
                drop(guard);
                republish_pool_after_set_change(
                    runner.as_ref(),
                    idx,
                    &line.pcscf_source_path,
                    &mut current_pool,
                    &mut published_pool,
                    engine.latest_pcscf_pool(runner.as_ref()),
                );
            }
            line_supervisor::SteadyOutcome::PcscfChanged { new_pcscf } => {
                drop(guard);
                // This path has never rewritten the selection; the new
                // negotiation's pool must still reach the agent, so it is
                // published (pool only) and recorded only once written.
                republish_pool_after_set_change(
                    runner.as_ref(),
                    idx,
                    &line.pcscf_source_path,
                    &mut current_pool,
                    &mut published_pool,
                    engine.latest_pcscf_pool(runner.as_ref()),
                );
                current_pcscf = new_pcscf;
                tracking.negotiated_at = Some(SystemTime::now());
            }
            // Only the strongSwan engine has a half-open IKE_SA to wait on.
            line_supervisor::SteadyOutcome::WaitingForNegotiation
            | line_supervisor::SteadyOutcome::Deferred { .. }
            | line_supervisor::SteadyOutcome::Held { .. } => drop(guard),
            line_supervisor::SteadyOutcome::Recovered { reason, .. } => {
                println!("[supervise] line {idx}: recovered from {reason:?}");
                if !tracking.pool_exhausted.is_empty() {
                    println!(
                        "{}",
                        pool_exhausted_detail(
                            idx,
                            &tracking.pool_exhausted,
                            tracking.negotiated_at
                        )
                    );
                }
                if let Some(h) = engine.dialer_handle.borrow().clone() {
                    started.lock().unwrap().vowifi_child_handles.push(h);
                }
                drop(guard);
                let _ = runner.run(&["pkill", "-f", &format!("vowifi-ims-agent --line {idx}$")]);
                if line_supervisor::recovery_renegotiates(reason) {
                    tracking.negotiated_at = Some(SystemTime::now());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- specs/094: the idle-tunnel keepalive is opt-in ----

    #[test]
    fn the_idle_keepalive_is_off_by_default() {
        let v = crate::config::VowifiConfig::default();
        assert_eq!(idle_keepalive_interval(&v), None);
    }

    #[test]
    fn the_idle_keepalive_runs_at_the_configured_interval_when_on() {
        let v = crate::config::VowifiConfig {
            idle_keepalive: true,
            keepalive_interval_sec: 45,
            ..Default::default()
        };
        assert_eq!(idle_keepalive_interval(&v), Some(Duration::from_secs(45)));
    }

    #[test]
    fn one_keepalive_cycle_makes_one_connect_inside_the_namespace() {
        for answers in [true, false] {
            let runner = crate::supervise::runner::MockCommandRunner::new();
            runner.set_file("/tmp/pcscf-0", "192.0.2.10\n");
            runner.set_tcp_connect_ok_in_netns("ims0", "192.0.2.10", 5060, answers);
            idle_keepalive_once(&runner, "ims0", "/tmp/pcscf-0");
            assert_eq!(
                runner.tcp_connect_attempts(),
                vec![(Some("ims0".to_string()), "192.0.2.10".to_string(), 5060)]
            );
        }
    }

    #[test]
    fn a_keepalive_cycle_without_a_pcscf_connects_nowhere() {
        let runner = crate::supervise::runner::MockCommandRunner::new();
        idle_keepalive_once(&runner, "ims0", "/tmp/pcscf-0");
        runner.set_file("/tmp/pcscf-0", "  \n");
        idle_keepalive_once(&runner, "ims0", "/tmp/pcscf-0");
        assert!(runner.tcp_connect_attempts().is_empty());
    }

    #[test]
    fn the_startup_log_line_says_whether_the_keepalive_is_on() {
        let off = crate::config::VowifiConfig::default();
        assert_eq!(
            idle_keepalive_log_line(&off),
            "idle-tunnel keepalive off ([vowifi].idle_keepalive = false); \
             P-CSCF reachability is probed by the line supervisor only"
        );
        let on = crate::config::VowifiConfig {
            idle_keepalive: true,
            keepalive_interval_sec: 30,
            ..Default::default()
        };
        assert_eq!(
            idle_keepalive_log_line(&on),
            "idle-tunnel keepalive on: TCP connect to each line's P-CSCF :5060 every 30s \
             ([vowifi].idle_keepalive = true)"
        );
    }

    #[test]
    fn the_startup_dpd_line_names_the_delay_or_says_it_is_ignored() {
        let default = crate::config::VowifiConfig::default();
        assert_eq!(
            dpd_log_line(&default),
            "strongSwan DPD: dpd_delay = 120s ([vowifi].dpd_delay_sec)"
        );
        let off = crate::config::VowifiConfig {
            dpd_delay_sec: 0,
            ..Default::default()
        };
        assert_eq!(
            dpd_log_line(&off),
            "strongSwan DPD off ([vowifi].dpd_delay_sec = 0)"
        );
        let swu = crate::config::VowifiConfig {
            tunnel_engine: "swu".to_string(),
            ..Default::default()
        };
        assert_eq!(
            dpd_log_line(&swu),
            "[vowifi].dpd_delay_sec ignored: the swu engine has no DPD setting"
        );
    }

    #[test]
    fn ignored_pcscf_lines_log_each_set_once_even_with_nothing_admitted() {
        use TunnelAddressFamily::Ipv6;
        let v = |a: &[&str]| a.iter().map(ToString::to_string).collect::<Vec<_>>();
        let mut last = Vec::new();
        // An ipv6 line offered only IPv4 P-CSCFs: nothing to publish, but
        // the operator still learns why.
        assert_eq!(
            ignored_pcscf_lines(1, Ipv6, v(&["10.0.0.10", "10.0.1.10"]), &mut last),
            v(&[
                "[supervise] line 1: ignoring P-CSCF 10.0.0.10 (tunnel_address_family = ipv6)",
                "[supervise] line 1: ignoring P-CSCF 10.0.1.10 (tunnel_address_family = ipv6)",
            ])
        );
        // The same set on the next tick is not repeated.
        assert!(ignored_pcscf_lines(1, Ipv6, v(&["10.0.0.10", "10.0.1.10"]), &mut last).is_empty());
        // A negotiation that changes only excluded addresses is logged.
        assert_eq!(
            ignored_pcscf_lines(1, Ipv6, v(&["10.0.0.10"]), &mut last),
            v(&["[supervise] line 1: ignoring P-CSCF 10.0.0.10 (tunnel_address_family = ipv6)"])
        );
        // Nothing ignored logs nothing, and a later return is logged again.
        assert!(ignored_pcscf_lines(1, Ipv6, Vec::new(), &mut last).is_empty());
        assert_eq!(
            ignored_pcscf_lines(1, Ipv6, v(&["10.0.0.10"]), &mut last).len(),
            1
        );
    }

    #[test]
    fn inval_sel_warning_fires_once_per_session_on_dual_lines_only() {
        use TunnelAddressFamily::{Dual, Ipv4};
        let s = |t: &str| Some(t.to_string());
        let mut last = None;
        let first = inval_sel_warning(0, Dual, s("ims0[3]"), &mut last).expect("warns");
        assert_eq!(
            first,
            "[supervise] line 0: ePDG rejected the CHILD_SA rekey's traffic selectors \
             (INVALID_SELECTORS); if this repeats, set tunnel_address_family = \"ipv4\" \
             for this line (see config.toml.example)"
        );
        assert_eq!(inval_sel_warning(0, Dual, s("ims0[3]"), &mut last), None);
        assert!(inval_sel_warning(0, Dual, s("ims0[4]"), &mut last).is_some());
        assert_eq!(inval_sel_warning(0, Dual, None, &mut last), None);

        let mut last = None;
        assert_eq!(inval_sel_warning(0, Ipv4, s("ims0[3]"), &mut last), None);
        assert_eq!(last, None);
    }

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

    /// The exact review-flagged scenario: a pending id's failure reason
    /// changes between ticks (`not_found` on the previous pass, some other
    /// rejection now — live testing found `sim_unreadable` reachable this
    /// way) without it ever actually becoming a resolved line. Must still
    /// count as missing.
    #[test]
    fn still_missing_from_resolution_stays_missing_when_the_failure_reason_changes() {
        let pending = std::collections::HashMap::from([(
            "ec20-0".to_string(),
            std::time::Instant::now() + Duration::from_secs(60),
        )]);
        let resolution = crate::vowifi::discovery::LineResolution {
            circuit_switched_excluded_ports: vec![],
            lines: vec![],
            failed: vec![
                crate::vowifi::discovery::FailedLine::new("ec20-0", "sim_unreadable: 13")
                    .configured(true),
            ],
        };
        let still_missing = still_missing_from_resolution(&pending, &resolution);
        assert!(
            still_missing.contains("ec20-0"),
            "a failure-reason change alone must never be treated as resolution"
        );
    }

    /// Builds a resolved line that reports itself as having come from the
    /// configured override named `identifier` — what `resolve_one_line`
    /// records once that override actually matches a modem.
    fn configured_modem_line(index: u32, port: &str, identifier: &str) -> LineResolutionEntry {
        let mut line = modem_line(index, port);
        line.configured_identifier = Some(identifier.to_string());
        line
    }

    /// First P1 review finding ("Recovery compares incompatible
    /// identities"): a `modem_port` override's pending key is the port
    /// string, never the USB-derived `card_id`, so a resolved line must be
    /// recognised by the identifier it records — not by any resemblance
    /// between the pending key and `card_id`.
    #[test]
    fn still_missing_from_resolution_clears_once_a_modem_port_override_resolves() {
        let pending = std::collections::HashMap::from([(
            "/dev/ttyUSB3".to_string(),
            std::time::Instant::now() + Duration::from_secs(60),
        )]);
        let resolution = crate::vowifi::discovery::LineResolution {
            circuit_switched_excluded_ports: vec![],
            lines: vec![configured_modem_line(0, "/dev/ttyUSB3", "/dev/ttyUSB3")],
            failed: vec![],
        };
        assert_ne!(
            resolution.lines[0].card_id, "/dev/ttyUSB3",
            "fixture sanity: card_id must genuinely differ from the pending key"
        );
        assert!(still_missing_from_resolution(&pending, &resolution).is_empty());
    }

    /// The `modem_serial` mirror case: the pending key is the raw USB
    /// serial the operator pinned, and the resolved line records that same
    /// serial — `card_id` never enters the comparison.
    #[test]
    fn still_missing_from_resolution_clears_once_a_modem_serial_override_resolves() {
        let serial = "0123456789ABCDEF";
        let pending = std::collections::HashMap::from([(
            serial.to_string(),
            std::time::Instant::now() + Duration::from_secs(60),
        )]);
        let resolution = crate::vowifi::discovery::LineResolution {
            circuit_switched_excluded_ports: vec![],
            lines: vec![configured_modem_line(0, "/dev/ttyUSB3", serial)],
            failed: vec![],
        };
        assert!(still_missing_from_resolution(&pending, &resolution).is_empty());
    }

    /// Second P1 review finding ("Serial collision fakes recovery"): the
    /// previous fix bridged the two identifier spaces through
    /// `derive_module_id`, which keeps only the last six alphanumerics
    /// uppercased — so a *different* modem whose serial shares that suffix
    /// (here differing in both the prefix and the case of the suffix)
    /// resolved into the same `card_id` and was read as the configured
    /// modem recovering. The configured modem here is genuinely still
    /// absent and must stay missing.
    #[test]
    fn still_missing_from_resolution_is_not_fooled_by_a_colliding_serial_suffix() {
        let configured = "AAAAAAAAAAabcdef";
        let unrelated = "BBBBBBBBBBABCDEF";
        assert_eq!(
            crate::modules::discovery::derive_module_id(configured),
            crate::modules::discovery::derive_module_id(unrelated),
            "fixture sanity: these two serials must collide under derive_module_id"
        );

        let pending = std::collections::HashMap::from([(
            configured.to_string(),
            std::time::Instant::now() + Duration::from_secs(60),
        )]);
        // The line that actually resolved is the *unrelated* modem.
        let resolution = crate::vowifi::discovery::LineResolution {
            circuit_switched_excluded_ports: vec![],
            lines: vec![configured_modem_line(0, "/dev/ttyUSB9", unrelated)],
            failed: vec![],
        };
        assert!(
            still_missing_from_resolution(&pending, &resolution).contains(configured),
            "a colliding serial suffix must never be read as the configured line recovering"
        );
    }

    /// An auto-discovered line (no override behind it) records no
    /// identifier at all, so it can never satisfy a pending configured
    /// override — the subsystem starting on unrelated hardware must not
    /// clear the line the operator is actually waiting for.
    #[test]
    fn still_missing_from_resolution_is_not_satisfied_by_an_unconfigured_line() {
        let pending = std::collections::HashMap::from([(
            "/dev/ttyUSB3".to_string(),
            std::time::Instant::now() + Duration::from_secs(60),
        )]);
        let resolution = crate::vowifi::discovery::LineResolution {
            circuit_switched_excluded_ports: vec![],
            lines: vec![modem_line(0, "/dev/ttyUSB3")],
            failed: vec![],
        };
        assert!(
            still_missing_from_resolution(&pending, &resolution).contains("/dev/ttyUSB3"),
            "a line with no configured_identifier is not the pinned line, whatever its port"
        );
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
            configured_identifier: None,
            msisdn: None,
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
            configured_identifier: None,
            msisdn: None,
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
    fn line_in_call_reads_the_agents_calls_file_and_defaults_to_no_call() {
        let mock = MockCommandRunner::new();
        assert!(!line_in_call(&mock, "/tmp/pcscf-0"), "no file: no call");
        mock.set_file("/tmp/pcscf-0.calls", "1\n");
        assert!(line_in_call(&mock, "/tmp/pcscf-0"));
        assert!(!line_in_call(&mock, "/tmp/pcscf-1"), "another line's file");
        mock.set_file("/tmp/pcscf-0.calls", "0\n");
        assert!(!line_in_call(&mock, "/tmp/pcscf-0"));
        mock.set_file("/tmp/pcscf-0.calls", "garbage\n");
        assert!(!line_in_call(&mock, "/tmp/pcscf-0"));
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
                real_shutting_down: None,
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
                real_shutting_down: None,
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
                real_shutting_down: None,
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

    mod rebuild_sync_tests {
        use super::*;
        use crate::supervise::runner::MockCommandRunner;
        use std::cell::Cell;

        // MOCK JUSTIFICATION (constitution Principle I): the runner stands in
        // for the real clock and charon log, which CI cannot provide. The
        // waiting logic under test is the real production code.

        const WAIT: Duration = Duration::from_secs(20);
        const POLL: Duration = Duration::from_secs(1);

        #[test]
        fn a_negotiation_that_appears_on_the_third_read_is_returned_after_two_sleeps() {
            let runner = MockCommandRunner::new();
            let reads = Cell::new(0);
            let got = await_fresh_pcscf(&runner, WAIT, POLL, &|| false, &|| {
                reads.set(reads.get() + 1);
                (reads.get() == 3).then(|| "192.0.2.11".to_string())
            });
            assert_eq!(got, Some("192.0.2.11".to_string()));
            assert_eq!(runner.sleeps.lock().unwrap().len(), 2);
        }

        #[test]
        fn no_negotiation_gives_up_after_the_bound() {
            let runner = MockCommandRunner::new();
            assert_eq!(
                await_fresh_pcscf(&runner, WAIT, POLL, &|| false, &|| None),
                None
            );
            assert_eq!(runner.sleeps.lock().unwrap().len(), 20);
        }

        #[test]
        fn shutdown_abandons_the_wait_at_once() {
            let runner = MockCommandRunner::new();
            let reads = Cell::new(0);
            let got = await_fresh_pcscf(&runner, WAIT, POLL, &|| true, &|| {
                reads.set(reads.get() + 1);
                None
            });
            assert_eq!(got, None);
            assert_eq!(reads.get(), 0);
            assert!(runner.sleeps.lock().unwrap().is_empty());
        }

        #[test]
        fn closing_the_gate_waits_out_an_in_flight_spawn() {
            let gate = Arc::new(RebuildGate::default());
            let slot = gate.spawn.lock().unwrap();
            let g = Arc::clone(&gate);
            let closer = std::thread::spawn(move || g.close());
            std::thread::sleep(Duration::from_millis(50));
            assert!(
                !gate.flag.load(Ordering::SeqCst),
                "close() must not complete while a spawn holds the slot"
            );
            drop(slot);
            closer.join().unwrap();
            assert!(gate.flag.load(Ordering::SeqCst));
            gate.open();
            assert!(!gate.flag.load(Ordering::SeqCst));
        }

        #[test]
        fn the_gate_returns_immediately_when_clear() {
            let runner = MockCommandRunner::new();
            wait_for_rebuild_gate(&AtomicBool::new(false), &runner, WAIT, &|| false);
            assert!(runner.sleeps.lock().unwrap().is_empty());
        }

        #[test]
        fn the_gate_returns_once_the_flag_clears() {
            let runner = MockCommandRunner::new();
            let flag = AtomicBool::new(true);
            // Clear it from inside `stop`, which is polled once per iteration.
            let polls = Cell::new(0);
            wait_for_rebuild_gate(&flag, &runner, WAIT, &|| {
                polls.set(polls.get() + 1);
                if polls.get() == 2 {
                    flag.store(false, Ordering::SeqCst);
                }
                false
            });
            assert_eq!(runner.sleeps.lock().unwrap().len(), 2);
        }

        #[test]
        fn the_gate_gives_up_at_the_bound_even_if_never_cleared() {
            let runner = MockCommandRunner::new();
            wait_for_rebuild_gate(&AtomicBool::new(true), &runner, WAIT, &|| false);
            assert_eq!(runner.sleeps.lock().unwrap().len(), 20);
        }

        #[test]
        fn the_gate_is_abandoned_on_shutdown() {
            let runner = MockCommandRunner::new();
            wait_for_rebuild_gate(&AtomicBool::new(true), &runner, WAIT, &|| true);
            assert!(runner.sleeps.lock().unwrap().is_empty());
        }
    }

    mod gm_state_tests {
        use super::*;
        use crate::supervise::runner::MockCommandRunner;
        use std::os::unix::process::ExitStatusExt;

        // MOCK JUSTIFICATION (constitution Principle I): root `ip netns exec`
        // and a populated XFRM table cannot exist in CI. The counting and
        // flush logic under test is the real production code.

        fn dump(n: usize) -> String {
            (0..n)
                .map(|i| format!("src 198.51.100.10 dst 192.0.2.10\n\tproto esp spi 0x{i:08x}\n"))
                .collect()
        }

        fn seed(runner: &MockCommandRunner, what: &str, status: i32, out: &str) {
            runner.set_run_output(
                &format!("netns:ims0:ip xfrm {what}"),
                std::process::Output {
                    status: std::process::ExitStatus::from_raw(status),
                    stdout: out.as_bytes().to_vec(),
                    stderr: Vec::new(),
                },
            );
        }

        fn flushes(runner: &MockCommandRunner) -> Vec<Vec<String>> {
            runner
                .run_in_netns_calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, a)| a.last().is_some_and(|l| l == "flush"))
                .map(|(_, a)| a.clone())
                .collect()
        }

        #[test]
        fn stale_state_and_policies_are_counted_and_flushed_in_the_namespace() {
            let runner = MockCommandRunner::new();
            seed(&runner, "state", 0, &dump(4));
            seed(&runner, "policy", 0, &dump(8));
            assert_eq!(clear_line_gm_state(&runner, 0, "ims0"), (4, 8));
            let f = flushes(&runner);
            assert!(f.contains(&vec![
                "ip".into(),
                "xfrm".into(),
                "state".into(),
                "flush".into()
            ]));
            assert!(f.contains(&vec![
                "ip".into(),
                "xfrm".into(),
                "policy".into(),
                "flush".into()
            ]));
            assert!(
                !runner
                    .run_calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|c| c.contains(&"xfrm".to_string())),
                "the host namespace's tunnel SAs must never be touched"
            );
        }

        #[test]
        fn an_empty_namespace_is_left_alone() {
            let runner = MockCommandRunner::new();
            seed(&runner, "state", 0, "");
            seed(&runner, "policy", 0, "");
            assert_eq!(clear_line_gm_state(&runner, 0, "ims0"), (0, 0));
            assert!(flushes(&runner).is_empty());
        }

        #[test]
        fn a_failed_dump_still_flushes() {
            let runner = MockCommandRunner::new();
            seed(&runner, "state", 1, "");
            seed(&runner, "policy", 1, "");
            assert_eq!(clear_line_gm_state(&runner, 0, "ims0"), (0, 0));
            assert_eq!(flushes(&runner).len(), 2);
        }

        #[test]
        fn a_failed_flush_is_not_reported_as_cleared() {
            let runner = MockCommandRunner::new();
            seed(&runner, "state", 0, &dump(4));
            seed(&runner, "policy", 0, &dump(8));
            seed(&runner, "state flush", 1, "");
            assert_eq!(clear_line_gm_state(&runner, 0, "ims0"), (0, 8));
        }

        #[test]
        fn only_the_strongswan_engine_clears_gm_state() {
            assert!(should_clear_gm_state("strongswan"));
            assert!(!should_clear_gm_state("swu"));
        }
    }

    mod tunnel_signal_tests {
        use super::*;
        use crate::control::protocol::IkeSaLossCause;
        use std::cell::RefCell;

        #[test]
        fn a_recovery_is_reported_as_a_counted_event_with_its_reason() {
            use crate::control::protocol::{ObservedEvent, TunnelRecoveryReason};
            let event = tunnel_recovered_event(
                line_supervisor::DegradeReason::DefaultRouteMissing,
                &[],
                None,
                SystemTime::now(),
            );
            assert_eq!(
                event,
                ObservedEvent::TunnelRecovered {
                    reason: TunnelRecoveryReason::DefaultRouteMissing,
                    pcscfs: Vec::new(),
                    rebuilt_at_unix: None,
                    dead_peer: None,
                }
            );
        }

        #[test]
        fn a_dead_peer_loss_carries_its_verdict_and_says_why() {
            use crate::control::protocol::{DeadPeerVerdict, ObservedEvent, TunnelRecoveryReason};
            let event = tunnel_recovered_event(
                line_supervisor::DegradeReason::IkeSaLost,
                &[],
                Some(DeadPeerVerdict::False),
                SystemTime::now(),
            );
            assert_eq!(
                event,
                ObservedEvent::TunnelRecovered {
                    reason: TunnelRecoveryReason::IkeSaLost,
                    pcscfs: Vec::new(),
                    rebuilt_at_unix: None,
                    dead_peer: Some(DeadPeerVerdict::False),
                }
            );
            let c = line_supervisor::DeadPeerClassification {
                verdict: DeadPeerVerdict::False,
                snapshot_age: Some(Duration::from_secs(12)),
                last_in_before_loss: Some(Duration::from_secs(42)),
                packets: Some(1834),
                rx_since_snapshot: Some(3),
            };
            assert_eq!(
                dead_peer_detail(0, &c),
                "[supervise] line 0: dead peer verdict: false (inbound ESP 42s before the loss, \
                 1834 packets, snapshot 12s old, 3 received on the tunnel since; charon \
                 retransmits for 165s)"
            );
            let unread = line_supervisor::DeadPeerClassification {
                verdict: DeadPeerVerdict::Unknown,
                rx_since_snapshot: None,
                last_in_before_loss: Some(Duration::from_secs(300)),
                ..c
            };
            assert_eq!(
                dead_peer_detail(0, &unread),
                "[supervise] line 0: dead peer verdict: unknown (inbound ESP 300s before the \
                 loss, 1834 packets, snapshot 12s old, tunnel counter unread; charon retransmits \
                 for 165s)"
            );
            let none = line_supervisor::DeadPeerClassification {
                verdict: DeadPeerVerdict::Unknown,
                snapshot_age: None,
                last_in_before_loss: None,
                packets: None,
                rx_since_snapshot: None,
            };
            assert_eq!(
                dead_peer_detail(1, &none),
                "[supervise] line 1: dead peer verdict: unknown (no inbound ESP snapshot)"
            );
        }

        #[test]
        fn a_pool_exhausted_recovery_reports_the_exhausted_pcscfs() {
            use crate::control::protocol::{ObservedEvent, TunnelRecoveryReason};
            let members = [line_supervisor::ExhaustedMember {
                addr: "10.0.0.11".into(),
                cause: "gm-keepalive".into(),
                until_unix: 1_790_000_330,
                marked_at_unix: 1_790_000_000,
            }];
            let event = tunnel_recovered_event(
                line_supervisor::DegradeReason::PcscfPoolExhausted,
                &members,
                None,
                std::time::UNIX_EPOCH + Duration::from_secs(1_790_000_400),
            );
            assert_eq!(
                event,
                ObservedEvent::TunnelRecovered {
                    reason: TunnelRecoveryReason::PcscfPoolExhausted,
                    pcscfs: vec!["10.0.0.11".into()],
                    rebuilt_at_unix: Some(1_790_000_400),
                    dead_peer: None,
                }
            );
            let detail = pool_exhausted_detail(
                0,
                &members,
                Some(std::time::UNIX_EPOCH + Duration::from_secs(1_789_999_000)),
            );
            assert!(
                detail.contains("10.0.0.11 (gm-keepalive, marked "),
                "{detail}"
            );
            assert!(
                detail.contains("since the negotiation at 2026-"),
                "{detail}"
            );
        }

        #[test]
        fn the_pool_state_is_read_through_the_runner_and_unusable_is_none() {
            let mock = MockCommandRunner::new();
            assert!(line_pool_state(&mock, "/tmp/pcscf-0").is_none(), "no file");
            mock.set_file("/tmp/pcscf-0.state", "garbage");
            assert!(line_pool_state(&mock, "/tmp/pcscf-0").is_none());
            mock.set_file(
                "/tmp/pcscf-0.state",
                r#"{"version":1,"last_registered":null,"marks":[{"addr":"10.0.0.11","until_unix":9,"failures":1,"cause":"transport","marked_at_unix":3}]}"#,
            );
            let state = line_pool_state(&mock, "/tmp/pcscf-0").unwrap();
            assert_eq!(state.marks[0].marked_at_unix, 3);
            assert!(
                line_pool_state(&mock, "/tmp/pcscf-1").is_none(),
                "another line's file"
            );
        }

        #[test]
        fn only_a_changed_signal_is_reported() {
            let sent = RefCell::new(Vec::new());
            let report = |s: TunnelSignal| sent.borrow_mut().push(s);
            let mut signal = TunnelSignal {
                child_sa_up: true,
                flapping: false,
                loss_cause: None,
            };
            let rebuilds = line_supervisor::RebuildWindow::new(Duration::from_secs(300));
            let mut tracking = line_supervisor::SteadyTracking {
                child_sa_usable: true,
                ..Default::default()
            };
            sync_tunnel_signal(&mut signal, &tracking, &rebuilds, None, &report);
            assert!(sent.borrow().is_empty(), "nothing changed");
            // A new loss cause alone is a change worth reporting.
            let cause = Some(IkeSaLossCause::CarrierDelete);
            sync_tunnel_signal(&mut signal, &tracking, &rebuilds, cause, &report);
            assert_eq!(sent.borrow().len(), 1);
            assert_eq!(sent.borrow()[0].loss_cause, cause);
            // ...and the same state again is not.
            sync_tunnel_signal(&mut signal, &tracking, &rebuilds, cause, &report);
            assert_eq!(sent.borrow().len(), 1);
            tracking.child_sa_usable = false;
            sync_tunnel_signal(&mut signal, &tracking, &rebuilds, cause, &report);
            assert_eq!(sent.borrow().len(), 2);
            assert!(!sent.borrow()[1].child_sa_up);
        }
    }
}
