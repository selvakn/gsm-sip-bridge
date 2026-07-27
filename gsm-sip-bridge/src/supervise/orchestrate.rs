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
use crate::config::AppConfig;
use crate::vowifi::discovery::LineResolutionEntry;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

    let started = Arc::new(Mutex::new(StartedState::default()));

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
        std::thread::spawn(move || loop {
            match runner.spawn(ChildSpec::new([bin.as_str(), "--config", cfg.as_str()])) {
                Ok(handle) => {
                    started.lock().unwrap().daemon_supervisor = Some(handle);
                    let status = runner.wait(handle);
                    println!(
                        "[supervise] gsm-sip-bridge daemon exited (status {status:?}); restarting in 5s"
                    );
                }
                Err(e) => eprintln!("[supervise] failed to spawn daemon: {e}"),
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
                vpcd::write_vpcd_reader_conf(runner.as_ref(), config.vowifi.vpcd_port);
                let Some(pcscd_handle) = vpcd::spawn_pcscd(runner.as_ref()) else {
                    eprintln!("[supervise] FATAL: failed to start pcscd");
                    return ExitCode::FAILURE;
                };
                started.lock().unwrap().pcscd = Some(pcscd_handle);
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
            }

            for line in &vowifi_lines {
                let runner = Arc::clone(&runner);
                let bin = bin.clone();
                let cfg = config_path_str.clone();
                let started = Arc::clone(&started);
                let config = config.clone();
                let line = line.clone();
                std::thread::spawn(move || {
                    start_vowifi_line(&runner, &bin, &cfg, &config, &line, &started);
                });
            }

            // Agent B: one shared process for every line's veth pair.
            {
                let runner = Arc::clone(&runner);
                let bin = bin.clone();
                let cfg = config_path_str.clone();
                let started = Arc::clone(&started);
                std::thread::spawn(move || loop {
                    match runner.spawn(ChildSpec::new([
                        bin.as_str(),
                        "--config",
                        cfg.as_str(),
                        "vowifi-sip-agent",
                    ])) {
                        Ok(handle) => {
                            started.lock().unwrap().sip_agent_supervisor = Some(handle);
                            let status = runner.wait(handle);
                            println!(
                                "[supervise] vowifi-sip-agent exited (status {status:?}); restarting in 5s"
                            );
                        }
                        Err(e) => eprintln!("[supervise] failed to spawn vowifi-sip-agent: {e}"),
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
    let (shutdown_tx, _rx) = crate::runtime::shutdown_channel();
    rt.block_on(crate::runtime::wait_for_shutdown(shutdown_tx));

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

/// One VoWiFi line's full startup — 1:1 port of `start_line_strongswan`/
/// `start_line_swu`'s shared prelude (modem presence, IMS mode reconcile,
/// mcc/mnc derivation), then dispatches to the engine-specific rest.
fn start_vowifi_line(
    runner: &Arc<dyn CommandRunner>,
    bin: &str,
    config_path: &str,
    config: &AppConfig,
    line: &LineResolutionEntry,
    started: &Arc<Mutex<StartedState>>,
) {
    let idx = line.index;
    let modem = line.modem_port.clone();
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

    let Some((mcc, mnc)) = resolve_mcc_mnc(runner.as_ref(), bin, line) else {
        eprintln!("[supervise] line {idx}: FATAL: could not derive MCC/MNC; skipping this line");
        return;
    };

    if config.vowifi.tunnel_engine == "strongswan" {
        start_vowifi_line_strongswan(runner, bin, config_path, config, line, &mcc, &mnc, started);
    } else {
        start_vowifi_line_swu(runner, bin, config_path, config, line, &mcc, &mnc, started);
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
    // never blocks this line's establish/steady-state loops below).
    let usim_holder: Arc<Mutex<Option<ChildHandle>>> = Arc::new(Mutex::new(None));
    {
        let runner = Arc::clone(runner);
        let bin = bin.to_string();
        let config_path = config_path.to_string();
        let modem = modem.clone();
        let vpcd_host = config.vowifi.vpcd_host.clone();
        let vpcd_port = line.vpcd_port;
        let usim_holder = Arc::clone(&usim_holder);
        let started = Arc::clone(started);
        std::thread::spawn(move || loop {
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
                    let status = runner.wait(h);
                    println!("[supervise] line {idx}: vowifi-usim-bridge exited (status {status:?}); restarting in 5s");
                }
                Err(e) => {
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
    );

    // Steady-state supervision loop — runs for the container's lifetime.
    let mut current_pcscf = pcscf;
    loop {
        runner.sleep(line_supervisor::STEADY_STATE_POLL_INTERVAL);
        match line_supervisor::tick_steady_state(&engine, runner.as_ref(), &current_pcscf) {
            line_supervisor::SteadyOutcome::StillUp => {}
            line_supervisor::SteadyOutcome::PcscfChanged { new_pcscf } => {
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
        std::thread::spawn(move || {
            let mut csim_fails = sim_recovery::IncidentCounters::default();
            let agent_log = PathBuf::from(format!("/tmp/ims-agent-{idx}.out"));
            loop {
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
                    eprintln!("[supervise] line {idx}: failed to spawn vowifi-ims-agent");
                    runner.sleep(Duration::from_secs(5));
                    continue;
                };
                started.lock().unwrap().vowifi_child_handles.push(handle);
                let status = runner.wait(handle);
                println!("[supervise] line {idx}: vowifi-ims-agent exited (status {status:?}); restarting in 5s");

                let log_content = runner.read_file(&agent_log).unwrap_or_default();
                let outcome = if sim_recovery::has_csim_failure(&log_content) {
                    sim_recovery::AgentExitOutcome::CsimFailure
                } else {
                    sim_recovery::AgentExitOutcome::Other
                };
                if csim_fails.observe(outcome) == sim_recovery::Action::ResetSim {
                    let holder = *usim_holder.lock().unwrap();
                    let reset_log = PathBuf::from(format!("/tmp/sim-reset-{idx}.log"));
                    sim_recovery::reset_modem_sim(
                        runner.as_ref(),
                        Path::new(&modem),
                        &reset_log,
                        holder,
                    );
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
    engine.restart_process(runner.as_ref()); // first spawn, same path as later respawns
    if let Some(h) = *engine.dialer_handle.borrow() {
        started.lock().unwrap().vowifi_child_handles.push(h);
    } else {
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
    );

    let mut current_pcscf = pcscf;
    loop {
        runner.sleep(Duration::from_secs(5));
        match line_supervisor::tick_steady_state(&engine, runner.as_ref(), &current_pcscf) {
            line_supervisor::SteadyOutcome::StillUp => {}
            line_supervisor::SteadyOutcome::PcscfChanged { new_pcscf } => {
                current_pcscf = new_pcscf;
            }
            line_supervisor::SteadyOutcome::Recovered { .. } => {
                if let Some(h) = *engine.dialer_handle.borrow() {
                    started.lock().unwrap().vowifi_child_handles.push(h);
                }
                let _ = runner.run(&["pkill", "-f", &format!("vowifi-ims-agent --line {idx}$")]);
            }
        }
    }
}
