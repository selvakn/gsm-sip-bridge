//! Transient VoWiFi capture used to prime VoLTE's per-line P-CSCF caches
//! (specs/080-volte-pcscf-auto-prime, generalized to multiple, concurrently-
//! primed lines by specs/081-multi-carrier-pcscf) — the in-process
//! replacement for docs/operations.md's manual "VoWiFi priming dance"
//! (enable `[vowifi]`, restart, confirm a capture file appeared, flip back
//! to `[volte]`, restart again), now run for every VoLTE line that needs it
//! in one pass instead of only the first discovered line.
//!
//! [`prime_pass`] resolves every VoLTE line named in `needed_card_ids` from
//! `discover`'s modem-discovery output, brings all of their ePDG tunnels up
//! *concurrently*, sharing one charon instance the same way the real,
//! persistent multi-line VoWiFi subsystem
//! (`super::orchestrate::start_vowifi_subsystem`) already does for N
//! simultaneous real lines (specs/081-multi-carrier-pcscf research.md R3) —
//! reusing [`super::orchestrate::prepare_vowifi_line`] and
//! [`super::orchestrate::establish_line_tunnel`] unchanged, exactly as
//! specs/080's single-line version did — writes each line's captured
//! address to *that line's own* per-`card_id` cache
//! (`volte::pcscf::per_line_cache_path`, research.md R1), and tears every
//! transient line all the way back down using the same, already-hardware-
//! exercised [`shutdown::build_shutdown_plan`] / [`shutdown::
//! execute_shutdown_plan`] the container's own shutdown uses, scoped to a
//! [`StartedState`] containing only the lines this pass started.
//!
//! Never runs when `[vowifi].enabled` is persistently true — `orchestrate`'s
//! own mutual-exclusion FATAL check already guarantees that whenever
//! `[volte].enabled` is being started at all, `[vowifi]` is not (see
//! `orchestrate::run`).
//!
//! The multi-line concurrency this module adds has not run against real,
//! simultaneous multi-carrier hardware: see
//! specs/081-multi-carrier-pcscf/quickstart.md's "Real-hardware validation
//! still needed" section for exactly what still needs a live rig pass
//! before this is fully trusted in production.

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
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

/// One line's priming result: its `card_id` paired with the outcome.
type LineOutcome = (String, Result<(), String>);

/// Bounds the establish-time loop for a priming attempt (research.md R3):
/// unlike a real persistent line, priming is a one-shot action inside an
/// operator-watched startup sequence and must not block it indefinitely on
/// a single stuck attempt. `ESTABLISH_POLL_INTERVAL` is 2s, so this is a
/// ~4-minute ceiling — generous enough for a slow ePDG negotiation, bounded
/// enough that a caller's own retry cadence (FR-009) gets a turn instead.
const MAX_ESTABLISH_ATTEMPTS: u32 = 120;

/// Resolves the [`LineResolutionEntry`] for every VoLTE line named in
/// `needed_card_ids`, each at a distinct **pass-local** index
/// (specs/081-multi-carrier-pcscf research.md R4) — 0..N across only the
/// lines actually being primed in this call, unrelated to any line's real
/// VoLTE index or its `card_id`-keyed cache filename.
///
/// Resolves the *set* of VoLTE-usable modems the same way
/// `crate::volte::discovery::resolve_volte_lines` (the exact selection
/// `volte-discover-lines` runs) does, then builds each requested line's
/// VoWiFi-shaped [`LineResolutionEntry`] directly via
/// `crate::vowifi::discovery::resolve_single_line`, never through
/// `resolve_lines`'s `[vowifi].max_lines`/pin-priority membership tiers
/// (Greptile PR #87 review, findings 1 and its mixed-modem follow-up, from
/// specs/080's single-line version — the same reasoning applies per line
/// here).
///
/// Two separate reasons `resolve_lines` (VoWiFi's real, persistent-line
/// resolver) is the wrong tool here: `[cs].enabled` (true by default)
/// reserves every unpinned audio-capable modem for the circuit-switched
/// pool, so VoWiFi's own resolver can report zero candidates for a modem
/// VoLTE's own resolver would happily use; separately, a *different* modem
/// pinned to every available `[vowifi].max_lines` slot can exclude a
/// VoLTE-selected modem even with `[cs].enabled = false`.
/// `resolve_single_line` sidesteps both: it derives each modem's line
/// resources directly, with no budget or pin tier to lose to — safe because
/// every priming tunnel is transient and fully torn down (`tear_down`,
/// below) before any real line, VoWiFi or circuit-switched, starts, so it
/// never actually contends with a real reservation.
///
/// Calls `commands::discover::scan_for_line_resolution` directly, in-process
/// — **not** the `discover` subcommand, and deliberately not through
/// `CommandRunner` at all, for the same reason specs/080's version had to:
/// the `discover` subcommand's own `[vowifi].enabled` gate always reports
/// zero lines whenever `[vowifi].enabled` is false, which is *always* true
/// here (the mutual-exclusion guarantee this module's own doc comment
/// describes).
///
/// A `card_id` in `needed_card_ids` that no longer appears among currently-
/// discovered VoLTE lines (e.g. its modem vanished between the manifest scan
/// and this call) is reported back in the second element of the returned
/// tuple rather than as an error for the whole pass — the caller turns each
/// one into a distinct, logged failure outcome for that `card_id`, exactly
/// as it would for any other priming failure, so the line's own retry loop
/// keeps waiting for its cache to appear without the operator losing
/// visibility into why.
fn discover_priming_lines(
    config: &AppConfig,
    needed_card_ids: &BTreeSet<String>,
) -> Result<(Vec<LineResolutionEntry>, Vec<String>), String> {
    let modems = crate::commands::discover::scan_for_line_resolution(config)
        .map_err(|e| format!("priming: {e}"))?;

    let volte_lines = crate::volte::discovery::resolve_volte_lines(&modems, &config.volte).lines;

    let mut result = Vec::new();
    let mut found_card_ids = BTreeSet::new();
    let mut pass_index: u32 = 0;
    for volte_line in &volte_lines {
        if !needed_card_ids.contains(&volte_line.card_id) {
            continue;
        }
        found_card_ids.insert(volte_line.card_id.clone());
        let Some(modem) = modems.iter().find(|m| m.card_id == volte_line.card_id) else {
            return Err(format!(
                "priming: internal error — VoLTE selected modem {} but it is missing from the \
                 scan that just produced it",
                volte_line.card_id
            ));
        };
        result.push(crate::vowifi::discovery::resolve_single_line(
            modem,
            &config.vowifi,
            pass_index,
        ));
        pass_index += 1;
    }

    Ok((result, missing_card_ids(needed_card_ids, &found_card_ids)))
}

