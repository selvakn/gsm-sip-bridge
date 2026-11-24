# Tasks: The Supervisor Stops Retrying in a Tight Loop While a Card-Reader SIM Is Out of Reach

**Input**: `specs/163-pcsc-outage-backoff/` (spec.md, plan.md, research.md D1–D11, data-model.md, contracts/operator-surface.md, quickstart.md)

**Tests**: requested. SC-007 asks for a deterministic test for every scenario, and the constitution makes TDD the default. Write each story's tests first and watch them fail. Where a test would fail only to compile, also check it against a mutation of the code it covers once it passes. #233 learned this: record which mutation fails which test.

**Paths**: source is `gsm-sip-bridge/src/...`, and docs are at the repo root. Use `CARGO_TARGET_DIR=$PWD/target-161` for every cargo/make command (an isolated, warm target dir; memory: shared target dir clobbered by worktrees).

**Never** put a real IMSI or MSISDN in code, tests or docs. Tests use `001010000000001`.

**Test doubles**:
- Supervisor decisions take `now: Instant` and run against `MockCommandRunner` (`supervise/runner.rs`): `set_run_output_sequence` scripts `vowifi-plmn`'s exit status, and `sleeps` records waits.
- Mock justification at each site: CI has no reader, no pcscd and no charon. The real `vowifi-plmn` exit mapping is covered by T003, and live by quickstart §2.

**Commits** (no dead code at any commit; `clippy -D warnings`):
1. signals;
2. `CardOutage` and agent pacing;
3. the tunnel card gate;
4. the alert text;
5. docs.

## Phase 1: Setup

- [X] T001 Confirm the branch is `163-pcsc-outage-backoff` with no upstream (`git rev-parse --abbrev-ref @{u}` fails). Run `CARGO_TARGET_DIR=$PWD/target-161 make test` from the repo root and record the passed/ignored counts here as the baseline.
  - Baseline (c249cb1, 2026-11-24): no upstream; `make test` (cargo test fallback) **3476 passed, 0 failed, 11 ignored**.

---

## Phase 2: Foundational: typed signals (commit 1, blocks every story)

**Purpose**: research D1. Both signals are used in this commit: the agent logs the marker, and `vowifi-plmn` returns the code.

