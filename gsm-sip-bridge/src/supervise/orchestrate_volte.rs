//! Host-side IMS over LTE orchestration (specs/021-entrypoint-supervise-rust
//! Phase 4) — 1:1 port of `docker/entrypoint.sh`'s VoLTE section. Not
//! exercised against real hardware this session ([volte].enabled = false in
//! the deployment this branch was validated against — see quickstart.md /
//! DECISIONS-LOG.md); ported with the same care as the VoWiFi path and
//! covered by the same `cargo test --workspace` gate, but flagging that this
//! specific path's live-validation is still outstanding.

use super::epdg_iface;
use super::runner::{ChildSpec, CommandRunner};
use super::shutdown::{StartedState, StartedVolteLine};
use crate::config::AppConfig;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

const VOLTE_RESTORE_CID_PATH: &str = "/run/volte-restore-cid";

/// Every `card_id` in `card_ids` that still lacks a usable P-CSCF address
/// (specs/081-multi-carrier-pcscf FR-002/FR-007), under the precedence
/// `line_pcscf_is_available` uses: an explicit override (from `overrides`,
/// keyed by `card_id`) always wins, otherwise this line's own per-`card_id`
/// cache.
///
/// Split out from the priming coordinator thread specifically so this
/// decision — which lines belong in the next pass — is testable without
/// spinning that thread or mocking `volte-discover-lines`/the manifest
/// subprocess chain.
fn lines_needing_priming(
    config: &AppConfig,
    card_ids: &[String],
    overrides: &std::collections::HashMap<String, String>,
) -> std::collections::BTreeSet<String> {
    card_ids
        .iter()
        .filter(|card_id| {
            !crate::volte::pcscf::line_pcscf_is_available(
                &config.volte.pcscf_source_path,
                card_id,
                overrides.get(*card_id).map(String::as_str),
            )
        })
        .cloned()
        .collect()
}