/// Every `card_id` present in `needed` but absent from `found` — pulled out
/// of `discover_priming_lines` as a pure function purely so this specific
/// piece of logic (which `card_id`s get their own reported failure instead
/// of vanishing silently) is unit-testable without going through the real,
/// hardware-dependent modem scan that function itself is not further tested
/// against (see this module's own test-module note on that).
fn missing_card_ids(needed: &BTreeSet<String>, found: &BTreeSet<String>) -> Vec<String> {
    needed
        .iter()
        .filter(|c| !found.contains(*c))
        .cloned()
        .collect()
}

/// Resolves the single line the legacy, single-line VoLTE path
/// (`orchestrate_volte::start_legacy_registration`, `[volte].bridge_inbound
/// = false`) primes — the exact same modem `discover_priming_line`
/// (specs/080-volte-pcscf-auto-prime's original, singular version) always
/// picked: VoLTE's own first-selected line, at pass-local index 0.
///
/// This path is unaffected by specs/081-multi-carrier-pcscf's per-`card_id`
/// caching (that only applies to the `bridge_inbound` manifest path) — it
/// keeps writing to the literal, unkeyed `[volte].pcscf_source_path`, so a
/// legacy single-line deployment's behavior is unchanged by this feature.
fn discover_first_volte_line(config: &AppConfig) -> Result<LineResolutionEntry, String> {
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

    let modem = modems
        .iter()
        .find(|m| m.card_id == volte_line.card_id)
        .ok_or_else(|| {
            format!(
                "priming: internal error — VoLTE selected modem {} but it is missing from the \
                 scan that just produced it",
                volte_line.card_id
            )
        })?;

    Ok(crate::vowifi::discovery::resolve_single_line(
        modem,
        &config.vowifi,
        0,
    ))
}

/// Renders the shared charon's assets for this pass's lines:
/// `PCSCF_PLUGIN_CONF` must list every line's connection name *before*
/// charon starts (`PCSCF_PLUGIN_CONF`'s own doc comment), and the swanctl
/// top conf must point at the directory `establish_line_tunnel` writes each
/// line's connection file into — the exact pattern
/// `start_vowifi_subsystem` already uses for N real, persistent lines
/// (research.md R3), scoped here to just the lines this pass is priming.
fn render_shared_charon_assets(runner: &dyn CommandRunner, targets: &[PrimeTarget]) {
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
    let conn_names: Vec<String> = targets
        .iter()
        .map(|t| format!("ims{}", t.line.index))
        .collect();
    let _ = runner.write_file(
        Path::new(PCSCF_PLUGIN_CONF),
        &super::render::render_pcscf_plugin_conf(&conn_names),
    );
}

/// One line to prime, paired with exactly where its captured address should
/// be written. Plain data rather than a callback so it stays trivially
/// `Send` across each line's own establish thread.
///
/// The multi-line pass (`prime_pass`) always pairs a line with its own
/// `card_id`-keyed path (`volte::pcscf::per_line_cache_path`, research.md
/// R1). The legacy single-line path (`prime_legacy_line`) pairs its one
/// line with the literal, unkeyed `[volte].pcscf_source_path` instead —
/// unaffected by this feature, matching its pre-081 behavior exactly.
struct PrimeTarget {
    line: LineResolutionEntry,
    cache_path: std::path::PathBuf,
}

