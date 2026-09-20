//! Host-side IMS over LTE orchestration (specs/021-entrypoint-supervise-rust
//! Phase 4) — 1:1 port of `docker/entrypoint.sh`'s VoLTE section. Not
//! exercised against real hardware this session ([volte].enabled = false in
//! the deployment this branch was validated against — see quickstart.md /
//! DECISIONS-LOG.md); ported with the same care as the VoWiFi path and
//! covered by the same `cargo test --workspace` gate, but flagging that this
//! specific path's live-validation is still outstanding.

use super::epdg_iface;
use super::runner::{ChildSpec, CommandRunner};
use super::shutdown::{LegacyVolteRegistration, StartedState, StartedVolteLine};
use crate::config::AppConfig;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

const VOLTE_RESTORE_CID_PATH: &str = "/run/volte-restore-cid";

/// specs/080-volte-pcscf-auto-prime FR-001/FR-005: if a usable P-CSCF is
/// already available (an explicit override, or a valid cache), does nothing
/// and returns `true` immediately — no `discover`/charon/pcscd activity of
/// any kind, so an already-primed or explicitly-pinned deployment sees no
/// new startup cost (SC-003). Otherwise runs exactly one priming attempt
/// (logging its outcome either way, FR-007) and returns `false` — callers
/// decide their own retry cadence (FR-008); this makes one attempt and
/// returns.
///
/// Extracted into its own function, rather than inlined at each call site,
/// specifically so it is unit-testable without spinning up either call
/// site's background thread/loop.
fn ensure_pcscf_primed(
    runner: &Arc<dyn CommandRunner>,
    bin: &str,
    config_path: &str,
    config: &AppConfig,
    override_addr: Option<&str>,
    shutting_down: &Arc<RwLock<bool>>,
    priming_activity: &Arc<Mutex<()>>,
) -> bool {
    ensure_pcscf_primed_with(
        std::path::Path::new(&config.volte.pcscf_source_path),
        override_addr,
        || {
            // Held for the whole attempt (specs/080-volte-pcscf-auto-prime,
            // Greptile PR #87 review, finding 2) — `orchestrate::run`'s
            // shutdown sequence waits for this same lock right after setting
            // the real `shutting_down` flag, so it never builds/executes the
            // real teardown plan while this attempt's own local one (below,
            // in `prime_with_line`) is still in flight.
            let _guard = priming_activity.lock();
            super::orchestrate_prime::prime_pcscf(
                Arc::clone(runner),
                bin,
                config_path,
                config,
                shutting_down,
            )
        },
    )
}

/// The decision `ensure_pcscf_primed` makes, with the actual capture attempt
/// injected as a closure rather than called directly.
///
/// Split out live, on the Vodafone rig (2026-09-17): `prime_pcscf`'s
/// discovery step calls the real modem scanner directly, not through
/// `CommandRunner` (see `orchestrate_prime::discover_priming_line`'s doc
/// comment for why it has to). That makes `prime_pcscf` itself untestable
/// without real hardware — exactly the "hardware not available in CI"
/// situation the constitution's mocking carve-out exists for — but this
/// decision (skip vs. attempt-and-report-false) doesn't need `prime_pcscf`
/// to actually run to be tested; it only needs to know whether it *was*
/// called.
fn ensure_pcscf_primed_with(
    cache_path: &std::path::Path,
    override_addr: Option<&str>,
    attempt_prime: impl FnOnce() -> Result<(), String>,
) -> bool {
    if crate::volte::pcscf::pcscf_is_available(cache_path, override_addr) {
        return true;
    }
    println!(
        "[supervise] priming: no usable P-CSCF yet for VoLTE; capturing one via a transient \
         VoWiFi tunnel before proceeding"
    );
    match attempt_prime() {
        Ok(()) => true,
        Err(e) => {
            eprintln!("[supervise] priming failed: {e}");
            false
        }
    }
}

