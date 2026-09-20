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

/// Resolves the one modem `[volte]` itself would pick as its own line 0
/// (`crate::volte::discovery::resolve_volte_lines` — the exact selection
/// `volte-discover-lines` runs), then builds the VoWiFi-shaped
/// [`LineResolutionEntry`] priming needs for *that* modem, ignoring
/// `[cs].enabled` (Greptile PR #87 review, finding 1).
///
/// Reusing VoWiFi's own resolver un-modified (`resolve_vowifi_lines`, the
/// original approach) picks whichever modem *it* considers eligible, which is
/// a materially different candidate set than VoLTE's: `[cs].enabled` (true by
/// default) reserves every unpinned audio-capable modem for the
/// circuit-switched pool, excluding it from VoWiFi's pool entirely — so on
/// the single most common deployment shape (one audio-capable modem,
/// `[cs].enabled` left at its default), VoWiFi's resolver reports zero
/// candidates while VoLTE's own would happily use that exact modem, and
/// priming retried forever. On a mixed-modem system it could instead capture
/// a P-CSCF for a *different* SIM/carrier than the one VoLTE is actually
/// registering. Forcing `cs_enabled = false` for this one-off resolution is
/// safe: priming's tunnel is transient and fully torn down (`tear_down`,
/// below) before any real line — VoWiFi or circuit-switched — starts, so it
/// never actually contends with a real reservation.
///
/// Calls `commands::discover::scan_for_line_resolution`/
/// `resolve_vowifi_lines_from_modems` directly, in-process — **not** the
/// `discover` subcommand, and deliberately not through `CommandRunner` at
/// all. Confirmed live on the Vodafone rig (2026-09-17): the `discover`
/// subcommand's own `[vowifi].enabled` gate (in `handle_discover_command`,
/// not in the resolution logic itself) means it always reports zero lines
/// whenever `[vowifi].enabled` is false — which is *always* true here, per
/// the mutual-exclusion guarantee this module's own doc comment describes.
/// Going through the subcommand can therefore never work for priming; the
/// underlying resolver has to be called directly, bypassing that gate.
fn discover_priming_line(config: &AppConfig) -> Result<LineResolutionEntry, String> {
    let modems = crate::commands::discover::scan_for_line_resolution(config)
        .map_err(|e| format!("priming: {e}"))?;

    let volte_line = crate::volte::discovery::resolve_volte_lines(&modems, &config.volte)
        .lines
        .into_iter()
        .next()
        .ok_or_else(|| {
            "priming: no usable modem/SIM found for VoLTE (no AT-capable modem with a ready SIM)"
                .to_string()
        })?;

    let resolution =
        crate::commands::discover::resolve_vowifi_lines_from_modems(&modems, config, false);
    resolution
        .lines
        .into_iter()
        .find(|l| l.card_id == volte_line.card_id)
        .ok_or_else(|| {
            format!(
                "priming: could not prepare {} (the modem VoLTE selected as its own line) as a \
                 VoWiFi-shaped line for capture — see the discovery errors above",
                volte_line.card_id
            )
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
    real_shutting_down: &Arc<RwLock<bool>>,
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

    let line = discover_priming_line(config)?;

    prime_with_line(runner, bin, config_path, config, &line, real_shutting_down)
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
    real_shutting_down: &Arc<RwLock<bool>>,
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
        real_shutting_down: Some(real_shutting_down),
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

        let real_shutting_down = Arc::new(RwLock::new(false));
        let err = prime_pcscf(
            runner,
            "gsm-sip-bridge",
            "/tmp/cfg.toml",
            &config,
            &real_shutting_down,
        )
        .unwrap_err();

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
        let real_shutting_down = Arc::new(RwLock::new(false));

        let err = prime_with_line(
            runner,
            "gsm-sip-bridge",
            "/tmp/cfg.toml",
            &config,
            &line,
            &real_shutting_down,
        )
        .unwrap_err();

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

    /// Greptile PR #87 review, finding 2: a real (container-wide) shutdown
    /// that lands while a priming attempt is polling for its tunnel must not
    /// be left to run out its own several-minute ceiling — it must abandon
    /// the attempt within one poll interval and still run its own local
    /// teardown, exactly like any other establish failure.
    #[test]
    fn a_real_shutdown_mid_establish_abandons_the_attempt_and_still_tears_down() {
        let mock = Arc::new(MockCommandRunner::new());
        let runner: Arc<dyn CommandRunner> = mock.clone();
        let mut config = test_config();
        config.volte.pcscf_source_path = std::env::temp_dir()
            .join("pcscf-prime-test-real-shutdown")
            .to_string_lossy()
            .to_string();
        let line = priming_line();
        // Already true before the attempt starts — the establish loop must
        // check this before its very first `tick_establishing`, not just
        // between sleeps, so this test never needs to drive a real
        // multi-iteration poll.
        let real_shutting_down = Arc::new(RwLock::new(true));

        let err = prime_with_line(
            runner,
            "gsm-sip-bridge",
            "/tmp/cfg.toml",
            &config,
            &line,
            &real_shutting_down,
        )
        .unwrap_err();

        assert!(err.contains("priming"), "got: {err}");
        assert!(
            mock.children
                .lock()
                .unwrap()
                .values()
                .any(|c| !c.signals_received.is_empty()),
            "teardown must still run for an attempt abandoned due to real shutdown"
        );
        assert!(!std::path::Path::new(&config.volte.pcscf_source_path).exists());
    }

    // `prime_pcscf` itself (as opposed to `prime_with_line`, tested above)
    // is deliberately NOT unit tested here: `discover_priming_line` calls
    // the real modem scanner directly, not through `CommandRunner` (see its
    // own doc comment for why), so `prime_pcscf`'s behavior legitimately
    // depends on whatever hardware is actually attached to the machine
    // running the test — confirmed live on the Vodafone rig (2026-09-17),
    // where an earlier version of this test that assumed "no modem present"
    // instead found the real modem and proceeded to (mock-)spawn pcscd,
    // failing the assertion that nothing gets spawned. That is exactly the
    // "hardware not available in CI" situation the constitution's mocking
    // carve-out exists for, in reverse: hardware *is* available here, so a
    // test asserting its absence is not a fact about this code, it's a fact
    // about this machine. `ensure_pcscf_primed_with` in `orchestrate_volte.rs`
    // is where the decision logic around `prime_pcscf` is actually tested,
    // with the call itself injected.
}
