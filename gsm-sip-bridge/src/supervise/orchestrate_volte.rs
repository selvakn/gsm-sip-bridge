//! Host-side IMS over LTE orchestration (specs/021-entrypoint-supervise-rust
//! Phase 4) — 1:1 port of `docker/entrypoint.sh`'s VoLTE section. Not
//! exercised against real hardware this session ([volte].enabled = false in
//! the deployment this branch was validated against — see quickstart.md /
//! DECISIONS-LOG.md); ported with the same care as the VoWiFi path and
//! covered by the same `cargo test --workspace` gate, but flagging that this
//! specific path's live-validation is still outstanding.

use super::runner::{ChildSpec, CommandRunner};
use super::shutdown::{LegacyVolteRegistration, StartedState, StartedVolteLine};
use crate::config::AppConfig;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const VOLTE_RESTORE_CID_PATH: &str = "/run/volte-restore-cid";

/// Entry point, called from `orchestrate::run` when `[volte].enabled`.
pub fn start(
    runner: Arc<dyn CommandRunner>,
    bin: String,
    config_path: String,
    config: AppConfig,
    started: Arc<Mutex<StartedState>>,
) {
    if config.volte.bridge_inbound {
        start_multiline(runner, bin, config_path, config, started);
    } else {
        start_legacy_registration(runner, bin, config_path, config, started);
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
) {
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

    for line in &manifest.lines {
        let runner = Arc::clone(&runner);
        let bin = bin.clone();
        let config_path = config_path.clone();
        let started = Arc::clone(&started);
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
            });
        }
        drop(state);

        println!("[supervise] volte line {idx}: starting volte-carrier-agent (netns {netns}), supervised...");
        std::thread::spawn(move || loop {
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
                    let mut state = started.lock().unwrap();
                    if let Some(entry) = state.volte_lines.iter_mut().find(|l| l.index == idx) {
                        entry.carrier_agent_handles.push(handle);
                    }
                    drop(state);
                    // Poll is_alive() rather than block on wait(): a real
                    // Greptile finding (mirroring the one already fixed on
                    // the vowifi-usim-bridge holder — see runner.rs) caught
                    // that RealCommandRunner::wait() removes the handle from
                    // the tracked table BEFORE blocking, which silently
                    // discards the shutdown plan's later `KillChild` signal
                    // to this exact handle (stored in
                    // `carrier_agent_handles` for that purpose) for the
                    // process's entire lifetime.
                    while runner.is_alive(handle) {
                        runner.sleep(Duration::from_secs(1));
                    }
                    println!("[supervise] volte line {idx}: volte-carrier-agent exited; restarting in 15s");
                }
                Err(e) => eprintln!(
                    "[supervise] volte line {idx}: failed to spawn volte-carrier-agent: {e}"
                ),
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
        match runner.spawn(ChildSpec::new([
            bin.as_str(),
            "--config",
            config_path.as_str(),
            "volte-bridge",
        ])) {
            Ok(handle) => {
                started.lock().unwrap().volte_bridge_supervisor = Some(handle);
                // See the volte-carrier-agent loop above: poll is_alive(),
                // don't block on wait(), so this handle (which the shutdown
                // plan signals via `volte_bridge_supervisor`) stays
                // signalable for as long as the process is actually alive.
                while runner.is_alive(handle) {
                    runner.sleep(Duration::from_secs(1));
                }
                println!("[supervise] volte-bridge exited; restarting in 15s");
            }
            Err(e) => eprintln!("[supervise] failed to spawn volte-bridge: {e}"),
        }
        runner.sleep(Duration::from_secs(15));
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
) {
    println!("[supervise] [volte].enabled — starting host-side IMS over LTE (resolving one line from config)");
    std::thread::spawn(move || loop {
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
                let mut state = started.lock().unwrap();
                state.legacy_volte_registration = Some(LegacyVolteRegistration {
                    supervisor_handle: handle,
                    bridge_inbound: false,
                    restore_cid: std::fs::read_to_string(VOLTE_RESTORE_CID_PATH).ok(),
                });
                drop(state);
                // See the volte-carrier-agent loop above: poll is_alive(),
                // don't block on wait(), so this handle (which the shutdown
                // plan signals via `legacy_volte_registration.
                // supervisor_handle`, then polls with `WaitForExit`) stays
                // signalable for as long as the process is actually alive.
                while runner.is_alive(handle) {
                    runner.sleep(Duration::from_secs(1));
                }
                println!("[supervise] the LTE IMS service exited; restarting in 15s");
            }
            Err(e) => eprintln!("[supervise] failed to spawn volte-register: {e}"),
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