/// Tears down exactly what this priming pass started, using the same
/// machinery the container's own shutdown uses (research.md R6) — never new
/// teardown code. Best-effort: called once, after every line in the pass
/// has concluded (success or failure), so whatever any of them partially
/// started is still cleaned up.
///
/// `started` is the real, container-wide `StartedState` (Greptile PR #87
/// review, finding 2's follow-up — carried from specs/080's single-line
/// version) — but the plan built below is deliberately scoped to a
/// *synthetic* `StartedState` containing only the four fields priming ever
/// populates, never a raw clone of the real one (Greptile PR #87 review,
/// sixth finding: "Priming Stops The Main Daemon" — the circuit-switched
/// daemon supervisor lives in that same shared struct, so a raw clone's
/// teardown plan would include a `KillChild` step for it on every priming
/// pass, successful or not).
///
/// `pcscd` and `vowifi_child_handles` are cleared unconditionally: both are
/// exclusively VoWiFi/ePDG-tunnel concepts, and the only VoWiFi-tunnel
/// activity possible while `[volte].enabled` is set is this pass's own
/// transient lines (`orchestrate::run`'s mutual-exclusion guarantee rules
/// out a concurrent *real, persistent* VoWiFi subsystem), so nothing else
/// can be writing into them concurrently.
///
/// `started_netns` and `vowifi_lines`, by contrast, are **not**
/// priming-exclusive: `orchestrate_volte`'s real per-line startup pushes a
/// live VoLTE line's own `netns` into that same `started_netns` Vec — and,
/// as of specs/081-multi-carrier-pcscf, that can now happen *concurrently*
/// with this pass, on `start_multiline`'s coordinator thread priming a
/// still-unprimed line while an already-primed line's own spawn loop is
/// simultaneously starting for real (Greptile PR #89 review, "Priming
/// Deletes Live Namespaces" — before this fix, a raw `.clone()`/`.clear()`
/// of the whole shared Vec would tear down and untrack a live line's
/// namespace right out from under it). `owned_netns` — every `netns` value
/// `targets` itself named, known upfront from the lines this specific pass
/// is priming — is used to filter both fields down to only this pass's own
/// entries, both when building the teardown plan and when removing them
/// from the real, shared state afterward (`retain`, never `clear`), so any
/// concurrently-added real line's entry is always left untouched.
fn tear_down(
    runner: &dyn CommandRunner,
    started: &Arc<Mutex<StartedState>>,
    config_path: &str,
    owned_netns: &BTreeSet<String>,
) {
    let scoped = {
        let real = started.lock().unwrap();
        StartedState {
            pcscd: real.pcscd.clone(),
            vowifi_child_handles: real.vowifi_child_handles.clone(),
            started_netns: real
                .started_netns
                .iter()
                .filter(|n| owned_netns.contains(*n))
                .cloned()
                .collect(),
            vowifi_lines: real
                .vowifi_lines
                .iter()
                .filter(|l| owned_netns.contains(&l.netns))
                .cloned()
                .collect(),
            ..StartedState::default()
        }
    };
    let steps = shutdown::build_shutdown_plan(&scoped, config_path);
    let _ = shutdown::execute_shutdown_plan(&steps, runner, &TeardownBudget::unbounded());

    let mut state = started.lock().unwrap();
    state.pcscd = None;
    state.vowifi_child_handles.clear();
    state.started_netns.retain(|n| !owned_netns.contains(n));
    state
        .vowifi_lines
        .retain(|l| !owned_netns.contains(&l.netns));
}

/// Runs one priming pass end to end for every `card_id` in `needed_card_ids`:
/// discover each line, bring every tunnel up *concurrently* sharing one
/// charon instance, write each success to that line's own per-`card_id`
/// cache, tear every line back down together. Callers (see
/// `orchestrate_volte`) supply their own retry cadence for whichever lines
/// come back `Err` — this function makes exactly one attempt per line and
/// returns.
///
/// Returns one `(card_id, outcome)` pair per `card_id` in `needed_card_ids`
/// — never a single pass/fail verdict for the whole pass, since one line's
/// failure must never obscure another's success (FR-005). A `card_id` that
/// could not be discovered at all still gets its own `Err` outcome (see
/// `discover_priming_lines`'s own doc comment), so every needed line is
/// accounted for and logged by the caller.
pub fn prime_pass(
    runner: Arc<dyn CommandRunner>,
    bin: &str,
    config_path: &str,
    config: &AppConfig,
    needed_card_ids: &BTreeSet<String>,
    started: &Arc<Mutex<StartedState>>,
    real_shutting_down: &Arc<RwLock<bool>>,
) -> Vec<LineOutcome> {
    if needed_card_ids.is_empty() {
        return Vec::new();
    }

    if config.vowifi.tunnel_engine != "strongswan" {
        let msg = format!(
            "priming requires [vowifi].tunnel_engine = \"strongswan\" (the default) to capture \
             a P-CSCF; this deployment configures {:?}, which priming does not support — supply \
             an explicit [[volte.line]].pcscf, or run the manual VoWiFi dance once \
             (docs/operations.md)",
            config.vowifi.tunnel_engine
        );
        return needed_card_ids
            .iter()
            .map(|c| (c.clone(), Err(msg.clone())))
            .collect();
    }

    let (lines, missing) = match discover_priming_lines(config, needed_card_ids) {
        Ok(v) => v,
        Err(e) => {
            return needed_card_ids
                .iter()
                .map(|c| (c.clone(), Err(e.clone())))
                .collect()
        }
    };
    let mut missing_outcomes: Vec<LineOutcome> = missing
        .into_iter()
        .map(|c| {
            let msg = format!(
                "priming: line {c}: needed but not found in this pass's VoLTE line selection \
                 (modem may have dropped out — will retry next pass)"
            );
            (c, Err(msg))
        })
        .collect();
    if lines.is_empty() {
        return missing_outcomes;
    }

    let targets: Vec<PrimeTarget> = lines
        .into_iter()
        .map(|line| {
            let cache_path = crate::volte::pcscf::per_line_cache_path(
                &config.volte.pcscf_source_path,
                &line.card_id,
            );
            PrimeTarget { line, cache_path }
        })
        .collect();

    let mut outcomes = prime_lines(
        runner,
        bin,
        config_path,
        config,
        &targets,
        started,
        real_shutting_down,
    );
    outcomes.append(&mut missing_outcomes);
    outcomes
}