/// Entry point, called from `orchestrate::run` when `[volte].enabled`.
pub fn start(
    runner: Arc<dyn CommandRunner>,
    bin: String,
    config_path: String,
    config: AppConfig,
    started: Arc<Mutex<StartedState>>,
    shutting_down: Arc<RwLock<bool>>,
    priming_activity: Arc<Mutex<()>>,
) {
    if config.volte.bridge_inbound {
        start_multiline(
            runner,
            bin,
            config_path,
            config,
            started,
            shutting_down,
            priming_activity,
        );
    } else {
        start_legacy_registration(
            runner,
            bin,
            config_path,
            config,
            started,
            shutting_down,
            priming_activity,
        );
    }
}

/// The auto-discovered, multi-line path (specs/020-volte-line-netns) — every
/// line in its own namespace, one carrier-agent process each, plus the one
/// shared `volte-bridge` telephony half.
fn start_multiline(
    runner: Arc<dyn CommandRunner>,
    bin: String,
    config_path: String,
    config: AppConfig,
    started: Arc<Mutex<StartedState>>,
    shutting_down: Arc<RwLock<bool>>,
    priming_activity: Arc<Mutex<()>>,
) {
    // Entirely on a background thread (Greptile PR #87 review, finding 2's
    // remaining edge): `volte-discover-lines` and, especially, the priming
    // pre-flight loop below (up to ~4 minutes per attempt) used to run
    // synchronously on `orchestrate::run`'s own thread, *before* it reached
    // `wait_for_signal()` — so a real shutdown signal arriving during any of
    // this had no installed handler to catch it yet, and the OS's default
    // disposition (terminate) applied regardless of `real_shutting_down`/
    // `priming_activity` plumbing. Spawning immediately, mirroring every
    // other subsystem's own startup convention (`start_vowifi_subsystem`
    // returns right after spawning each line's own thread), lets `run()`
    // reach `wait_for_signal()` right away, so that plumbing applies to
    // every priming attempt, including the very first.
    std::thread::spawn(move || {
        println!(
        "[supervise] [volte].enabled + bridge_inbound — answering inbound calls over LTE (auto-discovering modems, up to {} line(s))",
        config.volte.max_lines
    );

        let status = runner.run(&[
            &bin,
            "--config",
            &config_path,
            "volte-discover-lines",
            "--restore-cid-path",
            VOLTE_RESTORE_CID_PATH,
        ]);
        match status {
            Ok(o) if o.status.success() => {}
            _ => {
                eprintln!("[supervise] FATAL: 'volte-discover-lines' failed — see error above");
                return;
            }
        }

        let manifest_path = crate::volte::discovery::manifest_path();
        let manifest = match crate::volte::discovery::read_manifest(&manifest_path) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[supervise] FATAL: could not read VoLTE line manifest: {e}");
                return;
            }
        };
        println!(
            "[supervise] volte-discover-lines: VOLTE_LINE_COUNT={}",
            manifest.lines.len()
        );

        if manifest.lines.is_empty() {
            eprintln!(
            "[supervise] PROMINENT ERROR: [volte].enabled + bridge_inbound is true but no usable VoLTE \
             line was discovered — the VoLTE subsystem will NOT start this run."
        );
            return;
        }

        // specs/080-volte-pcscf-auto-prime FR-001/FR-002b/FR-005/FR-008: a
        // one-time pre-flight, unlike the legacy path's per-retry-iteration
        // check — every discovered line already shares this one cache path
        // (research.md R5), so there is nothing to gain from re-checking per
        // line. No enclosing retry loop exists at this scope, so this loop uses
        // the same 15s cadence directly rather than introducing a new one.
        //
        // Now runs on this function's own background thread (see the
        // `std::thread::spawn` this whole body is wrapped in, above), not
        // `orchestrate::run`'s — so `real_shutting_down`/`priming_activity`
        // apply to every attempt here, including the first.
        {
            let override_addr = manifest
                .lines
                .first()
                .filter(|l| !l.pcscf.is_empty())
                .map(|l| l.pcscf.as_str());
            while !ensure_pcscf_primed(
                &runner,
                &bin,
                &config_path,
                &config,
                override_addr,
                &shutting_down,
                &priming_activity,
            ) {
                if *shutting_down.read().unwrap() {
                    return;
                }
                runner.sleep(Duration::from_secs(15));
            }
        }

        // specs/041-shutdown-resource-cleanup US2/FR-014: mirrors the VoWiFi
        // reclamation in orchestrate::run — a force-killed previous run's
        // namespace/veth can still be on the host. Run before this loop creates
        // anything of its own, using each discovered line's own namespace/veth
        // names (a name this run's own discovery could also produce is, by
        // construction, ours — research.md R7). No XFRM/if_id concept here, so
        // (unlike the VoWiFi call) there is no separate flush step to pair it
        // with.
        let reclaim_candidates: Vec<epdg_iface::ReclaimCandidate> = manifest
            .lines
            .iter()
            .map(|l| {
                let suffix = if l.index == 0 {
                    String::new()
                } else {
                    l.index.to_string()
                };
                let has_veth = !l.veth_carrier_addr.is_empty();
                let veth_host =
                    has_veth.then(|| format!("{}{suffix}", config.volte.veth_telephony_iface));
                epdg_iface::ReclaimCandidate {
                    netns: l.netns.clone(),
                    tun_iface: None,
                    veth_host,
                    // Proof of ownership: the carrier-side veth end this
                    // deployment creates *inside* the namespace. A line with no
                    // veth at all (the diagnostic single-`--modem` path) cannot
                    // prove ownership, so `None` vetoes reclaiming it.
                    owned_iface_marker: has_veth
                        .then(|| format!("{}{suffix}", config.volte.veth_carrier_iface)),
                }
            })
            .collect();
        epdg_iface::reclaim_leftover_lines(
            runner.as_ref(),
            &reclaim_candidates,
            epdg_iface::reclaim_leftover_enabled(),
        );

        for line in &manifest.lines {
            let runner = Arc::clone(&runner);
            let bin = bin.clone();
            let config_path = config_path.clone();
            let started = Arc::clone(&started);
            let shutting_down = Arc::clone(&shutting_down);
            let idx = line.index;
            let card_id = line.card_id.clone();
            let modem_port = line.modem_port.clone();
            let netns = line.netns.clone();
            let veth_carrier_addr = line.veth_carrier_addr.clone();
            let veth_telephony_addr = line.veth_telephony_addr.clone();
            let veth_carrier_iface = format!(
                "{}{}",
                config.volte.veth_carrier_iface,
                if idx == 0 {
                    String::new()
                } else {
                    idx.to_string()
                }
            );
            let veth_telephony_iface = format!(
                "{}{}",
                config.volte.veth_telephony_iface,
                if idx == 0 {
                    String::new()
                } else {
                    idx.to_string()
                }
            );

            println!("[supervise] volte line {idx} ({card_id}): netns={netns}");

            // Modem's own IMS/VoLTE stack reconciliation — must run before
            // anything else touches this modem (research.md of
            // specs/020-volte-line-netns).
            if runner
                .run(&[
                    &bin,
                    "--config",
                    &config_path,
                    "modem-ims",
                    "--modem",
                    &modem_port,
                ])
                .map(|o| !o.status.success())
                .unwrap_or(true)
            {
                eprintln!("[supervise] volte line {idx}: FATAL: could not reconcile modem IMS mode; skipping this line");
                continue;
            }

            if !ensure_volte_line_netns(runner.as_ref(), &netns, &line.iface) {
                eprintln!("[supervise] volte line {idx}: FATAL: interface {} not present in container; skipping this line", line.iface);
                continue;
            }
            if !veth_carrier_addr.is_empty() {
                ensure_volte_line_veth(
                    runner.as_ref(),
                    &veth_telephony_iface,
                    &veth_carrier_iface,
                    &netns,
                    &veth_telephony_addr,
                    &veth_carrier_addr,
                );
            }

            started.lock().unwrap().started_netns.push(netns.clone());
            let mut state = started.lock().unwrap();
            let entry = state.volte_lines.iter_mut().find(|l| l.index == idx);
            if entry.is_none() {
                state.volte_lines.push(StartedVolteLine {
                    index: idx,
                    netns: netns.clone(),
                    carrier_agent_handles: Vec::new(),
                    // `None` when this line has no carrier veth pair at all —
                    // the diagnostic single-`--modem` path (`carrier_agent.rs`'s
                    // empty-address branch) never calls `ensure_volte_line_veth`
                    // below, so there is nothing here to delete at stop.
                    veth_host: if veth_carrier_addr.is_empty() {
                        None
                    } else {
                        Some(veth_telephony_iface.clone())
                    },
                });
            }
            drop(state);

            println!("[supervise] volte line {idx}: starting volte-carrier-agent (netns {netns}), supervised...");
            std::thread::spawn(move || loop {
                let guard = shutting_down.read().unwrap();
                if *guard {
                    return;
                }
                match runner.spawn(ChildSpec::new([
                    "ip",
                    "netns",
                    "exec",
                    &netns,
                    &bin,
                    "--config",
                    &config_path,
                    "volte-carrier-agent",
                    "--line",
                    &idx.to_string(),
                ])) {
                    Ok(handle) => {
                        // Shared: this loop polls liveness, the shutdown plan
                        // signals the same child from another thread.
                        let handle = std::sync::Arc::new(handle);
                        let mut state = started.lock().unwrap();
                        if let Some(entry) = state.volte_lines.iter_mut().find(|l| l.index == idx) {
                            entry.carrier_agent_handles.push(handle.clone());
                        }
                        drop(state);
                        drop(guard);
                        // Poll is_alive() rather than block on wait(): a real
                        // Greptile finding (mirroring the one already fixed on
                        // the vowifi-usim-bridge holder — see runner.rs) caught
                        // that RealCommandRunner::wait() removes the handle from
                        // the tracked table BEFORE blocking, which silently
                        // discards the shutdown plan's later `KillChild` signal
                        // to this exact handle (stored in
                        // `carrier_agent_handles` for that purpose) for the
                        // process's entire lifetime.
                        while runner.is_alive(&handle) {
                            runner.sleep(Duration::from_secs(1));
                        }
                        println!("[supervise] volte line {idx}: volte-carrier-agent exited; restarting in 15s");
                    }
                    Err(e) => {
                        drop(guard);
                        eprintln!(
                        "[supervise] volte line {idx}: failed to spawn volte-carrier-agent: {e}"
                    )
                    }
                }
                runner.sleep(Duration::from_secs(15));
            });
        }

        if started.lock().unwrap().volte_lines.is_empty() {
            eprintln!(
            "[supervise] PROMINENT ERROR: every VoLTE line failed to start (see FATAL lines above) — the \
             VoLTE subsystem will NOT start this run."
        );
            return;
        }

        println!("[supervise] starting volte-bridge (default netns, one shared process for all VoLTE lines), supervised...");
        std::thread::spawn(move || loop {
            let guard = shutting_down.read().unwrap();
            if *guard {
                return;
            }
            match runner.spawn(ChildSpec::new([
                bin.as_str(),
                "--config",
                config_path.as_str(),
                "volte-bridge",
            ])) {
                Ok(handle) => {
                    let handle = std::sync::Arc::new(handle);
                    started.lock().unwrap().volte_bridge_supervisor = Some(handle.clone());
                    drop(guard);
                    // See the volte-carrier-agent loop above: poll is_alive(),
                    // don't block on wait(), so this handle (which the shutdown
                    // plan signals via `volte_bridge_supervisor`) stays
                    // signalable for as long as the process is actually alive.
                    while runner.is_alive(&handle) {
                        runner.sleep(Duration::from_secs(1));
                    }
                    println!("[supervise] volte-bridge exited; restarting in 15s");
                }
                Err(e) => {
                    drop(guard);
                    eprintln!("[supervise] failed to spawn volte-bridge: {e}")
                }
            }
            runner.sleep(Duration::from_secs(15));
        });
    });
}

