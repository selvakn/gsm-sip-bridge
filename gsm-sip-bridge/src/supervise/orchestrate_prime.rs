//! Transient VoWiFi capture used to prime `[volte].pcscf_source_path`
//! (specs/080-volte-pcscf-auto-prime) — the in-process replacement for
//! docs/operations.md's manual "VoWiFi priming dance" (enable `[vowifi]`,
//! restart, confirm a capture file appeared, flip back to `[volte]`,
//! restart again).
//!
//! [`prime_pcscf`] borrows exactly one line from `discover`'s existing
//! modem-discovery output, brings its ePDG tunnel up far enough to receive a
//! P-CSCF from the IKE_AUTH config payload — reusing
//! [`super::orchestrate::prepare_vowifi_line`] and
//! [`super::orchestrate::establish_line_tunnel`] unchanged, the exact
//! sequence a real, persistent `[vowifi]` line already uses on real
//! hardware — writes the address to `[volte].pcscf_source_path`, and tears
//! the transient line all the way back down using the same, already-
//! hardware-exercised [`shutdown::build_shutdown_plan`] /
//! [`shutdown::execute_shutdown_plan`] the container's own shutdown uses,
//! scoped to a [`StartedState`] containing only this one line.
//!
//! Never runs when `[vowifi].enabled` is persistently true — `orchestrate`'s
//! own mutual-exclusion FATAL check already guarantees that whenever
//! `[volte].enabled` is being started at all, `[vowifi]` is not (see
//! `orchestrate::run`).
//!
//! This is the one thing in this feature that has not run against real
//! hardware: see specs/080-volte-pcscf-auto-prime/quickstart.md's final
//! section for exactly what still needs a live-hardware pass before this is
//! fully trusted in production.

use super::engines::SharedCharon;
use super::orchestrate::{
    establish_line_tunnel, prepare_vowifi_line, LineStartup, PCSCF_PLUGIN_CONF, SHARED_CHARON_LOG,
    SHARED_STRONGSWAN_CONF, SHARED_SWANCTL_CONF, SHARED_SWANCTL_CONF_DIR, SHARED_VICI_SOCKET,
};
use super::runner::CommandRunner;
use super::shutdown::{self, StartedState, TeardownBudget};
use super::{epdg_iface, vpcd};
use crate::config::AppConfig;
use crate::vowifi::discovery::LineResolutionEntry;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

/// Bounds the establish-time loop for a priming attempt (research.md R3):
/// unlike a real persistent line, priming is a one-shot action inside an
/// operator-watched startup sequence and must not block it indefinitely on
/// a single stuck attempt. `ESTABLISH_POLL_INTERVAL` is 2s, so this is a
/// ~4-minute ceiling — generous enough for a slow ePDG negotiation, bounded
/// enough that a caller's own retry cadence (FR-008) gets a turn instead.
const MAX_ESTABLISH_ATTEMPTS: u32 = 120;

/// Runs `discover` and returns its first line, if any — priming always uses
/// the first discovered line (FR-002b), matching `[volte].pcscf_source_path`
/// already being one shared value for every VoLTE line regardless of which
/// modem produced it (research.md R5).
fn discover_priming_line(
    runner: &dyn CommandRunner,
    bin: &str,
    config_path: &str,
) -> Result<LineResolutionEntry, String> {
    match runner.run(&[bin, "--config", config_path, "discover"]) {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            return Err(format!(
                "priming: 'discover' exited with {:?}",
                o.status.code()
            ))
        }
        Err(e) => return Err(format!("priming: could not run 'discover': {e}")),
    }
    let lines_file = crate::modules::discovery::lines_file_path();
    let resolution = crate::vowifi::discovery::read_line_resolution(&lines_file)
        .map_err(|e| format!("priming: could not read line resolution: {e}"))?;
    resolution.lines.into_iter().next().ok_or_else(|| {
        "priming: no usable modem/SIM found (no AT-capable modem with a ready SIM, or all \
         candidates are already serving the circuit-switched bridge)"
            .to_string()
    })
}