/// Runs one priming attempt for the legacy, single-line VoLTE path
/// (`[volte].bridge_inbound = false`) — the pre-081 behavior, unaffected by
/// per-`card_id` caching: discovers VoLTE's own first-selected line
/// (`discover_first_volte_line`) and writes its captured address to the
/// literal, unkeyed `[volte].pcscf_source_path`, exactly as
/// specs/080-volte-pcscf-auto-prime's original `prime_pcscf` did.
pub fn prime_legacy_line(
    runner: Arc<dyn CommandRunner>,
    bin: &str,
    config_path: &str,
    config: &AppConfig,
    started: &Arc<Mutex<StartedState>>,
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

    let line = discover_first_volte_line(config)?;
    let target = PrimeTarget {
        line,
        cache_path: std::path::PathBuf::from(&config.volte.pcscf_source_path),
    };

    let outcomes = prime_lines(
        runner,
        bin,
        config_path,
        config,
        &[target],
        started,
        real_shutting_down,
    );
    outcomes
        .into_iter()
        .next()
        .map(|(_, outcome)| outcome)
        .unwrap_or_else(|| {
            Err("priming: internal error — no outcome for the legacy line".to_string())
        })
}

/// Turns a joined per-line thread's result into its `LineOutcome`, keeping
/// `card_id` attributed to the right line even when the thread panicked —
/// `card_id` is captured by the caller *before* the thread is spawned
/// specifically so it survives a panic inside the thread, rather than
/// falling back to an unattributable placeholder.
fn join_outcome(card_id: String, joined: std::thread::Result<LineOutcome>) -> LineOutcome {
    joined.unwrap_or_else(|_| {
        (
            card_id,
            Err("priming: a line's establish thread panicked".to_string()),
        )
    })
}