/// The legacy, single-line, registration-only path — hold the registration
/// open, nothing more (specs/017-volte-inbound-bridge FR-023 default).
fn start_legacy_registration(
    runner: Arc<dyn CommandRunner>,
    bin: String,
    config_path: String,
    config: AppConfig,
    started: Arc<Mutex<StartedState>>,
    shutting_down: Arc<RwLock<bool>>,
    priming_activity: Arc<Mutex<()>>,
) {
    println!("[supervise] [volte].enabled — starting host-side IMS over LTE (resolving one line from config)");
    std::thread::spawn(move || loop {
        if *shutting_down.read().unwrap() {
            return;
        }

        // specs/080-volte-pcscf-auto-prime FR-001/FR-005/FR-008: checked on
        // every iteration of this existing retry loop, not just once — a
        // priming failure falls through to this same loop's own 15s
        // sleep-and-retry, so no new retry/backoff concept is introduced.
        // FR-002b: always the first configured line's override, matching
        // the shared cache path itself always being line 0's default.
        //
        // Deliberately called with no read guard held: `ensure_pcscf_primed`
        // passes this same `shutting_down` lock down into priming's own
        // establish-loop check (Greptile PR #87 review, finding 2), and
        // `std::sync::RwLock` does not guarantee recursive read locks are
        // deadlock-free against a concurrent writer — holding a guard here
        // across that call risked a real deadlock against `orchestrate::
        // run`'s shutdown sequence taking the write lock. The guard below,
        // around the actual spawn-then-register sequence, is unaffected —
        // this call never runs inside it.
        let override_addr = config
            .volte
            .line_overrides
            .first()
            .and_then(|o| o.pcscf.as_deref());
        if !ensure_pcscf_primed(
            &runner,
            &bin,
            &config_path,
            &config,
            override_addr,
            &shutting_down,
            &priming_activity,
        ) {
            runner.sleep(Duration::from_secs(15));
            continue;
        }

        let guard = shutting_down.read().unwrap();
        if *guard {
            return;
        }

        match runner.spawn(ChildSpec::new([
            bin.as_str(),
            "--config",
            config_path.as_str(),
            "volte-register",
            "--pcscf-source-path",
            &config.volte.pcscf_source_path,
            "--status-path",
            &config.volte.status_path,
            "--lock-path",
            &config.volte.lock_path,
            "--restore-cid-path",
            VOLTE_RESTORE_CID_PATH,
            "--keep-pdn",
        ])) {
            Ok(handle) => {
                let handle = std::sync::Arc::new(handle);
                let mut state = started.lock().unwrap();
                state.legacy_volte_registration = Some(LegacyVolteRegistration {
                    supervisor_handle: handle.clone(),
                    bridge_inbound: false,
                    restore_cid: std::fs::read_to_string(VOLTE_RESTORE_CID_PATH).ok(),
                });
                drop(state);
                drop(guard);
                // See the volte-carrier-agent loop above: poll is_alive(),
                // don't block on wait(), so this handle (which the shutdown
                // plan signals via `legacy_volte_registration.
                // supervisor_handle`, then polls with `WaitForExit`) stays
                // signalable for as long as the process is actually alive.
                while runner.is_alive(&handle) {
                    runner.sleep(Duration::from_secs(1));
                }
                println!("[supervise] the LTE IMS service exited; restarting in 15s");
            }
            Err(e) => {
                drop(guard);
                eprintln!("[supervise] failed to spawn volte-register: {e}")
            }
        }
        // Longer than the 5s used elsewhere: a restart re-runs PDN
        // attachment and a full IMS-AKA exchange, so a tight loop would
        // hammer both the modem and the carrier's registrar.
        runner.sleep(Duration::from_secs(15));
    });
}

