# Implementation Plan: The Supervisor Stops Retrying in a Tight Loop While a Card-Reader SIM Is Out of Reach

**Branch**: `163-pcsc-outage-backoff` | **Date**: 2026-11-24 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `specs/163-pcsc-outage-backoff/spec.md`

## Summary

While a card-reader line's SIM is out of reach, the supervisor stops restarting the agent every 5 s and stops re-initiating the ePDG tunnel every ~50 s. It does this with three pieces:

1. **Two typed signals.**
   - The agent logs a shared marker when it exits because the card is out of reach.
   - `vowifi-plmn` exits with code 3 for the same cause, which turns it into a bounded, killable card check (`timeout 5 … vowifi-plmn --pcsc-imsi <IMSI>`).
2. **One shared per-line `CardOutage` state** (pure, with an injected clock). It paces agent restarts at 5 s doubling to 120 s, and checks the card every 5 s so the line restarts as soon as the card is back.
3. **A card gate beside the existing underlay gate** in the steady tick.
   - A recovery that would build a new IKE_SA with no IKE_SA to lose is paced: 60 s doubling to 120 s.
   - A recovery that would tear down an established IKE_SA it cannot rebuild is held until the card is back.

The tunnel-failure alert names the cause, through a new optional `card_hold` field in the supervisor's report (Clarification 2026-11-24). Modem and VoLTE lines are untouched. Design decisions D1–D11 are in [research.md](research.md).

## Technical Context

**Language/Version**: Rust 1.99.1 (`rust-toolchain.toml`), crate `gsm-sip-bridge`
**Primary Dependencies**: existing only (`pcsc` through `vowifi-plmn`; `serde` for `AgentState`). No new crates
**Storage**: N/A. All state is in memory, per supervisor process
**Testing**: `cargo test` through `make test`. Injected clocks (`Instant` passed in) and `MockCommandRunner` for supervisor decisions, which is the existing style of `sim_recovery`, `underlay` and `line_supervisor`. Live on the local OMNIKEY rig (quickstart §2)
**Target Platform**: Linux container (host network), supervisor (`gsm-sip-bridge supervise`)
**Project Type**: telecom bridge, a single binary with subcommands
**Performance Goals**:
- agent restarts during a 10-minute outage: ≤15 (SC-001);
- tunnel initiations in the same window: ≤6 (SC-002);
- the line registered again ≤30 s after the card returns (SC-003).

**Constraints**:
- no unbounded blocking on a supervisor thread: card checks run under `timeout 5`;
- no free-text matching;
- no change to the uplink surface (FR-011a);
- modem lines are byte-for-byte unchanged.

**Scale/Scope**: about five source files plus one new module (`supervise/card_outage.rs`) and docs. Per line: one shared `CardOutage`, and at most one card subprocess every 5 s during an outage, none otherwise (apart from one check before a recovery that needs the SIM).

## Constitution Check

| Principle | Status |
|---|---|
| I. Integration-first testing | Pass. Decisions are tested through the real `sim_recovery`, `line_supervisor` and `underlay` code with `MockCommandRunner`, the existing supervisor test double for processes, because CI has no charon or reader. Live tests run on real hardware. The one mock site (the scripted `vowifi-plmn` exit status) gets a justification comment |
| II. Green on commit | Pass. `make format && make lint && make test` before each of the 5 commits |
| III. Atomic commits | Pass. The five commits: (1) signals; (2) `CardOutage` and agent pacing; (3) the tunnel card gate; (4) the alert text; (5) docs |
| IV. Makefile-driven | Pass. No new targets |
| V. Simplicity | Pass. It reuses `NORMAL_RESTART_DELAY`, `RETRY_MAX_BACKOFF`, `next_backoff`, `HELD_POLL_INTERVAL`, `recovery_renegotiates`, the `vowifi-plmn` read, the `HeldInitiationEstablished` path and the `uplink` report pattern. The only new abstraction is `CardOutage` |

Re-check after design: passes. The design adds one outcome variant, one gate function and one optional protocol field.

## Blast Radius

Real `grep` output from `c249cb1`, paths relative to `gsm-sip-bridge/src`.

### State and invariants inventory