/// Renders the one-line equivalent of `start_vowifi_subsystem`'s shared
/// charon assets: `PCSCF_PLUGIN_CONF` must list this line's connection name
/// *before* charon starts (`PCSCF_PLUGIN_CONF`'s own doc comment), and the
/// swanctl top conf must point at the directory `establish_line_tunnel`
/// writes this line's connection file into.
fn render_shared_charon_assets(runner: &dyn CommandRunner, line: &LineResolutionEntry) {
    let _ = runner.write_file(
        Path::new(SHARED_STRONGSWAN_CONF),
        &super::render::render_strongswan_conf(SHARED_VICI_SOCKET, SHARED_CHARON_LOG),
    );
    let _ = runner.run(&["mkdir", "-p", SHARED_SWANCTL_CONF_DIR]);
    let _ = runner.run(&[
        "sh",
        "-c",
        &format!("rm -f {SHARED_SWANCTL_CONF_DIR}/*.conf"),
    ]);
    let _ = runner.write_file(
        Path::new(SHARED_SWANCTL_CONF),
        &super::render::render_swanctl_top_conf(SHARED_SWANCTL_CONF_DIR),
    );
    let conn_name = format!("ims{}", line.index);
    let _ = runner.write_file(
        Path::new(PCSCF_PLUGIN_CONF),
        &super::render::render_pcscf_plugin_conf(&[conn_name]),
    );
}

/// Tears down exactly what this priming attempt started, using the same
/// machinery the container's own shutdown uses (research.md R3) — never new
/// teardown code. Best-effort: called on both the success and failure
/// paths, so whatever partially started before a failure is still cleaned
/// up.
fn tear_down(runner: &dyn CommandRunner, started: &Arc<Mutex<StartedState>>, config_path: &str) {
    let snapshot = started.lock().unwrap().clone();
    let steps = shutdown::build_shutdown_plan(&snapshot, config_path);
    let _ = shutdown::execute_shutdown_plan(&steps, runner, &TeardownBudget::unbounded());
}

/// Runs one priming attempt end to end: discover a line, bring its tunnel
/// up, write the captured address to `[volte].pcscf_source_path`, tear the
/// line back down. Callers (see `orchestrate_volte`) supply their own retry
/// cadence — this function makes exactly one attempt and returns.
pub fn prime_pcscf(
    runner: Arc<dyn CommandRunner>,
    bin: &str,
    config_path: &str,
    config: &AppConfig,
) -> Result<(), String> {
    if config.vowifi.tunnel_engine != "strongswan" {
        return Err(format!(
            "priming requires [vowifi].tunnel_engine = \"strongswan\" (the default) to capture \
             a P-CSCF; this deployment configures {:?}, which priming does not support — supply \
             an explicit [[volte.line]].pcscf, or run the manual VoWiFi dance once \
             (docs/operations.md)",
            config.vowifi.tunnel_engine
        ));
    }

    let line = discover_priming_line(runner.as_ref(), bin, config_path)?;

    prime_with_line(runner, bin, config_path, config, &line)
}