/// Idempotently ensures line `netns`'s namespace exists and `iface` (if any)
/// is inside it — 1:1 port of `ensure_volte_line_netns`.
fn ensure_volte_line_netns(runner: &dyn CommandRunner, netns: &str, iface: &str) -> bool {
    let netns_marker = format!("/var/run/netns/{netns}");
    let exists = runner
        .run(&["test", "-e", &netns_marker])
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !exists {
        let _ = runner.run(&["ip", "netns", "add", netns]);
    }
    let _ = runner.run_in_netns(netns, &["ip", "link", "set", "lo", "up"]);

    if iface.is_empty() {
        return true;
    }

    if runner
        .run_in_netns(netns, &["ip", "link", "show", iface])
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return true; // already in place — idempotent restart
    }
    if runner
        .run(&["ip", "link", "show", iface])
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        let _ = runner.run(&["ip", "link", "set", iface, "netns", netns]);
        return true;
    }
    false // not found in either namespace
}

/// Idempotently creates line's veth pair — 1:1 port of `ensure_volte_line_veth`.
fn ensure_volte_line_veth(
    runner: &dyn CommandRunner,
    veth_telephony: &str,
    veth_carrier: &str,
    netns: &str,
    telephony_addr: &str,
    carrier_addr: &str,
) {
    if !runner
        .run(&["ip", "link", "show", veth_telephony])
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        let _ = runner.run(&[
            "ip",
            "link",
            "add",
            veth_telephony,
            "type",
            "veth",
            "peer",
            "name",
            veth_carrier,
            "netns",
            netns,
        ]);
    }
    let _ = runner.run(&[
        "ip",
        "addr",
        "replace",
        &format!("{telephony_addr}/30"),
        "dev",
        veth_telephony,
    ]);
    let _ = runner.run(&["ip", "link", "set", veth_telephony, "up"]);
    let _ = runner.run_in_netns(
        netns,
        &[
            "ip",
            "addr",
            "replace",
            &format!("{carrier_addr}/30"),
            "dev",
            veth_carrier,
        ],
    );
    let _ = runner.run_in_netns(netns, &["ip", "link", "set", veth_carrier, "up"]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// specs/080-volte-pcscf-auto-prime User Story 3: an explicit override
    /// must skip priming entirely — no `discover`/charon/pcscd activity —
    /// regardless of what (if anything) is at the cache path.
    #[test]
    fn an_override_skips_priming_entirely() {
        let primed = ensure_pcscf_primed_with(
            std::path::Path::new("/nonexistent/pcscf"),
            Some("2402:8100::1"),
            || panic!("an override must short-circuit before any priming activity"),
        );

        assert!(primed);
    }

    /// specs/080-volte-pcscf-auto-prime User Story 3: a valid pre-existing
    /// cache must likewise skip priming entirely.
    #[test]
    fn a_valid_cache_skips_priming_entirely() {
        let cache = std::env::temp_dir().join(format!("pcscf-wiring-valid-{}", std::process::id()));
        std::fs::write(&cache, "2402:8100::5\n").unwrap();

        let primed = ensure_pcscf_primed_with(&cache, None, || {
            panic!("a valid cache must short-circuit before any priming activity")
        });

        assert!(primed);
        std::fs::remove_file(&cache).ok();
    }

    /// specs/080-volte-pcscf-auto-prime User Story 1: no override and no
    /// cache must trigger exactly one priming attempt and report "not yet
    /// primed" so the caller's own retry loop tries again. The actual
    /// capture is injected here — see `ensure_pcscf_primed_with`'s doc
    /// comment for why `prime_pcscf` itself can't be driven from a plain
    /// unit test.
    #[test]
    fn missing_cache_and_no_override_triggers_one_priming_attempt() {
        let attempted = std::cell::Cell::new(false);

        let primed =
            ensure_pcscf_primed_with(std::path::Path::new("/nonexistent/pcscf"), None, || {
                attempted.set(true);
                Err("simulated failure".to_string())
            });

        assert!(!primed, "must report not-yet-primed so the caller retries");
        assert!(attempted.get(), "must have attempted priming exactly once");
    }

    /// A priming attempt that succeeds must report `true` immediately, not
    /// `false` — the caller must not pay its own 15s retry-sleep on a cold
    /// start that just worked (Greptile PR #87 review).
    #[test]
    fn a_successful_priming_attempt_reports_primed_without_delay() {
        let primed =
            ensure_pcscf_primed_with(std::path::Path::new("/nonexistent/pcscf"), None, || Ok(()));

        assert!(
            primed,
            "a successful attempt must not fall through to a retry sleep"
        );
    }

    /// specs/080-volte-pcscf-auto-prime User Story 2: this is the exact same
    /// function and the exact same code path as a first-time deployment —
    /// there is no special-casing for "the cache used to be valid." A
    /// corrupt (not just missing) cache must be treated identically.
    #[test]
    fn a_corrupt_cache_is_treated_exactly_like_a_missing_one() {
        let cache =
            std::env::temp_dir().join(format!("pcscf-wiring-corrupt-{}", std::process::id()));
        std::fs::write(&cache, "not-an-address").unwrap();
        let attempted = std::cell::Cell::new(false);

        let primed = ensure_pcscf_primed_with(&cache, None, || {
            attempted.set(true);
            Err("simulated failure".to_string())
        });

        assert!(!primed);
        assert!(attempted.get());
        std::fs::remove_file(&cache).ok();
    }
}