/// The rest of one priming pass, given already-resolved lines — separated
/// from `prime_pass` so it is directly testable the same way every other
/// per-line function in `orchestrate.rs` already is: by handing it
/// `LineResolutionEntry` values built by the test, bypassing the `discover`
/// subprocess and its real (non-`CommandRunner`-mediated) modem scan.
///
/// `started` is the real, container-wide `StartedState` — **not** a local,
/// throwaway one, for the same reason specs/080's single-line version
/// needed it: the real shutdown plan is built from this exact structure.
/// Safe to share, including across this pass's own several concurrent
/// lines: every line only ever *pushes* into the `Vec`-shaped fields it
/// touches (never overwrites), and [`tear_down`] removes only its own
/// pass's entries again afterward (never a blanket clear — see its own doc
/// comment). This is no longer "priming always runs before any real
/// VoWiFi/VoLTE line starts": `orchestrate::run`'s mutual-exclusion
/// guarantee still rules out a concurrent *real, persistent* VoWiFi
/// subsystem, but specs/081-multi-carrier-pcscf's concurrent coordinator
/// thread means a real VoLTE line can now be starting for real, on its own
/// thread, while this pass primes a *different* line at the same time —
/// `tear_down`'s per-pass scoping is what keeps that safe.
///
/// `shutting_down` is local to this pass (shared across every line in it,
/// unlike `started`): it exists purely to stop this pass's own transient
/// background threads (each line's USIM bridge retry loop) once every line
/// has concluded, a signal with no meaning to anything outside this
/// function — using the real flag for that would falsely tell the rest of
/// the process a full container shutdown was under way.
/// `real_shutting_down` (passed separately) is how every line actually
/// observes a genuine one, read-only, to abandon an in-flight establish
/// promptly instead of running out its own several-minute ceiling.
fn prime_lines(
    runner: Arc<dyn CommandRunner>,
    bin: &str,
    config_path: &str,
    config: &AppConfig,
    targets: &[PrimeTarget],
    started: &Arc<Mutex<StartedState>>,
    real_shutting_down: &Arc<RwLock<bool>>,
) -> Vec<LineOutcome> {
    // Same reasoning as `start_vowifi_subsystem`'s own reclaim step: a
    // previous pass killed mid-flight (e.g. the whole `supervise` process
    // was itself killed) can leave this pass's if_ids/netns/veths claimed
    // on the host. Reclaim them all, up front, before creating anything of
    // our own.
    let our_if_ids: BTreeSet<u32> = targets.iter().map(|t| t.line.strongswan_if_id).collect();
    epdg_iface::reclaim_stale_xfrm(runner.as_ref(), &our_if_ids);
    let reclaim_candidates: Vec<epdg_iface::ReclaimCandidate> = targets
        .iter()
        .map(|t| epdg_iface::ReclaimCandidate {
            netns: t.line.netns.clone(),
            tun_iface: Some(t.line.strongswan_tun_iface.clone()),
            veth_host: Some(t.line.config.veth_sip_iface.clone()),
            owned_iface_marker: Some(t.line.strongswan_tun_iface.clone()),
        })
        .collect();
    epdg_iface::reclaim_leftover_lines(
        runner.as_ref(),
        &reclaim_candidates,
        epdg_iface::reclaim_leftover_enabled(),
    );

    // One shared pcscd for the whole pass, matching `start_vowifi_subsystem`
    // exactly: `render_vpcd_reader_conf`'s one conf entry already serves up
    // to 8 slots from `[vowifi].vpcd_port` upward (its own doc comment), and
    // every priming line's own `vpcd_port` — derived from its pass-local
    // index (research.md R4) — falls within that range by construction.
    let needs_vpcd = targets.iter().any(|t| !t.line.pcsc_reader);
    if needs_vpcd {
        vpcd::write_vpcd_reader_conf(runner.as_ref(), config.vowifi.vpcd_port);
    }
    let pcscd_handle = match vpcd::start_pcscd_with_retries(
        runner.as_ref(),
        needs_vpcd,
        &config.vowifi.vpcd_host,
        config.vowifi.vpcd_port,
    ) {
        Ok(h) => Arc::new(h),
        Err(e) => {
            let msg = format!("priming: pcscd/vpcd did not become ready: {e:?}");
            return targets
                .iter()
                .map(|t| (t.line.card_id.clone(), Err(msg.clone())))
                .collect();
        }
    };
    started.lock().unwrap().pcscd = Some(pcscd_handle);

    render_shared_charon_assets(runner.as_ref(), targets);
    let shared_charon = Arc::new(SharedCharon::new(
        SHARED_STRONGSWAN_CONF.to_string(),
        SHARED_SWANCTL_CONF.to_string(),
        std::path::PathBuf::from(SHARED_CHARON_LOG),
    ));

    // One local flag shared by every line's own USIM-bridge thread in this
    // pass — see this function's own doc comment for why it must be local
    // rather than the real, container-wide flag.
    let shutting_down = Arc::new(RwLock::new(false));

    let handles: Vec<(String, std::thread::JoinHandle<LineOutcome>)> = targets
        .iter()
        .map(|t| (t.line.clone(), t.cache_path.clone()))
        .map(|(line, cache_path)| {
            // Captured before the thread is spawned so a panic inside it
            // still lets the join fallback below attribute the failure to
            // the right line instead of reporting it as "<unknown>".
            let card_id_for_panic = line.card_id.clone();
            let runner = Arc::clone(&runner);
            let bin = bin.to_string();
            let config_path = config_path.to_string();
            let config = config.clone();
            let started = Arc::clone(started);
            let shutting_down = Arc::clone(&shutting_down);
            let shared_charon = Arc::clone(&shared_charon);
            let real_shutting_down = Arc::clone(real_shutting_down);

            let handle = std::thread::spawn(move || {
                let card_id = line.card_id.clone();
                let ctx = LineStartup {
                    runner: &runner,
                    bin: &bin,
                    config_path: &config_path,
                    config: &config,
                    started: &started,
                    shutting_down: &shutting_down,
                    alert_ctx: None,
                    shared_charon: &shared_charon,
                    real_shutting_down: Some(&real_shutting_down),
                };

                let Some((mcc, mnc)) = prepare_vowifi_line(&ctx, &line) else {
                    return (
                        card_id,
                        Err(
                            "priming: could not prepare the discovered line (modem/PLMN issue \
                             — see the error above)"
                                .to_string(),
                        ),
                    );
                };

                let result =
                    establish_line_tunnel(&ctx, &line, &mcc, &mnc, Some(MAX_ESTABLISH_ATTEMPTS));

                match result {
                    Some((pcscf, _usim_holder)) => {
                        let write_result = std::fs::write(&cache_path, &pcscf).map_err(|e| {
                            format!(
                                "priming: line {card_id}: captured {pcscf} but could not write \
                                 it to {}: {e}",
                                cache_path.display()
                            )
                        });
                        match &write_result {
                            Ok(()) => println!(
                                "[supervise] priming: line {card_id}: captured P-CSCF {pcscf}, \
                                 wrote it to {}",
                                cache_path.display()
                            ),
                            Err(e) => eprintln!("[supervise] priming: {e}"),
                        }
                        (card_id, write_result)
                    }
                    None => (
                        card_id.clone(),
                        Err(format!(
                            "priming: line {card_id}: the tunnel did not establish (see the \
                             error above for which step failed)"
                        )),
                    ),
                }
            });
            (card_id_for_panic, handle)
        })
        .collect();

    let outcomes: Vec<LineOutcome> = handles
        .into_iter()
        .map(|(card_id, h)| join_outcome(card_id, h.join()))
        .collect();

    // Stop every line's own background thread (each USIM bridge's retry
    // loop) *before* tearing down: `tear_down`'s `KillChild` step only stops
    // the current process — without this, those loops would just spawn
    // replacements a few seconds later, right as we delete the netns they
    // need. This is the local, pass-scoped flag, never the real one.
    *shutting_down.write().unwrap() = true;

    let owned_netns: BTreeSet<String> = targets.iter().map(|t| t.line.netns.clone()).collect();
    tear_down(runner.as_ref(), started, config_path, &owned_netns);

    outcomes
}

#[cfg(test)]
mod tests {
    use super::super::runner::MockCommandRunner;
    use super::*;
    use crate::config::AppConfig;

    #[test]
    fn missing_card_ids_reports_a_needed_line_absent_from_this_passs_selection() {
        let needed: BTreeSet<String> =
            ["ec20-AAAAAA".to_string(), "ec20-BBBBBB".to_string()].into();
        let found: BTreeSet<String> = ["ec20-AAAAAA".to_string()].into();

        assert_eq!(
            missing_card_ids(&needed, &found),
            vec!["ec20-BBBBBB".to_string()]
        );
    }

    #[test]
    fn missing_card_ids_is_empty_when_every_needed_line_was_found() {
        let needed: BTreeSet<String> = ["ec20-AAAAAA".to_string()].into();
        let found = needed.clone();

        assert!(missing_card_ids(&needed, &found).is_empty());
    }

    #[test]
    fn join_outcome_passes_through_a_successful_threads_own_outcome() {
        let joined: std::thread::Result<LineOutcome> = Ok(("ec20-AAAAAA".to_string(), Ok(())));

        let (card_id, outcome) = join_outcome("ec20-AAAAAA".to_string(), joined);

        assert_eq!(card_id, "ec20-AAAAAA");
        assert!(outcome.is_ok());
    }