| Changed item | Readers / writers (grep) | Copies at construction | Invariants and comments | Decision |
|---|---|---|---|---|
| `AgentExitOutcome` (new `CardUnreachable`) | `grep -rln AgentExitOutcome`: `supervise/sim_recovery.rs`, `supervise/orchestrate.rs` (`:3303`–`:3316`), `vowifi/usim_bridge.rs` (`:16`, `:413`, `:430`: constructs `Other`/`CsimFailure` only) | none | `observe` resets both counters on `Other`; the doc says "a non-CSIM failure is a different problem" | Add the variant with `Action::None` and no reset (D4). The `reason` match at `orchestrate.rs:3312` gains `" (SIM card out of reach)"`. `usim_bridge` is unaffected (it never sees the marker) |
| Agent restart wait `runner.sleep(restart_delay)` | `orchestrate.rs:3361`, the only site. `restart_delay(` production caller: `orchestrate.rs:3311` | none | The F1 loop checks `shutting_down` only at the top | Card exits use `wait_for_card_or(delay)`: 5 s steps, a check when due, early return on `Present` or shutdown. Other exits keep `runner.sleep(5 s)` |
| `SteadyOutcome` (new `CardHeld`) | Exhaustive matches: `line_supervisor.rs:1302` (`outcome_restarts_agent`), `underlay.rs:466` (`after`), `orchestrate.rs:1461` (hint), `:2231`, `:2287`, `:2324` (the loop's handling). `orchestrate_volte.rs:167` matches a different type | `UnderlayTracker` records holds from `Held` only | `outcome_restarts_agent` has "no wildcard" | Add the variant. The compiler forces each site: no agent restart, no uplink hold, log only the paced-initiation line, and after the tick call `CardOutage::record` / `tunnel_attempted` |
| `tick_steady_state_underlay` signature (adds `card: &CardView, check: &dyn Fn() -> CardVerdict`) | Callers: `line_supervisor.rs:1242` (`tick_steady_state_with_pool`, the guard-off wrapper), `:4980` (tests), `orchestrate.rs:2205` (the loop) | `UnderlayView` is built before the tick and "never changed by it" | Same contract for `CardView` | The guard-off wrapper and tests pass `CardView::none()` and a check closure that panics if called (proving it is never called). The loop builds the real view |
| SIM-needing recovery sites | `underlay_gate(` at `line_supervisor.rs:1510`, `:1626`, `:1694`, `:1832`, `:1927`; `IkeSaMissing` held branch `:1449`; `TunVanished` `:1395` (no gate today) | `tracking.held_initiate_sent`, `agent_restart_owed` (`:966`, set `:1455`, cleared `:1292`, read `:1565`) | `underlay_gate`: "Never called for `IkeSaLost` … nor for a structural reason" | `card_gate(card, check, reason)` is called after `underlay_gate` at each of these sites, and on its own at `TunVanished` and `IkeSaMissing`. The paced initiation reuses `agent_restart_owed`. The `:1565` release is unchanged (an established IKE_SA proves the card answered) |
| `recovery_renegotiates` | `orchestrate.rs:2267`, `:3689`; tests `line_supervisor.rs:3533`–`:3546`, `:4840` | - | "Narrower than `recovery_restarts_agent`" | Not changed. The new exhaustive `needs_the_sim` is pinned to it by a test (D5) |
| Steady loop poll interval | `UnderlayTracker::poll_after` (`underlay.rs:268`) uses `HELD_POLL_INTERVAL` (`:45`) | - | "Quicker while a hold is on" | The loop takes `min(underlay.poll_after(now), card ? HELD_POLL_INTERVAL : steady)` (D10) |
| `AgentState` (new `card_hold`) | Literal sites in `metrics/ingest.rs`, `control/protocol.rs`, `vowifi/mod.rs`, `supervise/orchestrate.rs` (29 `AgentState {` sites); reader `metrics/ingest.rs:325`–`:345` (supervisor branch) | The ingest record (`last_uplink` pattern, `:124`, `:291`, `:578`, `:782`) | serde `default`, `skip_serializing_if`; the round-trip test `protocol.rs:1120` | Optional field. Every literal gets `card_hold: None` or uses `..Default::default()`. Add a `last_card_hold` record field, copied at the same places as `last_uplink`. A round-trip and old-payload test |
| `tunnel_failure_description` | `metrics/ingest.rs:1144` (the only production caller), tests `:3114`–`:3122` | - | "The supervisor and the IMS agent share one folded alert" | Add a `card_hold: bool` parameter. The folded read takes the supervisor's `last_card_hold`, as for `uplink` |
| `vowifi::plmn::run` exit status | `orchestrate.rs:1070`, `:1077` via `read_plmn_once` (`:1130`: any non-zero means failure) | - | - | 3 for `is_card_unreachable()`, 1 otherwise. `read_plmn_once` is unchanged |
| `ims::agent::run` error arm | `ims/agent/mod.rs:226`–`:231` | - | - | Log `CARD_UNREACHABLE_MARKER` with `tracing::error!` before `eprintln!` when `e.is_card_unreachable()` |
| `RETRY_MAX_BACKOFF` | `ims/agent/mod.rs:164` (private), many uses in the agent | - | The budget-ladder tests at `mod.rs:6901`+ pin the ladder | Make it `pub(crate)` only. Value unchanged |

**`vowifi-plmn` exit-status dependents (T004, at implementation).** `grep -rn 'vowifi-plmn\|status.code()\|ExitStatus::from_raw(1' gsm-sip-bridge/src/supervise`:
- production: `orchestrate.rs:1070` and `:1077` (`resolve_mcc_mnc`), both through `read_plmn_once` (`:1130`), which tests `!out.status.success()` only;
- `engines.rs:231` (`status.code() != Some(TIMEOUT_EXIT_CODE)`) and `runner.rs:595` are about `swanctl` and the runner, not `vowifi-plmn`;
- every `ExitStatus::from_raw(1 << 8)` hit is a test fixture (`orchestrate.rs:4417`, `:4469`, `:4487`, `orchestrate_volte.rs:823`) asserting failure, which exit 3 still is.

No shell script or template reads its status (`grep -rn vowifi-plmn docker scripts`: a comment in `swanctl-epdg.conf.template` only). Nothing depends on the status being exactly 1.

**Implicit contracts:**
- **Text classifiers:** the marker is new, and nothing else contains it. `has_csim_failure` greps `AT+CSIM failed`, which a card-reader agent never logs. The `vowifi-plmn` stderr text is unchanged, so `mask_subscriber_ids` and the start-up error lines are unaffected.
- **Limits:** the existing modem `CSIM_FAIL_THRESHOLD`, `MAX_SIM_RESETS` and `EXHAUSTED_RESTART_DELAY` are untouched. The new cap reuses the agent's `RETRY_MAX_BACKOFF` instead of a new number.
- **Ordering:** classification is stall, then CSIM, then card, then other. In the tick, the uplink gate comes first and the card gate second (FR-010).

### Concurrency and ownership map

| Object | Owner | Shared with | Release |
|---|---|---|---|
| `Arc<Mutex<CardOutage>>` per card-reader line | `start_vowifi_line_strongswan` (created next to `RebuildGate`) | that line's agent thread (`start_line_tail`) and steady loop | Dropped with both threads at process exit. The lock is never held across a subprocess or a sleep: take a decision, drop the lock, act, re-lock to record |
| `timeout 5 vowifi-plmn --pcsc-imsi` child | `runner.run` (blocking, bounded to 5 s by `timeout`, reaped by `init: true`) | - | `timeout` kills it at 5 s. It opens the card through `PcscTransport::connect` and releases with `LeaveCard` (#232) |
| Agent restart wait | the agent thread | - | Each 5 s step checks `shutting_down`, so shutdown is no longer delayed by up to 120 s |

**Lock order:** `CardOutage` is the innermost lock: nothing else is taken while it is held, and it is never held across a subprocess or a sleep. The steady loop takes it while holding its `shutting_down` read guard (the guard spans the whole tick, as before); the agent thread takes it only after releasing `shutting_down` and never while holding `rebuild_gate.spawn` or the child-table lock. (At implementation: corrected from "never taken while `shutting_down` is held", which the steady loop's tick-long guard contradicts; the order is still acyclic.)

### Where else does this logic run?

- **SWu lines:** they cannot be card-reader lines (`orchestrate.rs:1551`), so there is no gate there.
- **Establishing phase (`tick_establishing`):** start-up, out of scope (D11).
- **VoLTE (`orchestrate_volte.rs`):** never card-reader.
- **The daemon's alert evaluation:** it reads one more optional field. The raise and recovery logic are unchanged.
- **Other lines:** each card-reader line has its own `CardOutage` and checks its own IMSI (FR-012).

### Situations checklist

| Situation | Answer |
|---|---|
| Idle | The outage paces restarts and initiations, and the card check runs every 5 s |
| In a call | Not reachable from the supervisor's side: a card outage during a call matters only if the tunnel drops or the agent exits, and then there is no call to protect. FR-009 never tears down a working tunnel because of the card |
| Ringing / origination pending | Same as in a call; the agent owns that |
| UDP / TCP | Not involved |
| Error and rollback | A check that cannot run is `Unknown` and changes nothing. A paced initiation that fails leaves the outage on and the interval doubled |
| Cancel / shutdown | The agent wait checks `shutting_down` every 5 s. The steady loop's existing shutdown handling is unchanged |
| Limits and off-by-one | The cap (120 s) applies to both backoffs. The first tunnel attempt under an outage is immediate (`tunnel_next_at = None`), then 60, 120, 120… (SC-002 ≤6). Agent delays are 5…120 (SC-001 ≤15) |
| Blocks a thread | Up to 5 s per check, on the line's own threads only (F10). The existing tick already runs bounded `swanctl` commands the same way |
| Docs, release notes, repo rules | `docs/observability.md`, `docs/omnikey-pcsc-vowifi.md`, the learnings line and one `## Unreleased` bullet (summary only bolded). No real IMSIs: tests use `001010000000001` |

## Project Structure

### Documentation (this feature)

```text
specs/163-pcsc-outage-backoff/
├── spec.md
├── plan.md
├── research.md
├── data-model.md
├── quickstart.md
├── contracts/operator-surface.md
├── checklists/requirements.md
└── tasks.md          # /speckit-tasks
```

### Source Code (repository root)

```text
gsm-sip-bridge/src/
├── ims/agent/mod.rs              # CARD_UNREACHABLE_MARKER, logged in run()'s Err arm; RETRY_MAX_BACKOFF pub(crate)
├── vowifi/plmn.rs                # EXIT_CARD_UNREACHABLE (3)
├── supervise/
│   ├── card_outage.rs            # NEW: CardVerdict, CardOutage, card_check()
│   ├── sim_recovery.rs           # AgentExitOutcome::CardUnreachable, has_card_unreachable()
│   ├── line_supervisor.rs        # CardView, needs_the_sim(), card_gate(), SteadyOutcome::CardHeld
│   ├── underlay.rs               # after(): CardHeld changes no uplink state
│   ├── orchestrate.rs            # wiring: shared CardOutage, agent wait, loop view/poll, logs, report
│   └── mod.rs                    # mod card_outage
├── control/protocol.rs           # AgentState.card_hold
└── metrics/ingest.rs             # last_card_hold; tunnel_failure_description suffix
docs/observability.md, docs/omnikey-pcsc-vowifi.md, docs/greptile-review-learnings.md, RELEASE_NOTES.md
```

**Structure decision:** the existing single crate. The one new module, `supervise/card_outage.rs`, sits next to `sim_recovery.rs` and `underlay.rs`, which have the same shape: pure, per-line, with an injected clock.

## Spec refinements made during planning

Recorded in the spec's Clarifications.

- **FR-009 sharpened.** While the card is out, a recovery that would tear down an established IKE_SA (PcscfPoolExhausted, PcscfUnreachable, ChildSaStuck, ChildSaRekeyStuck, TunVanished) is held until the card is back, not paced. Only initiations for a missing IKE_SA are paced (FR-007's floor). Rebuilding needs the SIM, so tearing down could only lose what is left.
- **FR-013 aligned with US3 scenario 3.** A paced retry keeps one line (with its delay), and held ticks log nothing at the default level. SC-005 counts only the began and ended lines.

## Complexity Tracking

None. There are no constitution violations.