/// Entry point, called from `orchestrate::run` when `[volte].enabled`.
///
/// Auto-discovers every line (specs/020-volte-line-netns) and runs each in
/// its own namespace, one carrier-agent process each, plus the one shared
/// `volte-bridge` telephony half. The namespace is what keeps a line's IMS
/// traffic on its own LTE interface: in the host namespace a Wi-Fi or
/// Ethernet uplink with a lower-metric default route wins the egress (#165).
pub fn start(
    runner: Arc<dyn CommandRunner>,
    bin: String,
    config_path: String,
    config: AppConfig,
    started: Arc<Mutex<StartedState>>,
    shutting_down: Arc<RwLock<bool>>,
) {
    // Entirely on a background thread (Greptile PR #87 review): `volte-
    // discover-lines` and, especially, the priming pre-flight loop below (up
    // to ~4 minutes per attempt) used to run synchronously on `orchestrate::
    // run`'s own thread, *before* it reached `wait_for_signal()` — so a real
    // shutdown signal arriving during any of this had no installed handler
    // to catch it yet, and the OS's default disposition (terminate) applied
    // regardless of `real_shutting_down`. Spawning immediately, mirroring
    // every other subsystem's own startup convention (`start_vowifi_
    // subsystem` returns right after spawning each line's own thread), lets
    // `run()` reach `wait_for_signal()` right away.
    std::thread::spawn(move || {
        println!(
        "[supervise] [volte].enabled — answering inbound calls over LTE (auto-discovering modems, up to {} line(s))",
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
                "[supervise] PROMINENT ERROR: [volte].enabled is true but no usable VoLTE \
             line was discovered — the VoLTE subsystem will NOT start this run."
            );
            return;
        }

        // specs/081-multi-carrier-pcscf FR-002/FR-002b/FR-005/FR-009: every
        // manifest line that lacks a usable address (its own explicit
        // override or its own per-`card_id` cache — `line_pcscf_is_available`,
        // research.md R2) is primed together
        // in one concurrent pass (research.md R3), not just the first
        // discovered line. This coordinator runs on its own background
        // thread, separate from the per-line spawn loop below, and retries
        // whichever lines are still missing after each pass on the
        // existing 15s cadence — but it never blocks that spawn loop: each
        // line's own thread (further down) independently polls for its own
        // address and proceeds the instant it appears, regardless of what
        // this coordinator is still doing for any other line (FR-005).
        // Only one `prime_pass` call is ever in flight at a time (this
        // single thread), avoiding two concurrent passes fighting over the
        // one shared charon/pcscd a pass uses (research.md R3/R6).
        {
            let runner = Arc::clone(&runner);
            let bin = bin.clone();
            let config_path = config_path.clone();
            let config = config.clone();
            let started = Arc::clone(&started);
            let shutting_down = Arc::clone(&shutting_down);
            let card_ids: Vec<String> = manifest.lines.iter().map(|l| l.card_id.clone()).collect();
            let overrides: std::collections::HashMap<String, String> = manifest
                .lines
                .iter()
                .filter(|l| !l.pcscf.is_empty())
                .map(|l| (l.card_id.clone(), l.pcscf.clone()))
                .collect();

            std::thread::spawn(move || loop {
                if *shutting_down.read().unwrap() {
                    return;
                }
                let needed = lines_needing_priming(&config, &card_ids, &overrides);
                if needed.is_empty() {
                    return;
                }
                println!(
                    "[supervise] priming: no usable P-CSCF yet for {} line(s); capturing via a \
                     transient VoWiFi tunnel before those lines register",
                    needed.len()
                );
                let outcomes = super::orchestrate_prime::prime_pass(
                    Arc::clone(&runner),
                    &bin,
                    &config_path,
                    &config,
                    &needed,
                    &started,
                    &shutting_down,
                );
                for (card_id, outcome) in &outcomes {
                    match outcome {
                        Ok(()) => println!("[supervise] priming: line {card_id}: succeeded"),
                        Err(e) => eprintln!("[supervise] priming: line {card_id}: failed: {e}"),
                    }
                }
                if *shutting_down.read().unwrap() {
                    return;
                }
                runner.sleep(Duration::from_secs(15));
            });
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

            // Held across creating this line's netns/veth *and* registering
            // it into `started` — not just a one-off check — matching the
            // same "critical section" idiom `orchestrate::run`'s own daemon-
            // supervisor loop and `establish_line_tunnel`'s charon-spawn use
            // (see their own comments): `orchestrate::run`'s shutdown
            // sequence takes a *write* guard on this same `shutting_down`
            // before snapshotting `started`, which blocks until every
            // outstanding *read* guard — including this one — is released,
            // so a real shutdown can never see this line's netns/veth exist
            // on the host without also seeing it in the snapshot (Greptile
            // PR #87 review). Bounded: `ensure_volte_line_netns`/`_veth` are
            // plain local `ip`/`ip netns` commands, not the unbounded modem-
            // hardware AT calls above, so this section is always fast.
            let guard = shutting_down.read().unwrap();
            if *guard {
                drop(guard);
                break;
            }
            if !ensure_volte_line_netns(runner.as_ref(), &netns, &line.iface) {
                drop(guard);
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
            drop(guard);

            // specs/081-multi-carrier-pcscf FR-005/FR-009: this line's own
            // gate, checked fresh on every loop iteration (so a restart
            // after the carrier agent exits re-checks too, though in
            // practice the address never regresses once captured). Reuses
            // this same thread — already one per line, already retrying on
            // this exact cadence — rather than a new synchronization
            // primitive (research.md R5): a line whose address isn't ready
            // yet (still being primed, or its own priming attempt failed
            // and the coordinator above hasn't retried it yet) simply waits
            // here, never blocking or being blocked by any other line.
            let pcscf_source_path_for_gate = config.volte.pcscf_source_path.clone();
            let override_addr_for_gate = if line.pcscf.is_empty() {
                None
            } else {
                Some(line.pcscf.clone())
            };
            let card_id_for_gate = card_id.clone();

            println!("[supervise] volte line {idx}: starting volte-carrier-agent (netns {netns}), supervised...");
            std::thread::spawn(move || loop {
                let guard = shutting_down.read().unwrap();
                if *guard {
                    return;
                }
                if !crate::volte::pcscf::line_pcscf_is_available(
                    &pcscf_source_path_for_gate,
                    &card_id_for_gate,
                    override_addr_for_gate.as_deref(),
                ) {
                    drop(guard);
                    runner.sleep(Duration::from_secs(15));
                    continue;
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

    fn test_config_with_pcscf_base(base: &str) -> AppConfig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[sip]\nserver = \"sip.example.com\"\nusername = \"user\"\npassword = \"pass\"\n",
        )
        .unwrap();
        let mut config = crate::config::load_config(&path).unwrap();
        config.volte.pcscf_source_path = base.to_string();
        config
    }

    /// specs/081-multi-carrier-pcscf FR-002/FR-002b/User Story 1: a fully
    /// unconfigured, multi-carrier fleet must have every one of its lines
    /// come back as needing priming — not just the first.
    #[test]
    fn every_unconfigured_line_needs_priming() {
        let base = std::env::temp_dir()
            .join(format!("lines-needing-priming-none-{}", std::process::id()))
            .to_string_lossy()
            .to_string();
        let config = test_config_with_pcscf_base(&base);
        let card_ids = vec!["ec20-AAAAAA".to_string(), "ec20-BBBBBB".to_string()];

        let needed = lines_needing_priming(&config, &card_ids, &Default::default());

        assert_eq!(
            needed,
            ["ec20-AAAAAA", "ec20-BBBBBB"]
                .into_iter()
                .map(String::from)
                .collect()
        );
    }

    /// specs/081-multi-carrier-pcscf User Story 3/SC-003: a line with an
    /// explicit override, or a line whose own per-line cache is already
    /// valid, must never appear in the needed set — only the genuinely
    /// unconfigured line should.
    #[test]
    fn already_available_lines_are_excluded_mixed_carrier_fleet() {
        let base = std::env::temp_dir()
            .join(format!(
                "lines-needing-priming-mixed-{}",
                std::process::id()
            ))
            .to_string_lossy()
            .to_string();
        let config = test_config_with_pcscf_base(&base);
        let primed_cache = crate::volte::pcscf::per_line_cache_path(&base, "ec20-CACHED");
        std::fs::write(&primed_cache, "2402:8100::5\n").unwrap();

        let card_ids = vec![
            "ec20-PINNED".to_string(),
            "ec20-CACHED".to_string(),
            "ec20-UNCONFIGURED".to_string(),
        ];
        let mut overrides = std::collections::HashMap::new();
        overrides.insert("ec20-PINNED".to_string(), "2402:8100::1".to_string());

        let needed = lines_needing_priming(&config, &card_ids, &overrides);

        assert_eq!(
            needed,
            ["ec20-UNCONFIGURED"]
                .into_iter()
                .map(String::from)
                .collect()
        );

        std::fs::remove_file(&primed_cache).ok();
    }

    /// A fleet-topology change (research.md R1) must never make a
    /// still-valid line reappear as needing priming just because a
    /// *different* card_id was added to the manifest — the check is keyed
    /// by `card_id`, not position.
    #[test]
    fn adding_a_new_line_does_not_disturb_an_already_primed_ones_status() {
        let base = std::env::temp_dir()
            .join(format!(
                "lines-needing-priming-topology-{}",
                std::process::id()
            ))
            .to_string_lossy()
            .to_string();
        let config = test_config_with_pcscf_base(&base);
        let existing_cache = crate::volte::pcscf::per_line_cache_path(&base, "ec20-EXISTING");
        std::fs::write(&existing_cache, "2402:8100::9\n").unwrap();

        let before =
            lines_needing_priming(&config, &["ec20-EXISTING".to_string()], &Default::default());
        assert!(before.is_empty());

        // A new modem (`ec20-AAAAAA`, sorting before `ec20-EXISTING`) joins
        // the fleet — same config, same cache, one more card_id.
        let after = lines_needing_priming(
            &config,
            &["ec20-AAAAAA".to_string(), "ec20-EXISTING".to_string()],
            &Default::default(),
        );

        assert_eq!(
            after,
            ["ec20-AAAAAA"].into_iter().map(String::from).collect()
        );

        std::fs::remove_file(&existing_cache).ok();
    }

    /// specs/081-multi-carrier-pcscf User Story 2 scenario 1: a redeploy
    /// that wipes every line's cache in a mixed-carrier fleet must bring
    /// every line back into the needed set, exactly like first-time setup —
    /// no special-casing for "this used to work."
    #[test]
    fn every_line_needs_repriming_after_its_cache_is_wiped() {
        let base = std::env::temp_dir()
            .join(format!(
                "lines-needing-priming-wiped-{}",
                std::process::id()
            ))
            .to_string_lossy()
            .to_string();
        let config = test_config_with_pcscf_base(&base);
        let card_ids = vec!["ec20-AAAAAA".to_string(), "ec20-BBBBBB".to_string()];
        let cache_a = crate::volte::pcscf::per_line_cache_path(&base, "ec20-AAAAAA");
        let cache_b = crate::volte::pcscf::per_line_cache_path(&base, "ec20-BBBBBB");
        std::fs::write(&cache_a, "2402:8100::1\n").unwrap();
        std::fs::write(&cache_b, "2402:8100::2\n").unwrap();

        assert!(lines_needing_priming(&config, &card_ids, &Default::default()).is_empty());

        // Simulate a redeploy that wipes /tmp.
        std::fs::remove_file(&cache_a).ok();
        std::fs::remove_file(&cache_b).ok();

        let needed = lines_needing_priming(&config, &card_ids, &Default::default());

        assert_eq!(
            needed,
            ["ec20-AAAAAA", "ec20-BBBBBB"]
                .into_iter()
                .map(String::from)
                .collect()
        );
    }
}