    #[test]
    fn join_outcome_attributes_a_panicked_threads_failure_to_its_own_card_id() {
        let joined: std::thread::Result<LineOutcome> = Err(Box::new(()));

        let (card_id, outcome) = join_outcome("ec20-BBBBBB".to_string(), joined);

        assert_eq!(
            card_id, "ec20-BBBBBB",
            "a panic must not lose which line it belongs to"
        );
        assert!(outcome.is_err());
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
        config.vowifi.epdg_ip = Some("192.0.2.1".to_string());
        // Isolated per test (and distinct from the real default
        // `/tmp/pcscf-0`), so `per_line_cache_path`-derived assertions never
        // collide with another test or a stale file left by an earlier run.
        config.volte.pcscf_source_path = std::env::temp_dir()
            .join(format!("pcscf-prime-test-{}", uuid_like_suffix()))
            .to_string_lossy()
            .to_string();
        config
    }

    /// A cheap, dependency-free unique-enough suffix for test-local temp
    /// paths — this crate has no `uuid` dependency, and a std-only source
    /// (thread id + a monotonic counter) is sufficient to keep concurrently
    /// run tests from colliding on the same file.
    fn uuid_like_suffix() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        format!(
            "{:?}-{}-{}",
            std::thread::current().id(),
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[test]
    fn refuses_the_swu_engine_before_touching_anything() {
        let mut config = test_config();
        config.vowifi.tunnel_engine = "swu".to_string();
        let mock = Arc::new(MockCommandRunner::new());
        let runner: Arc<dyn CommandRunner> = mock.clone();

        let started = Arc::new(Mutex::new(StartedState::default()));
        let real_shutting_down = Arc::new(RwLock::new(false));
        let needed: BTreeSet<String> = ["card0".to_string()].into_iter().collect();
        let outcomes = prime_pass(
            runner,
            "gsm-sip-bridge",
            "/tmp/cfg.toml",
            &config,
            &needed,
            &started,
            &real_shutting_down,
        );

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].0, "card0");
        assert!(outcomes[0].1.as_ref().unwrap_err().contains("strongswan"));
        assert!(
            mock.run_calls.lock().unwrap().is_empty(),
            "must bail before even running 'discover'"
        );
    }

    /// A minimal, otherwise-viable line — mirrors the `LineResolutionEntry`
    /// literals `orchestrate.rs`'s own tests already build to exercise
    /// `start_vowifi_line_strongswan` directly, bypassing `discover`.
    fn priming_line(index: u32, card_id: &str, if_id: u32, vpcd_port: u16) -> LineResolutionEntry {
        LineResolutionEntry {
            index,
            card_id: card_id.to_string(),
            modem_port: format!("/dev/ttyUSB{index}"),
            netns: format!("ims{index}"),
            control_port: 0,
            veth_local_addr: format!("169.254.{index}.2"),
            veth_peer_addr: format!("169.254.{index}.1"),
            vpcd_port,
            strongswan_if_id: if_id,
            strongswan_tun_iface: format!("tun{if_id}"),
            pcscf_source_path: format!("/tmp/pcscf-prime-test-{index}"),
            mcc: "404".to_string(),
            mnc: "043".to_string(),
            pcsc_reader: false,
            configured_identifier: None,
            msisdn: None,
            // Bypasses `resolve_imsi`'s `vowifi-imsi` subprocess call, which
            // MockCommandRunner would otherwise answer with an empty-but-
            // successful output (no IMSI parsed, so the line would bail
            // before ever reaching the establish loop this module's tests
            // care about).
            config: crate::config::VowifiConfig {
                imsi_override: Some("404430123456789".to_string()),
                ..Default::default()
            },
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
        let config = test_config();
        let line = priming_line(0, "card0", 23, 35963);
        let cache_path =
            crate::volte::pcscf::per_line_cache_path(&config.volte.pcscf_source_path, "card0");
        let target = PrimeTarget {
            line,
            cache_path: cache_path.clone(),
        };
        let started = Arc::new(Mutex::new(StartedState::default()));
        let real_shutting_down = Arc::new(RwLock::new(false));

        let outcomes = prime_lines(
            runner,
            "gsm-sip-bridge",
            "/tmp/cfg.toml",
            &config,
            &[target],
            &started,
            &real_shutting_down,
        );

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].0, "card0");
        let err = outcomes[0].1.as_ref().unwrap_err();
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
            !cache_path.exists(),
            "a failed attempt must never write a cache file"
        );
        let state = started.lock().unwrap();
        assert!(
            state.pcscd.is_none()
                && state.vowifi_child_handles.is_empty()
                && state.started_netns.is_empty()
                && state.vowifi_lines.is_empty(),
            "the real, shared StartedState must be left exactly as it was found — no phantom \
             entries for a failed attempt's already-torn-down resources"
        );
    }

    /// specs/081-multi-carrier-pcscf User Story 1/FR-005: one line's early
    /// failure must never prevent, or corrupt the outcome of, another line
    /// concurrently primed in the same pass.
    ///
    /// A genuine *successful* establish cannot be driven in this mock at
    /// all — confirmed live while writing this test: `SharedCharon::
    /// spawn_locked` unconditionally truncates the charon log
    /// (`engines.rs`, `runner.write_file(&self.charon_log, "")`) the moment
    /// it spawns the (mocked, no real process) charon daemon, so a
    /// pre-seeded "established" log line can never survive to be read by
    /// `tick_establishing` — the same "no real charon/EAP-AKA in CI" gap
    /// this module's own doc comment already flags. So instead of proving
    /// one line *succeeds* alongside another's failure, this proves the
    /// narrower, still load-bearing property a regression here would break
    /// first: one line failing fast (an absent modem port, caught by
    /// `prepare_vowifi_line`'s existence check before it ever touches
    /// charon) does not short-circuit the pass and skip the other line, and
    /// each line's own outcome is attributed correctly — never the other
    /// line's error, never silently dropped.
    ///
    /// The "other" line here is `pcsc_reader: true` rather than a second
    /// modem-backed line — `prepare_vowifi_line` skips its real, unmockable
    /// `Path::exists()` modem check entirely for a pcsc_reader line, so it
    /// reaches the (fully `CommandRunner`-mocked) establish stage the same
    /// way on every machine, including CI runners with no `/dev/ttyUSB*` at
    /// all. An earlier version of this test used a second modem-backed line
    /// instead and happened to pass locally only because this sandbox has
    /// real `/dev/ttyUSB0`/`ttyUSB1` devices — it failed in CI, where they
    /// don't exist, because *both* lines then failed at the same "could not
    /// prepare" step with the same message. `set_born_dead_if_argv_contains`
    /// (the same technique `establish_failure_still_tears_down_whatever_
    /// partially_started` already uses) then gives this line its own
    /// deterministic, later-stage failure instead.
    #[test]
    fn one_lines_failure_does_not_prevent_or_corrupt_another_lines_outcome() {
        let mock = Arc::new(MockCommandRunner::new());
        mock.set_born_dead_if_argv_contains("charon");
        let runner: Arc<dyn CommandRunner> = mock.clone();
        let config = test_config();
        mock.set_tcp_connect_ok(&config.vowifi.vpcd_host, config.vowifi.vpcd_port, true);
        let mut failing = priming_line(1, "card-failing", 24, 35964);
        failing.modem_port = "/dev/ttyUSB-nonexistent-for-this-test".to_string();
        let mut other = priming_line(0, "card-other", 23, 35963);
        other.pcsc_reader = true;
        let other_cache =
            crate::volte::pcscf::per_line_cache_path(&config.volte.pcscf_source_path, "card-other");
        let failing_cache = crate::volte::pcscf::per_line_cache_path(
            &config.volte.pcscf_source_path,
            "card-failing",
        );
        let targets = vec![
            PrimeTarget {
                line: other,
                cache_path: other_cache.clone(),
            },
            PrimeTarget {
                line: failing,
                cache_path: failing_cache.clone(),
            },
        ];
        let started = Arc::new(Mutex::new(StartedState::default()));
        let real_shutting_down = Arc::new(RwLock::new(false));

        let outcomes = prime_lines(
            runner,
            "gsm-sip-bridge",
            "/tmp/cfg.toml",
            &config,
            &targets,
            &started,
            &real_shutting_down,
        );

        assert_eq!(
            outcomes.len(),
            2,
            "the failing line's early return must not skip attempting the other line"
        );
        let by_card: std::collections::HashMap<_, _> = outcomes.into_iter().collect();
        let failing_err = by_card["card-failing"].as_ref().unwrap_err();
        assert!(
            failing_err.contains("could not prepare"),
            "got: {failing_err}"
        );
        let other_err = by_card["card-other"].as_ref().unwrap_err();
        assert!(
            other_err.contains("the tunnel did not establish"),
            "the other line's own outcome must never be the failing line's error, and must \
             reach its own, later failure stage: {other_err}"
        );
        assert!(!other_cache.exists());
        assert!(!failing_cache.exists());
    }

    /// Greptile PR #87 review, sixth finding: "Priming Stops The Main
    /// Daemon" — the circuit-switched daemon supervisor always starts before
    /// VoLTE and lives in the same shared `StartedState` priming now
    /// registers its own resources into. A teardown plan built from a raw
    /// clone of that whole structure would include a `KillChild` step for
    /// the daemon on every pass, successful or not — this proves it does
    /// not, regardless of how many lines a pass primes at once.
    #[test]
    fn tear_down_never_touches_resources_it_did_not_itself_create() {
        let mock = Arc::new(MockCommandRunner::new());
        mock.set_born_dead_if_argv_contains("charon");
        let runner: Arc<dyn CommandRunner> = mock.clone();
        let config = test_config();
        let line = priming_line(0, "card0", 23, 35963);
        let cache_path =
            crate::volte::pcscf::per_line_cache_path(&config.volte.pcscf_source_path, "card0");
        let target = PrimeTarget { line, cache_path };
        let daemon_handle = Arc::new(
            mock.spawn(super::super::runner::ChildSpec::new(["true"]))
                .unwrap(),
        );
        let started = Arc::new(Mutex::new(StartedState {
            daemon_supervisor: Some(daemon_handle.clone()),
            ..StartedState::default()
        }));
        let real_shutting_down = Arc::new(RwLock::new(false));

        let _ = prime_lines(
            runner,
            "gsm-sip-bridge",
            "/tmp/cfg.toml",
            &config,
            &[target],
            &started,
            &real_shutting_down,
        );

        assert!(
            mock.children
                .lock()
                .unwrap()
                .get(&daemon_handle.id())
                .expect("the daemon's own handle must still be tracked")
                .signals_received
                .is_empty(),
            "priming's own teardown must never signal a resource it did not create"
        );
        assert_eq!(
            started
                .lock()
                .unwrap()
                .daemon_supervisor
                .as_ref()
                .map(Arc::as_ptr),
            Some(Arc::as_ptr(&daemon_handle)),
            "an unrelated field in the shared StartedState must be left untouched"
        );
    }

    /// Greptile PR #89 review, "Priming Deletes Live Namespaces": unlike
    /// `pcscd`/`vowifi_child_handles` (exclusively VoWiFi-tunnel concepts,
    /// safe to clear wholesale — see `tear_down`'s own doc comment),
    /// `started_netns` is also written by `orchestrate_volte`'s real
    /// per-line startup, which specs/081-multi-carrier-pcscf's concurrent
    /// coordinator thread can now run at the same time as a priming pass
    /// for a *different* line. A real line's own netns entry, added to the
    /// shared `StartedState` while this pass is still running, must survive
    /// this pass's teardown untouched — never deleted, never dropped from
    /// tracking.
    #[test]
    fn tear_down_leaves_a_concurrently_started_real_lines_netns_alone() {
        let mock = Arc::new(MockCommandRunner::new());
        mock.set_born_dead_if_argv_contains("charon");
        let runner: Arc<dyn CommandRunner> = mock.clone();
        let config = test_config();
        // Reaching the establish stage (and therefore `started_netns.push`)
        // requires pcscd/vpcd to look ready first — without this, the whole
        // pass would return before `tear_down` is even called, and this
        // test would pass for the wrong reason.
        mock.set_tcp_connect_ok(&config.vowifi.vpcd_host, config.vowifi.vpcd_port, true);
        let line = priming_line(0, "card0", 23, 35963);
        let cache_path =
            crate::volte::pcscf::per_line_cache_path(&config.volte.pcscf_source_path, "card0");
        let target = PrimeTarget { line, cache_path };
        // Simulates a real, already-primed VoLTE line's own per-line spawn
        // loop (orchestrate_volte.rs's `started.lock().unwrap().started_netns
        // .push(...)`) landing concurrently with this priming pass, for a
        // line this pass knows nothing about.
        let started = Arc::new(Mutex::new(StartedState {
            started_netns: vec!["volte5".to_string()],
            vowifi_lines: vec![shutdown::StartedVowifiLine {
                index: 5,
                strongswan: None,
                netns: "volte5".to_string(),
                veth_host: "veth-volte5".to_string(),
            }],
            ..StartedState::default()
        }));
        let real_shutting_down = Arc::new(RwLock::new(false));

        let _ = prime_lines(
            runner,
            "gsm-sip-bridge",
            "/tmp/cfg.toml",
            &config,
            &[target],
            &started,
            &real_shutting_down,
        );

        assert!(
            !mock
                .run_calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c == &["ip", "netns", "del", "volte5"]),
            "the concurrently-started real line's namespace must never be deleted by this pass"
        );
        let state = started.lock().unwrap();
        assert!(
            state.started_netns.contains(&"volte5".to_string()),
            "the real line's netns must still be tracked for the eventual real shutdown"
        );
        assert!(
            state.vowifi_lines.iter().any(|l| l.netns == "volte5"),
            "the real line's own StartedVowifiLine entry must still be tracked"
        );
        assert!(
            !state.started_netns.contains(&"ims0".to_string()),
            "this pass's own netns must still be torn down and untracked as before"
        );
    }

    /// Greptile PR #87 review, finding 2: a real (container-wide) shutdown
    /// that lands while a priming pass is polling for its tunnel(s) must not
    /// be left to run out its own several-minute ceiling — it must abandon
    /// every in-flight line within one poll interval and still run its own
    /// local teardown, exactly like any other establish failure.
    #[test]
    fn a_real_shutdown_mid_establish_abandons_every_line_and_still_tears_down() {
        let mock = Arc::new(MockCommandRunner::new());
        let runner: Arc<dyn CommandRunner> = mock.clone();
        let config = test_config();
        let line = priming_line(0, "card0", 23, 35963);
        let cache_path =
            crate::volte::pcscf::per_line_cache_path(&config.volte.pcscf_source_path, "card0");
        let target = PrimeTarget {
            line,
            cache_path: cache_path.clone(),
        };
        let started = Arc::new(Mutex::new(StartedState::default()));
        // Already true before the pass starts — the establish loop must
        // check this before its very first `tick_establishing`, not just
        // between sleeps, so this test never needs to drive a real
        // multi-iteration poll.
        let real_shutting_down = Arc::new(RwLock::new(true));

        let outcomes = prime_lines(
            runner,
            "gsm-sip-bridge",
            "/tmp/cfg.toml",
            &config,
            &[target],
            &started,
            &real_shutting_down,
        );

        assert_eq!(outcomes.len(), 1);
        let err = outcomes[0].1.as_ref().unwrap_err();
        assert!(err.contains("priming"), "got: {err}");
        assert!(
            mock.children
                .lock()
                .unwrap()
                .values()
                .any(|c| !c.signals_received.is_empty()),
            "teardown must still run for a pass abandoned due to real shutdown"
        );
        assert!(!cache_path.exists());
    }

    // `prime_pass` itself (as opposed to `prime_lines`, tested above) is
    // deliberately NOT further unit tested for its `discover_priming_lines`
    // step: that step calls the real modem scanner directly, not through
    // `CommandRunner` (see its own doc comment for why), so its behavior
    // legitimately depends on whatever hardware is actually attached to the
    // machine running the test — the same "hardware not available in CI"
    // situation the constitution's mocking carve-out exists for, in
    // reverse, that specs/080's original single-line version already
    // documented. The per-line gating/resolution decision logic around it
    // (which `card_id`s end up in `needed_card_ids` at all) is tested in
    // `orchestrate_volte.rs` instead, with the call itself injected.
}