/// The rest of one priming attempt, given an already-resolved line —
/// separated from `prime_pcscf` so it is directly testable the same way
/// every other per-line function in `orchestrate.rs` already is: by handing
/// it a `LineResolutionEntry` built by the test, bypassing the `discover`
/// subprocess and its real (non-`CommandRunner`-mediated) lines file.
pub(super) fn prime_with_line(
    runner: Arc<dyn CommandRunner>,
    bin: &str,
    config_path: &str,
    config: &AppConfig,
    line: &LineResolutionEntry,
) -> Result<(), String> {
    // Local to this one attempt — never the real, container-wide
    // StartedState/shutting-down flag. Setting the real flag would begin a
    // full container shutdown; a priming attempt must never do that, and
    // must never be visible to the real shutdown plan either (it tears
    // itself down synchronously, below, well before this function returns).
    let started = Arc::new(Mutex::new(StartedState::default()));
    let shutting_down = Arc::new(RwLock::new(false));

    // Same reasoning as `start_vowifi_subsystem`'s own reclaim step: a
    // previous priming attempt killed mid-flight (e.g. the whole `supervise`
    // process was itself killed) can leave this if_id/netns/veth claimed on
    // the host. Reclaim them before creating anything of our own.
    let mut our_if_ids = std::collections::BTreeSet::new();
    our_if_ids.insert(line.strongswan_if_id);
    epdg_iface::reclaim_stale_xfrm(runner.as_ref(), &our_if_ids);
    epdg_iface::reclaim_leftover_lines(
        runner.as_ref(),
        &[epdg_iface::ReclaimCandidate {
            netns: line.netns.clone(),
            tun_iface: Some(line.strongswan_tun_iface.clone()),
            veth_host: Some(line.config.veth_sip_iface.clone()),
            owned_iface_marker: Some(line.strongswan_tun_iface.clone()),
        }],
        epdg_iface::reclaim_leftover_enabled(),
    );

    let needs_vpcd = !line.pcsc_reader;
    if needs_vpcd {
        vpcd::write_vpcd_reader_conf(runner.as_ref(), line.vpcd_port);
    }
    let pcscd_handle = match vpcd::start_pcscd_with_retries(
        runner.as_ref(),
        needs_vpcd,
        &config.vowifi.vpcd_host,
        line.vpcd_port,
    ) {
        Ok(h) => Arc::new(h),
        Err(e) => {
            return Err(format!("priming: pcscd/vpcd did not become ready: {e:?}"));
        }
    };
    started.lock().unwrap().pcscd = Some(pcscd_handle);

    render_shared_charon_assets(runner.as_ref(), line);

    let shared_charon = Arc::new(SharedCharon::new(
        SHARED_STRONGSWAN_CONF.to_string(),
        SHARED_SWANCTL_CONF.to_string(),
        std::path::PathBuf::from(SHARED_CHARON_LOG),
    ));

    let ctx = LineStartup {
        runner: &runner,
        bin,
        config_path,
        config,
        started: &started,
        shutting_down: &shutting_down,
        alert_ctx: None,
        shared_charon: &shared_charon,
    };

    let Some((mcc, mnc)) = prepare_vowifi_line(&ctx, line) else {
        tear_down(runner.as_ref(), &started, config_path);
        return Err(
            "priming: could not prepare the discovered line (modem/PLMN issue — see the \
                     error above)"
                .to_string(),
        );
    };

    let result = establish_line_tunnel(&ctx, line, &mcc, &mnc, Some(MAX_ESTABLISH_ATTEMPTS));

    // Stop this attempt's own background threads (the USIM bridge's retry
    // loop) *before* tearing down: `tear_down`'s `KillChild` step only stops
    // the current process — without this, that loop would just spawn a
    // replacement a few seconds later, right as we delete the netns it
    // needs. This is the local, attempt-scoped flag, never the real one.
    *shutting_down.write().unwrap() = true;

    let outcome = match result {
        Some((pcscf, _usim_holder)) => {
            let write_result = std::fs::write(&config.volte.pcscf_source_path, &pcscf)
                .map_err(|e| format!("priming: captured {pcscf} but could not write it: {e}"));
            match &write_result {
                Ok(()) => println!(
                    "[supervise] priming: captured P-CSCF {pcscf}, wrote it to {}",
                    config.volte.pcscf_source_path
                ),
                Err(e) => eprintln!("[supervise] priming: {e}"),
            }
            write_result
        }
        None => Err(
            "priming: the tunnel did not establish (see the error above for which step failed)"
                .to_string(),
        ),
    };

    tear_down(runner.as_ref(), &started, config_path);

    outcome
}

#[cfg(test)]
mod tests {
    use super::super::runner::MockCommandRunner;
    use super::*;
    use crate::config::AppConfig;