- [X] T002 [P] In `gsm-sip-bridge/src/ims/agent/mod.rs` add `pub(crate) const CARD_UNREACHABLE_MARKER: &str = "the SIM card is out of reach; exiting";`, with a doc comment saying `supervise::sim_recovery::has_card_unreachable` greps for it and tests pin both sides (as for `watchdog::STALL_MARKER`). In `run()`'s `Err(e)` arm, before `eprintln!`, add `if e.is_card_unreachable() { tracing::error!(error = %e, "{}", CARD_UNREACHABLE_MARKER); }`. Make the error arm testable through a small `fn exit_log_line(e: &BridgeError) -> Option<String>` or equivalent, and test it: `a_card_unreachable_exit_logs_the_marker` (CardUnreachable gives `Some` containing the marker; `Ims("x")` gives `None`).
- [X] T003 [P] In `gsm-sip-bridge/src/vowifi/plmn.rs` add `pub(crate) const EXIT_CARD_UNREACHABLE: u8 = 3;` with a doc comment (the supervisor's card check, research D2). Split `run`'s result mapping into a pure `fn exit_code_for(result: &BridgeResult<_>) -> ExitCode`-style helper and use it. Test: `an_unreachable_card_exits_3_and_other_errors_exit_1` (CardUnreachable → 3, `Ims` → 1, `Discovery` → 1; success → 0).
- [X] T004 Grep for anything that depends on `vowifi-plmn`'s exit status being exactly 1: `grep -rn 'vowifi-plmn\|status.code()\|ExitStatus::from_raw(1' gsm-sip-bridge/src/supervise`. `read_plmn_once` checks `!status.success()` only. Record the result in the plan's Blast Radius. Run `make format && make lint && make test` and commit: `feat: signal an unreachable card from the agent's exit and from vowifi-plmn (#233 follow-up)`.

**Checkpoint**: both signals exist and are pinned on the emitting side.

---

## Phase 3: User Story 1 - A missing card no longer restarts the agent every few seconds (P1) 🎯 MVP

**Goal**: FR-001, FR-002, FR-003, FR-004, FR-005, FR-006 and FR-012, for the agent. Research D2, D3, D4.

**Independent test**: with the card out, a run of card-unreachable exits restarts after 5, 10, 20… up to 120 s; a `Present` check restarts at once; modem exits are unchanged.

### Tests for User Story 1 (write first)

- [X] T005 [P] [US1] Create `gsm-sip-bridge/src/supervise/card_outage.rs` with its `mod tests` first (add `mod card_outage;` in `supervise/mod.rs`). Pure tests with an injected `Instant`:
  - `an_outage_begins_once_and_ends_once` (`on_out` twice gives one `Began`; `on_present` gives `Ended { held_for }` with the right duration; a second `on_present` gives nothing);
  - `unknown_never_starts_or_ends_an_outage`;
  - `a_check_is_due_at_most_every_five_seconds`;
  - `agent_delays_double_from_five_seconds_to_the_cap` (5, 10, 20, 40, 80, 120, 120);
  - `tunnel_attempts_go_at_once_then_every_60_then_120_seconds`;
  - `every_way_out_resets_every_delay` (`on_present`, `on_registered`; a later outage starts from 5 s and 60 s again).
- [X] T006 [P] [US1] In `gsm-sip-bridge/src/supervise/card_outage.rs` tests, for `card_check(runner, bin, imsi) -> CardVerdict` against `MockCommandRunner`:
  - `card_check_maps_exit_status` (0 → Present, `EXIT_CARD_UNREACHABLE` → Out, 1 → Unknown, 124 → Unknown, a spawn error → Unknown);
  - `card_check_is_bounded_and_names_the_imsi` (argv is `["timeout", "5", bin, "vowifi-plmn", "--pcsc-imsi", "001010000000001"]`).
- [X] T007 [P] [US1] In `gsm-sip-bridge/src/supervise/sim_recovery.rs` tests:
  - `has_card_unreachable_matches_the_marker_the_agent_emits` (built from `crate::ims::agent::CARD_UNREACHABLE_MARKER`, as the stall test is);
  - `a_card_unreachable_exit_leaves_the_csim_counters_alone` (`observe(CardUnreachable)` returns `Action::None` and does not reset a CSIM count in progress);
  - `classification_prefers_stall_then_csim_then_card` (a log with both a stall marker and the card marker classifies as `AtStall`), through the extracted `classify_exit(log, pcsc_reader)` of T011;
  - `a_modem_line_never_classifies_as_card_unreachable` (`pcsc_reader = false` with the marker present gives `Other`; FR-003).
- [X] T008 [P] [US1] In `gsm-sip-bridge/src/supervise/orchestrate.rs` tests, for the extracted `wait_for_card_or(runner, outage, delay, check, shutting_down) -> WaitEnd`:
  - `the_wait_returns_early_when_the_card_is_back` (the scripted checks give Out, Out, Present: two 5 s sleeps recorded, then it returns; `CardOutage` records `Ended`);
  - `the_wait_lasts_the_whole_delay_while_the_card_stays_out` (20 s gives four 5 s sleeps);
  - `the_wait_stops_at_once_on_shutdown`;
  - `the_wait_runs_at_most_one_check_per_five_seconds_when_shared` (another holder recorded a check 2 s ago, so the first step does not check).

### Implementation for User Story 1

- [X] T009 [US1] Implement `CardVerdict`, `CardOutage` and its methods per data-model.md in `gsm-sip-bridge/src/supervise/card_outage.rs`.
  - Constants `CARD_CHECK_INTERVAL` (5 s; doc cites `underlay::HELD_POLL_INTERVAL`) and `CARD_CHECK_TIMEOUT` (5 s), and `CARD_TUNNEL_INITIAL_INTERVAL` (60 s). `CARD_OUTAGE_MAX_BACKOFF = crate::ims::agent::RETRY_MAX_BACKOFF`: make that `pub(crate)` in `ims/agent/mod.rs` with a doc line, value unchanged.
  - Doubling through `crate::ims::session::next_backoff`.
  - T005 passes.
- [X] T010 [US1] Implement `card_check` in `gsm-sip-bridge/src/supervise/card_outage.rs`: `runner.run(&["timeout", "5", bin, "vowifi-plmn", "--pcsc-imsi", imsi])` mapped per D2, with no stdout or stderr parsing. Doc comment: why a subprocess (killable, mockable) and that `vowifi-plmn` releases the card leaving it (#232). T006 passes.
- [X] T011 [US1] In `gsm-sip-bridge/src/supervise/sim_recovery.rs` add `AgentExitOutcome::CardUnreachable` with a doc comment, `pub fn has_card_unreachable(log)`, and a pure `pub fn classify_exit(log: &str, pcsc_reader: bool) -> AgentExitOutcome` (stall, CSIM, card if `pcsc_reader`, other). In `observe`, `CardUnreachable` returns `Action::None` without touching the counters. Use `classify_exit` at `supervise/orchestrate.rs:3303`. T007 passes.
- [X] T012 [US1] In `gsm-sip-bridge/src/supervise/orchestrate.rs`:
  - Create `Arc<Mutex<CardOutage>>` in `start_vowifi_line_strongswan` next to `rebuild_gate`, but only `Some` for `line.pcsc_reader`. Pass it to `start_line_tail`.
  - In the agent loop: for `CardUnreachable`, take `delay = outage.next_agent_delay()` and `record(Out)` (logging `Began` per contract), log `vowifi-ims-agent exited (SIM card out of reach); restarting in Ns`, and wait with `wait_for_card_or` (a 5 s step with `runner.sleep`, then `shutting_down` and `check_due` → `card_check` → `record`, then return on `Present`, shutdown or the end of the delay). Log `Ended` per contract.
  - On the existing "registered" detection (`:3290`), call `outage.on_registered(now)` and log `Ended` if it returns one.
  - Other exits keep `runner.sleep(restart_delay)`.
  - Never hold the `CardOutage` lock across `runner.run` or `runner.sleep`.
  - T008 passes.
- [X] T013 [US1] Run the supervisor test suite and every existing `sim_recovery` and `orchestrate` test unchanged. Then `make format && make lint && make test`, and commit: `fix(supervise): pace agent restarts while a card-reader SIM is out of reach`.

**Checkpoint**: SC-001 and SC-003 for the agent, by test.

---

## Phase 4: User Story 2 - A missing card no longer floods the carrier with failed logins (P1)

**Goal**: FR-007, FR-008, FR-009, FR-010, FR-011 (decided in D6), and FR-012 for the tunnel. Research D5, D10.

**Independent test**: drive `tick_steady_state_underlay` with a fake engine, a `CardView` and a check closure. Missing-IKE_SA initiations are paced while the card is out; rebuilds of an established IKE_SA are held; the uplink hold composes; modem lines are unchanged.

### Tests for User Story 2 (write first, in `gsm-sip-bridge/src/supervise/line_supervisor.rs` tests, next to the underlay tests around `:4962`)

- [X] T014 [P] [US2] `needs_the_sim_is_recovery_renegotiates_minus_the_shared_daemon` (every `DegradeReason`) and `card_view_none_never_calls_the_check` (passing a check that panics, over every existing fixture).
- [X] T015 [P] [US2] `card_out_paces_initiations_for_a_missing_ike_sa`: `IkeSaMissing` on a card-reader line with the outage on.
  - `attempt_due = true` gives `CardHeld { skipped: IkeSaLost, initiated: true }`, one `reinitiate`, and `agent_restart_owed` set.
  - `attempt_due = false` gives `CardHeld { initiated: false }` and no reinitiate.
  - Same for the half-open stuck path (`IkeSaNegotiationStuck`).
- [X] T016 [P] [US2] `card_out_holds_a_rebuild_of_an_established_ike_sa`: for each of PcscfPoolExhausted, PcscfUnreachable, ChildSaStuck, ChildSaRekeyStuck and TunVanished (with the interface recreated), with the card out, the outcome is `CardHeld { skipped: <reason>, initiated: false }` with no `terminate`, `delete` or `reinitiate` on the engine (FR-009).
- [X] T017 [P] [US2] `a_recovery_that_needs_the_sim_checks_the_card_first`: card-reader line, no outage.
  - The check returns `Out`: `CardHeld`, and the loop's post-tick hook (tested in T020) records `Began`.
  - The check returns `Present` or `Unknown`: the recovery runs exactly as today.
  - The check is called at most once per tick.
- [X] T018 [P] [US2] `the_uplink_hold_wins_and_the_card_gate_is_not_consulted` (an uplink `holding()` and the card out give the uplink's `Held`, and the check is not called) and `a_card_hold_starts_no_uplink_hold` (`UnderlayTracker::after(CardHeld)` leaves the verdict `Up` and the uplink file unchanged; FR-010, FR-011a).
- [X] T019 [P] [US2] `a_modem_line_recovers_exactly_as_before`: replay the existing recovery fixtures with `CardView::none()` and compare outcomes and engine calls (FR-003). Also `child_sa_missing_is_never_card_gated` (a CHILD_SA re-initiate inside a live IKE_SA needs no SIM).
- [X] T020 [P] [US2] In `gsm-sip-bridge/src/supervise/orchestrate.rs` tests, for the extracted post-tick hook `after_card_tick(outcome, outage, now) -> Option<Transition>`:
  - `CardHeld { initiated: true }` calls `tunnel_attempted`;
  - the gate's `Out` gives `Began`;
  - `HeldInitiationEstablished` on a card-reader line with the outage on still restarts the agent once;
  - the poll interval is `HELD_POLL_INTERVAL` while the outage is on.

### Implementation for User Story 2

- [X] T021 [US2] In `gsm-sip-bridge/src/supervise/line_supervisor.rs` add:
  - `CardView { pcsc_reader, outage, attempt_due }` with `CardView::none()`;
  - `needs_the_sim(reason)`, exhaustive with no wildcard;
  - `SteadyOutcome::CardHeld { skipped, initiated }`;
  - `fn card_gate(engine, runner, card, check, reason, tracking) -> Option<SteadyOutcome>` implementing D5: paced initiation for `IkeSaLost`/`IkeSaNegotiationStuck`, a hold for the rest, and an on-demand check when `pcsc_reader && !outage`. Doc comments cite FR-007 and FR-009 and Clarification 2026-11-24.
  - Classify `CardHeld` in `outcome_restarts_agent` (false).
- [X] T022 [US2] Add the `card: &CardView, check: &dyn Fn() -> CardVerdict` parameters to `tick_steady_state_underlay`/`tick_underlay`.
  - Call `card_gate` after `underlay_gate` at `:1510` (stuck only), `:1626`, `:1694`, `:1832` and `:1927`.
  - In the `IkeSaMissing` branch after the underlay-held branch (`:1449`).
  - In the `TunVanished` branch after `recreate_interface` succeeds, before `terminate`.
  - `tick_steady_state_with_pool` (`:1242`) and the existing tests pass `CardView::none()` and a panicking check. T014–T019 pass.
- [X] T023 [US2] In `gsm-sip-bridge/src/supervise/underlay.rs`, classify `CardHeld` in `UnderlayTracker::after` (`:466`): no uplink state change, and no pending-check change. T018 passes.
- [X] T024 [US2] In `gsm-sip-bridge/src/supervise/orchestrate.rs`'s steady loop (`:2205`):
  - Build `CardView` from the shared `CardOutage` before each tick. The check closure runs `card_check` and `record`s its verdict (rate-limited by `check_due`), for card-reader lines only.
  - After the tick, call `after_card_tick` and log `Began`/`Ended` and the paced-initiation line per contract.
  - Poll at `min(underlay.poll_after(now), HELD_POLL_INTERVAL if outage)`.
  - While the outage is on, run one `check_due` → `card_check` per tick so that `Present` is noticed within 5 s even with no recovery due (D10).
  - Classify `CardHeld` at the loop's `match outcome` sites (`:1461`, `:2231`, `:2287`, `:2324`).
  - T020 passes.
- [X] T025 [US2] Re-run every `line_supervisor`, `underlay` and `orchestrate` test. Then `make format && make lint && make test`, and commit: `fix(supervise): hold tunnel rebuilds that need the SIM while a card-reader SIM is out of reach`.

**Checkpoint**: SC-002, and FR-009 by test.

---

## Phase 5: User Story 3 - The operator sees one clear signal per card outage (P2)

**Goal**: FR-013 and FR-014. Research D7, D8.

### Tests for User Story 3 (write first)

- [X] T026 [P] [US3] In `gsm-sip-bridge/src/control/protocol.rs` tests: `card_hold_round_trips_and_old_payloads_still_parse` (`{"card_hold":true}` round-trips; `None` is omitted; a payload without the field parses as `None`), next to `uplink_state_and_hold_events_round_trip_and_old_payloads_still_parse` (`:1120`).
- [X] T027 [P] [US3] In `gsm-sip-bridge/src/metrics/ingest.rs` tests (next to `:3114`):
  - `tunnel_failure_text_names_the_card_while_held` (each of the three contract examples, byte for byte);
  - `tunnel_failure_text_is_unchanged_without_a_card_hold` (every existing case with `card_hold = false`);
  - `a_supervisor_report_with_card_hold_reaches_the_folded_alert` (ingest a supervisor report with `card_hold: Some(true)`, then the IMS agent's failure: the description has the suffix; after `Some(false)`, it does not).
- [X] T028 [P] [US3] In `gsm-sip-bridge/src/supervise/orchestrate.rs` tests: `a_ten_minute_outage_logs_one_began_and_one_ended_line` (drive `wait_for_card_or` and `after_card_tick` over a simulated 10 minutes with captured log lines: exactly one began and one ended; paced lines carry their delay; no IMSI; SC-005).

### Implementation for User Story 3

- [X] T029 [US3] In `gsm-sip-bridge/src/control/protocol.rs` add `#[serde(skip_serializing_if = "Option::is_none", default)] pub card_hold: Option<bool>` to `AgentState`, with a doc comment (supervisor only; #233 follow-up). Update every `AgentState { … }` literal that does not use `..Default::default()` (29 sites across `metrics/ingest.rs`, `control/protocol.rs`, `vowifi/mod.rs`, `supervise/orchestrate.rs`). T026 passes.
- [X] T030 [US3] In `gsm-sip-bridge/src/metrics/ingest.rs`:
  - add `last_card_hold: Option<bool>` to the record, carried exactly where `last_uplink` is (`:124`, `:291`, `:578`, `:782`), and set from the supervisor's report next to `uplink` (`:341`);
  - give `tunnel_failure_description` a `card_hold: bool` parameter that appends the contract suffix after the uplink suffix;
  - pass the supervisor's `last_card_hold` from `alert_description` (`:1144`).

  T027 passes.
- [X] T031 [US3] In `gsm-sip-bridge/src/supervise/orchestrate.rs`, set `signal.card_hold = Some(true/false)` on `Began`/`Ended`, next to `signal.uplink` (`:2130`, `:2260`), from both the agent thread and the steady loop. Through one helper, so the two cannot diverge. T028 passes.
- [X] T032 [US3] `make format && make lint && make test`, and commit: `feat(alerts): name an unreachable card in the tunnel-failure alert`.

---

## Phase 6: Polish & Cross-Cutting Concerns

- [X] T033 [P] `docs/observability.md`: under the card-reader section #233 added, describe the supervisor's pacing, the four supervisor log lines, and the tunnel-failure alert's suffix. Replace the "line stays down until the card returns" caveat with what happens now.
- [X] T034 [P] `docs/omnikey-pcsc-vowifi.md` Troubleshooting, "The card was out of reach": the restart and tunnel pacing, the log lines to look for, and recovery within about 30 s of the card returning. Remove the "restarts every few seconds" sentence.
- [X] T035 [P] `docs/greptile-review-learnings.md` Part 1 "Recovery loops": add a line saying that a recovery that needs the SIM (a new IKE_SA) is gated on the card for card-reader lines, through `card_gate` beside `underlay_gate`, and that a new recovery reason must be classified in `needs_the_sim` (exhaustive).
- [X] T036 [P] `RELEASE_NOTES.md` `## Unreleased`: one bullet, with only the summary bolded. Cover the agent restarts and tunnel logins that are now paced while a card-reader SIM is out of reach, a working tunnel that is no longer torn down for a rebuild it cannot complete, the alert naming the card, and recovery within about 30 s. Say what was verified live and what by tests only.
- [X] T037 Self-review the whole diff against CLAUDE.md's checklist and learnings Part 1, recording the answers for the PR:
  - every `CardOutage` reader and writer (`grep -rn 'CardOutage\|card_hold' gsm-sip-bridge/src`);
  - every `SteadyOutcome::` match;
  - lock scope (no `runner` call under the lock);
  - shutdown during a long wait;
  - every new string against `grep -rn 'contains(' gsm-sip-bridge/src/supervise`;
  - every new constant against existing ones;
  - modem-line paths untouched (`git diff` restricted to `pcsc_reader` branches).
- [X] T038 Live per quickstart §2a–2d on the local rig, with the maintainer's agreement to recreate the rig container. Record exactly what was and was not exercised under Deviations, with IST times. Restore the rig config afterwards.
- [X] T039 `make format && make lint && make test`, and commit the docs and spec: `docs: supervisor pacing during a card-reader SIM outage`. Push with an explicit refspec (`git push -u origin 163-pcsc-outage-backoff:163-pcsc-outage-backoff`), open the PR (follow-up to #233/#235), and include the T037 answers and the live results. `greploop` and `/code-review` run only when the maintainer asks.

---

## Traceability

| Requirement | Test(s) |
|---|---|
| FR-001 | `has_card_unreachable_matches_the_marker_the_agent_emits`, `a_card_unreachable_exit_logs_the_marker` |
| FR-002 | `card_check_maps_exit_status`, `card_check_is_bounded_and_names_the_imsi`, `an_unreachable_card_exits_3_and_other_errors_exit_1` |
| FR-003 | `a_modem_line_never_classifies_as_card_unreachable`, `a_modem_line_recovers_exactly_as_before`, `card_view_none_never_calls_the_check` |
| FR-004 | `agent_delays_double_from_five_seconds_to_the_cap`, `a_card_unreachable_exit_leaves_the_csim_counters_alone` |
| FR-005 | `every_way_out_resets_every_delay` |
| FR-006 | `the_wait_returns_early_when_the_card_is_back` |
| FR-007 | `tunnel_attempts_go_at_once_then_every_60_then_120_seconds`, `card_out_paces_initiations_for_a_missing_ike_sa` |
| FR-008 | `after_card_tick` tests (T020), `a_recovery_that_needs_the_sim_checks_the_card_first` |
| FR-009 | `card_out_holds_a_rebuild_of_an_established_ike_sa` |
| FR-010, FR-011 | `the_uplink_hold_wins_and_the_card_gate_is_not_consulted`, `a_card_hold_starts_no_uplink_hold` |
| FR-012 | Per-line `CardOutage` (T012/T024 wiring). The check names the line's own IMSI (`card_check_is_bounded_and_names_the_imsi`) |
| FR-013 | `a_ten_minute_outage_logs_one_began_and_one_ended_line` |
| FR-014 | `tunnel_failure_text_names_the_card_while_held`, `tunnel_failure_text_is_unchanged_without_a_card_hold`, `a_supervisor_report_with_card_hold_reaches_the_folded_alert`, `card_hold_round_trips_and_old_payloads_still_parse` |
| FR-015 | No agent change apart from the marker line. The existing #233 tests stay green |

## Dependencies & Execution Order

- **Phase 1 → Phase 2 (commit 1) → US1 (commit 2) → US2 (commit 3) → US3 (commit 4) → Polish (commit 5) → PR.**
- US1 needs Phase 2 (marker, exit code).
- US2 needs US1's `CardOutage` and `card_check`.
- US3 needs both: the `Began`/`Ended` transitions come from either thread.
- Within each story, the tests marked [P] touch different files and can be written together. Implementation tasks are sequential (same files).

## Parallel Example: User Story 1

```text
T005 supervise/card_outage.rs   (CardOutage tests)
T006 supervise/card_outage.rs   (card_check tests; same file as T005, write together)
T007 supervise/sim_recovery.rs  (classification tests)
T008 supervise/orchestrate.rs   (wait tests)
```

## Parallel Example: Polish

```text
T033 docs/observability.md   T034 docs/omnikey-pcsc-vowifi.md   T035 docs/greptile-review-learnings.md   T036 RELEASE_NOTES.md
```

## Implementation Strategy

- **MVP = Phase 2 + US1 (commits 1–2).** It removes the larger flood (about 600 agent restarts an hour), and stands alone.
- **US2 (commit 3)** removes the failed carrier logins and stops a rebuild from destroying a working IKE_SA.
- **US3 + Polish (commits 4–5)** add the alert text and docs.

## Self-review answers (T037, for the PR)

- **`CardOutage` readers and writers** (`grep -rn 'CardOutage\|card_hold\|card_state' gsm-sip-bridge/src`): one per card-reader line, created in `start_vowifi_line_strongswan` (`line.pcsc_reader` only; SWu passes `None`, VoLTE never builds one). Agent thread: `on_out` + `next_agent_delay` on a card exit, `on_registered` when the run registers, `wait_for_card_or` (checks through `poll_during_outage`). Steady loop: `is_out` for the pause, `poll_during_outage` then `is_out`/`tunnel_attempt_due` for the `CardView`, `claim_check` in the gate's check closure, `after_card_tick` (`record`, `tunnel_attempted`), `is_out` for `card_hold`. `card_hold` is written only by `sync_tunnel_signal`/`send_tunnel` from that last read, and read by ingest into `last_card_hold`, used only by `alert_description`. No struct copies it at construction.
- **Every `SteadyOutcome` match:** `outcome_restarts_agent` (false), `UnderlayTracker::end_tick` (no state, pending check kept), the strongSwan loop's `match outcome` (drop the guard; a paced initiation marks the owed restart as the card's), the SWu loop (listed with the outcomes it never produces), `after_card_tick`. The `if let Held { IkeSaLost, initiated: true }` dead-peer report and `renegotiated` are not reached by `CardHeld`, so the rebuild gate opens and no rebuild is counted towards flapping. `orchestrate.rs`'s other `let hint = match outcome` is pcscd's, a different type.
- **Lock scope:** no `runner` call or sleep under the `CardOutage` lock: every lock is a temporary or a block that ends before `card_check`, `runner.sleep` or `println!`. `CardOutage` is the innermost lock (plan, corrected lock-order note).
- **Shutdown during a long wait:** `wait_for_card_or` checks `shutting_down` before every step, so shutdown waits at most one 5 s sleep plus one 5 s check, where the old 5 s sleep ignored it. A 120 s card delay no longer delays shutdown. The steady loop's shutdown handling is unchanged; its new checks run under the existing tick guard, bounded at 5 s like its `swanctl` calls.
- **New strings against classifiers** (`grep -rn 'contains(\|starts_with(' gsm-sip-bridge/src/supervise gsm-sip-bridge/src/ims`): the only new match is `has_card_unreachable` on `CARD_UNREACHABLE_MARKER`, pinned by test. No existing classifier contains "out of reach" or "SIM card"; the agent's error text is unchanged, and the marker is a separate tracing line. `has_csim_failure`/`has_at_stall` are checked first.
- **New constants against existing:** `CARD_CHECK_INTERVAL = HELD_POLL_INTERVAL`, `CARD_OUTAGE_MAX_BACKOFF = RETRY_MAX_BACKOFF`, the first agent delay is `NORMAL_RESTART_DELAY`, doubling is `next_backoff`. `CARD_CHECK_TIMEOUT` (5 s) and `CARD_TUNNEL_INITIAL_INTERVAL` (60 s) are new, with no existing equivalent (`MAX_SIM_RESETS`, `CSIM_FAIL_THRESHOLD`, `EXHAUSTED_RESTART_DELAY` untouched).
- **Modem-line paths:** with `card_state = None`, `classify_exit(log, false)` is the old stall → CSIM → other order, the registration watch is gated on `had_restart` as before, the restart wait is the old `runner.sleep`, the tick gets `CardView::none()` (proved by a panicking check in every underlay test and `a_modem_line_recovers_exactly_as_before`), `steady_pause(p, false) == p`, and `card_hold` stays `None`, so it is never sent.
- **Threads, sockets, subprocesses:** no new thread or socket. The card check is `timeout 5 … vowifi-plmn` through `runner.run`, reaped by `init: true`, and leaves the card (#232).
- **Docs:** `docs/observability.md`, `docs/omnikey-pcsc-vowifi.md`, the learnings line and the release note describe what the code does; the release note says it is covered by tests only until T038 runs, with only the summary bolded.

## Deviations (recorded during implementation)

- **T008/T012: `wait_for_card_or` lives in `supervise/card_outage.rs`, not `orchestrate.rs`,** with its tests there. It needs only the runner, the shared `CardOutage` and closures, so it sits with the state it paces. It takes an injected `clock: &dyn Fn() -> Instant` (tests advance it with the mock's recorded sleeps). A step is "check if due, then sleep", so a 20 s wait is four 5 s sleeps and Out, Out, Present returns after two. An extra test, `the_wait_ends_when_the_other_thread_saw_the_card`, covers the steady loop ending the outage while the agent thread waits.
- **T009: `CardOutage::since` is an `Instant` only** (no `SystemTime`): nothing reports the wall-clock start. `claim_check(now)` is added beside `check_due`, so the two threads cannot both run a check inside one interval. `tunnel_attempted` returns the gap to the next try, for the paced-initiation log line.
- **T006: the spawn-error case is tested through the pure `verdict_for`,** because `MockCommandRunner::run` never fails. A signal-killed check (no exit code) is also `Unknown`.
- **T012: `start_line_tail` gains `#[allow(clippy::too_many_arguments)]`** (8 parameters with the shared `CardOutage`), as other supervisor functions have.
- **T014/D5: `needs_the_sim` also excludes `PcscfChanged`.** `recovery_renegotiates(PcscfChanged)` is true, but data-model.md's own table marks it "no": it is a refresh, never issued as a recovery. The pin is `needs_the_sim(r) == recovery_renegotiates(r) && !matches!(r, ProcessDied | ViciBroken | PcscfChanged)`.
- **T022: the card gate also covers the half-open re-initiate right after a no-route hold ends** (`tick_half_open`'s `restored` path, `IkeSaNegotiationStuck`), not only the stuck path. Same class: it re-initiates without an IKE_SA to lose. A paced tick that does not initiate keeps the half-open record.
- **T022: no per-tick memo around `check`.** Every card-gate site returns, so a tick reaches at most one; `a_recovery_that_needs_the_sim_checks_the_card_first` and `a_card_hold_does_not_hold_back_the_route_repair` assert one check.
- **T022: a card-held rekey rebuild still lets the same tick's route repair run,** as an uplink-held one does (the repair needs no SIM).
- **T020: `after_card_tick`, `steady_pause` and `paced_initiation_line` live in `card_outage.rs`** with their tests (as for T008). `after_card_tick` records the gate's own verdict first, then the paced initiation, so an outage its check just found counts that initiation as its first (at once, then 60 s).
- **T024: the held-initiation restart line names the hold.** `IKE_SA re-established after a SIM card hold (…)` when the owed restart came from a paced card initiation; the documented uplink text is unchanged.
- **T023: `UnderlayTracker::end_tick(CardHeld)` leaves the pending liveness check as it is** (task text), so a recovery waiting on it does not send a second DPD when the card returns.
- **T031: `card_hold` is reported by the steady loop alone, read from the shared `CardOutage` each tick** through `sync_tunnel_signal` (the one helper), not set at each `Began`/`Ended` on both threads. The reporter belongs to the steady thread; an outage the agent thread begins reaches the daemon on the next tick (≤30 s, against a 300 s alert threshold). A card-reader line reports `Some(false)` from its first tick; a modem line never sends the field.
- **T029: one `AgentState {` literal lacked `..Default::default()`** (`tests/test_observability_ingest.rs`), not 29; every other site already used it.
- **T028: the 10-minute simulations live in `card_outage.rs`** (as for T008), and a second one, `a_ten_minute_outage_makes_at_most_six_tunnel_attempts`, pins SC-002 (attempts at 0, 60, 180, 300, 420, 540 s).
- **T039 split:** format, lint, test and the docs/spec commit are done (final run: 3521 passed, 0 failed, 11 ignored, against the 3476/11 baseline). The push and the PR wait on T038 and the maintainer's go-ahead, so T038's results can go in the PR.
- **T038 live, 2026-11-24, local OMNIKEY rig, image `gsm-sip-bridge:pr163` built from `git archive` of this branch (times IST):**
  - **2a (US1): passed.** 23:21:28 reader unbound, agent killed. The agent's last line carried `the SIM card is out of reach; exiting`; `Began` once; 9 paced restarts in about 9.5 minutes (5, 10, 20, 40, 80, 120, 120, 120, 120 s; SC-001 limit 15). 23:30:58 reader rebound; 23:31:03 `SIM card reachable again after 566s; resuming`; 23:31:07 registered (SC-003: 9 s). The tunnel stayed up throughout, so 2a made no tunnel initiations.
  - **2b (US2, FR-009): held rebuild seen.** `gm_options_keepalive = true`, recreated. 23:31:45 reader unbound and the only P-CSCF routed to the silent veth peer. 23:33:54 the agent's `gm-keepalive` mark made PcscfPoolExhausted due; the card gate's own check found the card out (`Began`) and held it. 23:34:24 one DPD from the underlay gate (it runs first, FR-010) for the same pending rebuild. Through 23:39:36: no `deleting IKE_SA`, 0 `initiating IKE_SA`, no `SCardConnect` burst, 0 agent exits. Paced initiations for a missing IKE_SA were not exercised live (the IKE_SA was never lost): tests only.
  - **2b end:** through 23:44:14, 10 min 20 s after the hold began: 0 `initiating IKE_SA`, 0 `deleting IKE_SA`, 0 `SCardConnect`, 0 agent exits, 1 DPD in all. The agent's mark had lapsed by then (`marks: []`), so the held rebuild was no longer due. 23:44:37 route removed and reader rebound; 23:44:41 `reachable again after 646s` (4 s).
  - **FINDING (SC-003 missed in 2b), FIXED in 977f244 and verified live; see the re-run below: registered at 23:46:24, 107 s after the card returned** (limit 30 s). In 2b the agent never exited: it rode the outage out in-process (#233) and retried its full registration on its own backoff (up to 120 s). The supervisor's `Ended` only shortens the supervisor's own waits, and nothing nudges a live, unregistered agent. 2a met SC-003 (9 s) only because the agent had exited. **Not fixed and not accepted as a limitation:** waiting on the maintainer. The release note and `omnikey-pcsc-vowifi.md` were corrected to say what happens now rather than "about 30 seconds".
  - **2c (US3): not exercised live.** The tunnel never went down in 2b (`uplink: up`, CHILD_SA up), so the tunnel-failure alert's condition was never met. The registration went `Failed` (the registration-loss alert's domain, #233). The suffix is covered by tests only.
  - **2d (no regression): partly.** With the card in, the line registered at both container starts (23:21, 23:31), and the supervisor logged no card lines before the outages. No in-place refresh cycle was waited for (about 50 min).
  - **Re-run with the SC-003 fix, 2026-11-25, image `gsm-sip-bridge:pr163b` (977f244):** keepalives on; 00:07:43 reader unbound and the P-CSCF black-holed; the outage began and the PcscfPoolExhausted rebuild was held as before; the registration went `Failed` at 00:09:55, and the agent's own retry reached `retry_in_secs=120`. 00:13:10 route removed and reader rebound: `reachable again after 195s`, then `SIM card back but vowifi-ims-agent is not registered; restarting it now`, and the held rebuild ran (`recovered from PcscfPoolExhausted`). **Registered at 00:13:21, 11 s after the card returned** (SC-003). The rig was restored again at 00:13:37 (`pr235`, original config).
  - **Greptile (#236) fixed in 7ed29da:** a held `TunVanished` is now gated before `recreate_interface`, so it is not forgotten, and the agent's card wait is measured against a deadline on the clock. Neither path was exercised live (tests only).
  - **Greptile (#236, 2nd review) addressed:** the post-outage restart is skipped while every P-CSCF in the agent's pool still carries a mark in force (read from the same status reply), because a new agent would wait those marks out (up to 120 s) before its first attempt. In the re-run above the mark was still in force when the card returned, so under this rule that restart would be skipped and the line would recover through the held PcscfPoolExhausted rebuild, which ran on the same tick and restarts the agent on a fresh P-CSCF. The first run's 107 s case had no marks left (`marks: []`), so the restart still covers it. Tests only for the skip.
  - **Rig restored** 23:46:48: `config.toml` and `docker-compose.yaml` from the `*.pre-163` backups (`gm_options_keepalive` off, image `gsm-sip-bridge:pr235`), recreated, and registered.
  - Test-method note: the rig runs `pid: host`, so `pkill -f` inside it also matched a host shell whose command line held the pattern. Use the supervisor's anchored `vowifi-ims-agent --line N$`.
- **Mutations checked (#233 practice):** dropping the `CardUnreachable` arm of `exit_status_for` fails `an_unreachable_card_exits_3_and_other_errors_exit_1`; always returning the marker fails `a_card_unreachable_exit_logs_the_marker`; dropping the delay reset on the end of an outage fails `every_way_out_resets_every_delay`; turning the card gate's hold into a pass-through fails four tests including `card_out_holds_a_rebuild_of_an_established_ike_sa`; classifying `TunVanished` as not needing the SIM fails `needs_the_sim_is_recovery_renegotiates_minus_the_shared_daemon`; delaying the first paced initiation fails `a_recovery_that_needs_the_sim_checks_the_card_first`; dropping the half-open record on a paced non-initiating tick fails `card_out_paces_initiations_for_a_missing_ike_sa`; dropping the ingest of `card_hold` fails `a_supervisor_report_with_card_hold_reaches_the_folded_alert`.