    fn test_config() -> AppConfig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[sip]\nserver = \"sip.example.com\"\nusername = \"user\"\npassword = \"pass\"\n",
        )
        .unwrap();
        let mut config = crate::config::load_config(&path).unwrap();
        config.vowifi.epdg_ip = Some("192.0.2.1".to_string());
        config
    }

    #[test]
    fn refuses_the_swu_engine_before_touching_anything() {
        let mut config = test_config();
        config.vowifi.tunnel_engine = "swu".to_string();
        let mock = Arc::new(MockCommandRunner::new());
        let runner: Arc<dyn CommandRunner> = mock.clone();

        let err = prime_pcscf(runner, "gsm-sip-bridge", "/tmp/cfg.toml", &config).unwrap_err();

        assert!(err.contains("strongswan"), "got: {err}");
        assert!(
            mock.run_calls.lock().unwrap().is_empty(),
            "must bail before even running 'discover'"
        );
    }

    /// A minimal, otherwise-viable line — mirrors the `LineResolutionEntry`
    /// literals `orchestrate.rs`'s own tests already build to exercise
    /// `start_vowifi_line_strongswan` directly, bypassing `discover`.
    fn priming_line() -> LineResolutionEntry {
        LineResolutionEntry {
            index: 0,
            card_id: "card0".to_string(),
            modem_port: "/dev/ttyUSB2".to_string(),
            netns: "ims".to_string(),
            control_port: 0,
            veth_local_addr: "169.254.10.2".to_string(),
            veth_peer_addr: "169.254.10.1".to_string(),
            vpcd_port: 35963,
            strongswan_if_id: 23,
            strongswan_tun_iface: "tun23".to_string(),
            pcscf_source_path: "/tmp/pcscf-prime-test".to_string(),
            mcc: "404".to_string(),
            mnc: "043".to_string(),
            pcsc_reader: false,
            configured_identifier: None,
            msisdn: None,
            config: crate::config::VowifiConfig::default(),
        }
    }

    #[test]
    fn establish_failure_still_tears_down_whatever_partially_started() {
        // Deterministic, fast failure (mirrors orchestrate.rs's own
        // convention — e.g. start_vowifi_line_strongswan_skips_usim_bridge_
        // only_for_pcsc — of forcing charon "born dead" rather than driving
        // a real establish to completion, which nothing in this codebase's
        // unit tests does; see quickstart.md's hardware-validation note).
        let mock = Arc::new(MockCommandRunner::new());
        mock.set_born_dead_if_argv_contains("charon");
        let runner: Arc<dyn CommandRunner> = mock.clone();
        let mut config = test_config();
        config.volte.pcscf_source_path = std::env::temp_dir()
            .join("pcscf-prime-test-out")
            .to_string_lossy()
            .to_string();
        let line = priming_line();

        let err =
            prime_with_line(runner, "gsm-sip-bridge", "/tmp/cfg.toml", &config, &line).unwrap_err();

        assert!(err.contains("priming"), "got: {err}");
        assert!(
            mock.children
                .lock()
                .unwrap()
                .values()
                .any(|c| !c.signals_received.is_empty()),
            "teardown must have signaled at least the pcscd child, even on a failed attempt"
        );
        assert!(
            !std::path::Path::new(&config.volte.pcscf_source_path).exists(),
            "a failed attempt must never write a cache file"
        );
    }

    #[test]
    fn fails_cleanly_when_discover_finds_no_line() {
        let mock = Arc::new(MockCommandRunner::new());
        // No `discover` output queued at all — the mock's default `run`
        // behavior for an un-stubbed argv is success with empty output, so
        // `read_line_resolution` on a nonexistent/empty lines file is what
        // actually drives this to "no usable modem/SIM found".
        let runner: Arc<dyn CommandRunner> = mock.clone();
        let config = test_config();

        let err = prime_pcscf(runner, "gsm-sip-bridge", "/tmp/cfg.toml", &config).unwrap_err();

        assert!(err.contains("priming"), "got: {err}");
        assert!(
            mock.spawn_specs.lock().unwrap().is_empty(),
            "no charon/pcscd/usim-bridge should ever be spawned when there is no line to prime"
        );
    }
}
