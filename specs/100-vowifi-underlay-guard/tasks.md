# Tasks: VoWiFi underlay guard and the half-open IKE_SA fix (100)

**Input**: `specs/100-vowifi-underlay-guard/` (spec.md, plan.md, research.md, data-model.md, contracts/, quickstart.md)

**Tests**: required. The constitution defaults to TDD: write each test before the code it covers, and watch it fail first.

Paths are relative to `gsm-sip-bridge/src/` unless stated otherwise.

Before every commit:
- run `make format && make lint && make test` with `CARGO_TARGET_DIR=$PWD/target-100` (shared-target-dir memory);
- end the message with the Co-Authored-By trailer.

Fixtures use synthetic addresses only (`10.0.0.x`, `192.0.2.x`, `203.0.113.x`, `2001:db8::x`) and synthetic IMSIs (`001010000000001`). Real charon log lines captured from the rigs must have their IMSI/NAI and addresses rewritten to these before they go into a test.

**Order note**:
- US3 (half-open fix) is implemented first. It stands alone, it is the smallest shippable piece, and the decision table in US1/US2 builds on its new health state.
- US1 and US2 share one decision table, so they are implemented together. US2's phase holds the tests that prove the guard does *not* hold real faults.
- US4 (observability) follows.
- The live phases run last, on one image containing everything.

## Phase 1: Setup

- [X] T001 Confirm the branch is `100-vowifi-underlay-guard` on top of `origin/main` (`f500c88`), with no upstream set, and that `make test` is green before any change. Commit the plan artifacts (plan.md, research.md, data-model.md, contracts/, quickstart.md, tasks.md, the updated spec.md and checklist, and the `CLAUDE.md` SPECKIT pointer) as `docs(100): plan and tasks for the underlay guard (#141)`.

## Phase 2: Foundational (blocking for US1/US2/US3)

**Purpose**: the strongSwan DPD command, the log/listing parsers, and the engine seam that every story reads through.

- [X] T002 Write `docker/patches/0002-strongswan-vici-dpd.patch` against the fork at `022f3e515`, per [contracts/dpd-command.md](contracts/dpd-command.md):
  - a `dpd` vici command in `src/libcharon/plugins/vici/vici_control.c`, registered next to `redirect` (checkout by name, refuse unless `ESTABLISHED`, queue `ike_dpd_create(TRUE)`, `initiate(NULL…)`, checkin; reply `success`/`ike-id`/`errmsg`);
  - a new `src/swanctl/commands/dpd.c` (`--dpd`/`-D`, `--ike`, `--raw`/`--pretty`), modelled on `redirect.c`;
  - an entry in `src/swanctl/Makefile.am`.
  
  Generate the patch from a clean checkout (`git diff`) so it applies with `patch -p1`.
- [X] T003 Apply the patch in `docker/Dockerfile`'s `strongswan-builder` stage: `COPY docker/patches/0002-strongswan-vici-dpd.patch /tmp/`, then `patch -p1 < /tmp/0002-…` after the checkout and before `autoreconf -if`. Extend the stage comment to say why (FR-004a, research R5). Build the image locally (`docker build` per the existing Makefile/compose target) and confirm `swanctl --help` lists `--dpd`.
- [X] T004 [P] In `supervise/engines.rs`, tests first, using real `swanctl --list-sas` text: `ike_sa_header(sas, conn) -> Option<(u32 /*#N*/, IkeHeaderState)>`, where `IkeHeaderState` is `Established | Connecting | Other(String)`.
  - Cases: `ESTABLISHED` and `CONNECTING` headers for `ims0`;
  - `ims10` not matched by `ims1`;
  - another connection's header ignored;
  - empty input → `None`.
  
  Keep `list_sas_has_ike_sa` as is.
- [X] T005 [P] In `supervise/engines.rs`, tests first, using real charon log lines (R4): `last_ike_activity(log, conn) -> Option<String /*timestamp*/>`, the newest `<conn|N>` line that is `generating …`, `parsed …` or `retransmit N of request`. Cases:
  - an IKE_SA_INIT retry cycle;
  - a DPD exchange;
  - lines for `ims1` ignored when asking about `ims0`;
  - no lines → `None`.
  
  Parse the log's `YYYY-MM-DD HH:MM:SS` prefix to a `SystemTime` in UTC, matching how charon writes it in the container. Add a test that pins the timezone assumption.
- [X] T006 [P] In `supervise/engines.rs`, tests first: `ike_evidence_since(log, conn, mark) -> IkeEvidence { parsed_from_peer: bool, unanswered_retransmits: u32 }` over the log text after a `LogMark`. Cases:
  - a parsed response clears the retransmit count;
  - an ePDG-initiated `parsed INFORMATIONAL request` counts as life;
  - 2 retransmits and nothing parsed;
  - `generating` with no retransmit yet;
  - another connection's lines ignored.
  
  Reuse `lines_for_conn` scoping.
- [X] T007 [P] In `supervise/engines.rs`, tests first: `route_to(runner, addr) -> bool` running `ip route get <addr>` in the host namespace through `CommandRunner`. It is true only on exit 0 *with* a `src` token. Mock cases: normal output, the three RTNETLINK failures from research R3, `Network is unreachable`, and exit 0 without `src`. Justify the mock in a one-line comment pointing at the existing `MockCommandRunner` rationale.
- [X] T008 Extend the `TunnelEngine` trait in `supervise/line_supervisor.rs` with default implementations, and implement them on `StrongswanEngine` in `supervise/engines.rs`:
  - `fn epdg_addr(&self) -> Option<&str>` (default `None`); `StrongswanEngine` stores the resolved `epdg_ip` from `orchestrate.rs` line setup;
  - `fn route_ok(&self, runner) -> bool` (default `true`);
  - `fn ike_header(&self, runner) -> Option<(u32, IkeHeaderState)>` (default `None`), reading the listing cached by `steady_state_health` without listing again;
  - `fn last_ike_activity(&self, runner) -> Option<SystemTime>` (default `None`);
  - `fn ike_evidence_since(&self, runner, mark) -> IkeEvidence` (default: life seen);
  - `fn send_dpd(&self, runner) -> bool` (default `false`), which runs `swanctl --dpd --ike <conn>` under the same bounded-timeout wrapper as `list_sas`.
  
  Give `FakeEngine` (`line_supervisor.rs` tests) scripted fields for each. `SwuEngine` keeps the defaults. Unit-test `send_dpd`'s argv and its exit-code handling with `MockCommandRunner`.
- [X] T009 `make format lint test`; commit `feat(supervise): parsers and engine seam for the underlay guard, and an on-demand DPD in the strongSwan fork (#141)`. If the image build in T003 makes the commit heavy, split the patch and Dockerfile into their own commit first: `build(strongswan): add swanctl --dpd to the fork (#141)`.

**Checkpoint**: the image has `swanctl --dpd`, and the parsers and engine seam exist. No behaviour has changed yet.

## Phase 3: User Story 3 — leave an in-flight IKE_SA negotiation alone (P1)

**Goal**: data-model rows 3–4, spec FR-014–FR-017. **Independent test**: tick unit tests with a scripted half-open listing (SC-007).

- [X] T010 [US3] In `supervise/line_supervisor.rs` add `SteadyStateHealth::IkeSaConnecting { ike_id: u32 }`, `DegradeReason::IkeSaNegotiationStuck`, `SteadyOutcome::WaitingForNegotiation`, `HALF_OPEN_STUCK_AFTER: Duration = 200s` (derivation in the doc comment, R6), and `SteadyTracking.half_open: Option<HalfOpen { ike_id, waiting_logged }>`. Make `recovery_restarts_agent` and `recovery_renegotiates` true for it. Add the label to `From<DegradeReason> for TunnelRecoveryReason` and `TunnelRecoveryReason::IkeSaNegotiationStuck` (`ike_sa_negotiation_stuck`) in `control/protocol.rs`, with a wire round-trip test.
- [X] T011 [US3] In `StrongswanEngine::steady_state_health` (`supervise/engines.rs`), tests first: a listing whose header is `CONNECTING` returns `IkeSaConnecting { ike_id }` instead of `ChildSaMissing`. An `ESTABLISHED` header without a CHILD_SA still returns `ChildSaMissing`. Update the existing assertion at the `connecting` seed (it currently expects `ChildSaMissing`) and its comment, which cites 087 R1.
- [X] T012 [US3] In `tick_steady_state_with_pool`, tests first through `FakeEngine`:
  - (a) `IkeSaConnecting` with last activity 10s ago → `WaitingForNegotiation`, with no `reinitiate`/`terminate` calls and the log flag set once per `#N`;
  - (b) the same `#N` on later ticks with activity → still waiting, no second log;
  - (c) last activity 201s ago → `terminate` + `reinitiate`, `Recovered { IkeSaNegotiationStuck }`;
  - (d) a new `#N` resets `waiting_logged`;
  - (e) `ChildSaMissing` under an `ESTABLISHED` header keeps its two strikes (086, #107);
  - (f) no activity line at all → measured from when `#N` was first seen.
  
  Then implement. Inject "now" through a closure argument or an existing seam rather than reading the clock inside the decision.
- [X] T013 [US3] In `supervise/orchestrate.rs`, handle `WaitingForNegotiation` in the steady loop's `match outcome`: open the rebuild gate, no agent restart, `sync_tunnel_signal` as for `StillUp`. Log `ims0 IKE_SA #N still negotiating; leaving it to charon` once per `#N` (contracts/observability.md). The stuck recovery flows through the existing `Recovered` branch.
- [X] T014 [US3] `make format lint test`; commit `fix(supervise): leave a half-open IKE_SA to charon instead of terminating it as a missing CHILD_SA (#141)`.

**Checkpoint**: US3 is shippable alone. In an outage the supervisor no longer kills its own in-flight IKE attempts.

## Phase 4: User Story 1 — an uplink outage doesn't churn the line (P1)

**Goal**: data-model rows 5–7 and the hold lifecycle, spec FR-001–FR-013. **Independent test**: decision-table and lifecycle unit tests. Live: quickstart §2–§3.

- [X] T015 [US1] In `supervise/line_supervisor.rs` add:
  - `UnderlayVerdict { Up, NoRoute, NoResponse }`;
  - `UnderlayView { enabled, verdict, route_ok }` with `UnderlayView::disabled()`;
  - `SteadyOutcome::Held { cause, skipped: DegradeReason }`;
  - an exhaustive `fn is_reachability_inferred(DegradeReason) -> bool` (no wildcard arm), per data-model.md's table.
  
  Unit-test the classification of every variant.
- [X] T016 [US1] Thread `underlay: &UnderlayView` into `tick_steady_state_with_pool`, and add a `tick_steady_state` wrapper argument passing `UnderlayView::disabled()`, so existing tests are unchanged. Tests first:
  - every reachability-inferred recovery the tick would take becomes `Held` with no engine call, when the verdict is `NoRoute` or `NoResponse`: `PcscfUnreachable` after its strikes, `PcscfPoolExhausted`, `ChildSaMissing`, `ChildSaStuck`, `ChildSaRekeyStuck` and `IkeSaNegotiationStuck`;
  - `IkeSaLost` while held: the first tick issues exactly one `reinitiate` (FR-010a) and reports it as a held initiation; a later tick with still no IKE_SA issues none until an IKE_SA has been listed again;
  - structural reasons (`ProcessDied`, `ViciBroken`, `TunVanished`, `TunUnavailable`, `DefaultRouteMissing`) still run while held (FR-009);
  - `Up` behaves exactly as before.
  
  Track `held_initiate_sent` in `SteadyTracking`.
- [X] T017 [US1] Data-model row 7, tests first: with verdict `Up`, `route_ok == false` and a reachability-inferred recovery pending this tick → `Held { cause: NoRoute }` immediately. With `route_ok == false` and nothing pending → `StillUp` (no hold yet; debounce is the loop's job).
- [X] T018 [US1] In `supervise/orchestrate.rs` add `UnderlayTracker` (data-model.md): route-failure debounce (2 failures ≥10s apart, FR-002), `hold: Option<Hold>`, and `pending_check` (filled in US2). Before each tick, compute the verdict:
  - `engine.route_ok()` first; `NoRoute` takes precedence;
  - then `NoResponse` from `engine.ike_evidence_since(mark_of_last_life)`: ≥2 unanswered retransmits and no life;
  - any life → `Up` (FR-006).
  
  Build the `UnderlayView`. When `[vowifi].underlay_guard` is off (T034), always pass `UnderlayView::disabled()`. Factor the verdict computation as a pure function over (previous tracker, route result, evidence, now) and unit-test it: debounce, precedence, release on life, release on route.
- [X] T019 [US1] Hold lifecycle in the steady loop (`supervise/orchestrate.rs`), with the transitions as pure methods on `UnderlayTracker` and tests first:
  - **On entry:** log the hold-start line with its evidence (contracts/observability.md).
  - **While held:** handle `SteadyOutcome::Held`: no `pkill`, no `RebuildWindow::record`, record `skipped`, open the rebuild gate.
  - **On exit when the path returns:** log the hold-end line with duration and the deduplicated skipped reasons; set `tracking.negotiated_at = Some(now)` (FR-011); reset `held_initiate_sent`; if the engine reports neither an established nor a half-open IKE_SA, issue one `engine.reinitiate` (FR-013).
  - **At 15 minutes** (FR-012): end the hold with `ended_by=max_hold`, let this tick run unheld once, and let a new hold start on fresh evidence.
  - **On shutdown while held:** end with `ended_by=shutdown`.
- [X] T020 [US1] Handle `SteadyOutcome::Held` in the loop's `match` exactly once per tick, under the existing shutdown guard. Confirm by test or by inspection that `sync_tunnel_signal` keeps `child_sa_up` truthful (down) during a hold, and that `flapping` cannot latch, since no `record` happens.
- [X] T021 [US1] `make format lint test`; commit `feat(supervise): hold reachability-inferred recoveries while the path to the ePDG is down (#141)`.

**Checkpoint**: with the guard on, a no-route or no-response outage no longer churns the line. This is the MVP for #141.

## Phase 5: User Story 2 — a real carrier fault is still repaired (P1)

**Goal**: data-model rows 6 and 8, spec FR-004/FR-004a, US2 scenarios. **Independent test**: tick unit tests. Live: quickstart §4.

- [X] T022 [US2] Add `SteadyOutcome::Deferred { reason }` and the pending-check input in `supervise/line_supervisor.rs`. `UnderlayView` gains `pending: Option<PendingView { reason, life_since_mark: bool }>` and `recent_life: bool` (a parsed IKE message or inbound ESP younger than 30s). Tests first:
  - Row 8: verdict `Up`, a reachability-inferred recovery R ≠ `IkeSaLost`, an `ESTABLISHED` IKE_SA and no recent life → `engine.send_dpd()` called once, `Deferred { R }`, no recovery issued.
  - Row 8 with recent life → R is taken at once (no DPD).
  - Row 6: a pending check for R with `life_since_mark` → R is taken; without it → `Held { cause: NoResponse }`.
  - A second suspicion while pending reuses it (no second `send_dpd`).
  - `send_dpd()` returning false still defers, and the next tick decides from the log alone.
- [X] T023 [US2] In `supervise/orchestrate.rs`, fill `UnderlayTracker.pending_check`:
  - on `Deferred`, store the `LogMark` taken just before the tick and `deadline = now + 30s`, and log `checking the ePDG is alive before R (DPD sent)`;
  - on the next tick, compute `life_since_mark` from `engine.ike_evidence_since(mark)`, the inbound ESP age and the tunnel RX counter (research R4);
  - log `ePDG answered; proceeding with R`, or start a `NoResponse` hold;
  - clear the pending check when the recovery runs, the hold starts, or the IKE_SA disappears.
- [X] T024 [US2] Regression tests in `supervise/line_supervisor.rs` mirroring US2's scenarios through `FakeEngine`:
  - (1) single-P-CSCF pool exhausted while the ePDG answers → DPD, then a rebuild on the next tick, never `Held`;
  - (2) namespace default route missing → `DefaultRouteMissing` repaired on the same tick, whatever the verdict;
  - (3) `ProcessDied`/`ViciBroken`/`TunVanished` while held → recovered;
  - (4) `IkeSaLost` with cause `CarrierDelete` and verdict `Up` → re-initiated as today.
- [X] T025 [US2] `make format lint test`; commit `feat(supervise): confirm the ePDG is alive with one on-demand DPD before a reachability-inferred rebuild (#141)`.

## Phase 6: User Story 4 — the operator can see the uplink was at fault (P2)

**Goal**: [contracts/observability.md](contracts/observability.md) and [contracts/status-file.md](contracts/status-file.md), spec FR-018–FR-023. **Independent test**: wire/ingest unit tests plus the control-socket integration test.

- [X] T026 [P] [US4] In `control/protocol.rs`, tests first:
  - `UplinkState { Up, NoRoute, NoResponse }` (snake_case);
  - `AgentState.uplink: Option<UplinkState>` with `skip_serializing_if`/`default`;
  - `ObservedEvent::UplinkHoldStarted { cause }` and `UplinkHoldEnded { cause, held_secs, ended_by }`, with `UplinkHoldCause { NoRoute, NoResponse }` and `HoldEndedBy { Restored, MaxHold, Shutdown }`;
  - `DeadPeerVerdict::Uplink` (`uplink`).
  
  Round-trip each, check an old payload without the fields still parses, and check that existing wire shapes are unchanged.
- [X] T027 [US4] In `metrics/mod.rs`, register `gsm_sip_bridge_vowifi_uplink_holds_total{module,cause}`, `…_uplink_held_seconds_total{module,cause}` and gauge `…_uplink_held{module}`. In `metrics/ingest.rs`:
  - count the two events;
  - set the gauge from `AgentState.uplink` on supervisor reports, leaving it untouched when absent;
  - keep `last_uplink: Option<UplinkState>` on the supervisor record;
  - extend `tunnel_failure_description` with an `uplink` argument that appends `; uplink down (no route|no response), recovery on hold` only when held.
  
  Tests first: every existing description is byte-identical when `uplink` is `Up`/`None`; both suffixes; the counters and gauge per event or state.
- [X] T028 [US4] In `supervise/orchestrate.rs`:
  - add `uplink` to `TunnelSignal` and `sync_tunnel_signal`, so a verdict change is reported once; with the guard off, never set the field;
  - emit `UplinkHoldStarted`/`UplinkHoldEnded` from the T019 transitions through `send_tunnel`;
  - when a held tick sees `IkeSaLost` with cause `DeadPeer`, report `dead_peer: Uplink` instead of the 098 classification (FR-022).
  
  Unit-test the signal change detection.
- [X] T029 [P] [US4] In `ims/pcscf_files.rs`, tests first: `uplink_path(selection_path)` (`<path>.uplink`), `render_uplink(state, since) -> String` and `parse_uplink(&str) -> Option<(UplinkState, Option<u64>)>` per contracts/status-file.md. Reject unknown `v`, malformed JSON and an unknown `state`. Use `write_atomic`.
- [X] T030 [US4] Write the uplink file from `supervise/orchestrate.rs` at line start (`up`) and on every verdict change. Do not write it on swu lines or with the guard off.
- [X] T031 [US4] In `vowifi/mod.rs` `print_status`, tests first via a pure `format_uplink_line(parsed: Option<…>, now) -> String`: `uplink: up`; `uplink: no_route (held 3m12s, recovery on hold)`; `uplink: unknown` for a missing or unparseable file. Print it under each line after the registration block, and never change the exit code.
- [X] T032 [US4] Extend `gsm-sip-bridge/tests/test_vowifi_tunnel_recovery_metrics.rs`: a real reporter sends:
  - a supervisor state `uplink=no_route`, then `uplink_hold_started{no_route}`, then `uplink_hold_ended{no_route, 312, restored}` with `uplink=up`;
  - `TunnelRecovered{IkeSaNegotiationStuck}`;
  - `TunnelRecovered{IkeSaLost, dead_peer: uplink}`.
  
  Assert the registry: holds=1, held_seconds=312, the gauge 1 then 0, a recovery counted with `reason="ike_sa_negotiation_stuck"`, and a dead-peer loss with `verdict="uplink"`.
- [X] T033 [US4] `make format lint test`; commit `feat(observability): report uplink holds in metrics, alert text and vowifi-status (#141)`.

## Phase 7: Configuration (FR-025/FR-025a, SC-008)

- [X] T034 Tests first in `config/mod.rs` `mod tests`: `underlay_guard` defaults to `true`; `false` is accepted; `"yes"`, `1` and `"true"` are rejected with `vowifi.underlay_guard` in the error; it is not a per-line key. Then add `underlay_guard: bool` to `RawVowifi` (`config/raw.rs`, default `true`), `VowifiConfig` (`config/mod.rs`, doc comment per contracts/config.md) and `build_vowifi` (`config/build.rs`).
- [X] T035 Wire it in `supervise/orchestrate.rs`: off → `UnderlayView::disabled()`, no `uplink` field, no status file, no `send_dpd`. Log the startup line once per deployment, with swu wording too (contracts/config.md). Add a pure `underlay_guard_log_line(&VowifiConfig) -> String` next to 098's `dpd_log_line`, log it with `tracing::info!` beside the `dpd_log_line` call, and unit-test its three wordings. Add a loop-level test, using whatever seam 098 used for its loop tests (otherwise a pure helper), proving that with the guard off a `PcscfPoolExhausted` tick behaves exactly as before.
- [X] T036 `make format lint test`; commit `feat(vowifi): [vowifi].underlay_guard, on by default (#141)`.

## Phase 8: Polish (docs)

- [ ] T037 [P] Docs:
  - `docs/configuration.md`: `[vowifi]` row for `underlay_guard`, in the `dpd_delay_sec` row's style;
  - `config.toml.example`: commented `# underlay_guard = true`;
  - `docs/observability.md`: the three metrics and the two new label values;
  - `docs/operations.md`: a new section "VoWiFi: `uplink down … holding recovery`", covering what is held and what never is, how to read `vowifi-status` and the alert suffix, the 15-minute bound and the kill switch; plus a note in the "giving up after 5 retransmits" section that during a hold the dead-peer verdict reads `uplink`;
  - `CHANGELOG.md` under Unreleased: two entries, the guard and the half-open fix, with only the summary bold (release-notes memory).
  
  `tests/test_config_docs.rs` must pass.
- [ ] T038 `make format lint test`; commit `docs: the VoWiFi underlay guard and the half-open fix (#141)`.

## Phase 9: Live verification (SC-001…SC-008)

Build this branch's image for amd64 (Vodafone, local) and arm64 (Jio, via `arm-build-200`), with the P-CSCF probe forced off as in the 098 baseline. Never push the throwaway commit. Recreate the containers; don't restart them.

- [ ] T039 Quickstart §1 on both rigs: `swanctl --dpd --ike ims0` exits 0, and a `parsed INFORMATIONAL response` follows within 2s.
- [ ] T040 [US3] [US1] Quickstart §2 (no-route) on both rigs: SC-001/002/003, the status line mid-outage, and no `CONNECTING` IKE_SA terminated while active.
- [ ] T041 [US1] Quickstart §3 (upstream-dead) on both rigs: SC-001/002/003, with a DPD sent and then `no_response` within 120s.
- [ ] T042 [US2] Quickstart §4 on Vodafone: the single-P-CSCF black-hole recovers within the 097 time + 30s with no hold, and a namespace route deletion is repaired in one tick (SC-004).
- [ ] T043 [US1] Quickstart §5 (16.5-minute outage) on Vodafone: exactly one `max_hold` recovery, then a new hold, and recovery within 30s of restore (SC-005).
- [ ] T044 Quickstart §6 (guard off) on Vodafone: the R1 baseline is reproduced, apart from the half-open fix (SC-008).
- [ ] T045 Quickstart §7 and §8: the idle-hour outer packet count is unchanged (SC-006), and the metrics read as expected after T040/T041.
- [ ] T046 Write `specs/100-vowifi-underlay-guard/live-verification.md` (SC-001…SC-008 with timestamps; addresses and identities redacted to roles). Commit `docs(100): live verification of the underlay guard (#141)`.

## Dependencies

- **T001 → Phase 2.** T002 → T003. T004–T007 are parallel. T008 needs T004–T007. T009 closes the phase.
- **Phase 3 (US3)** needs T004/T005/T008. It is independent of US1/US2/US4, so it can ship alone.
- **Phase 4 (US1)** needs Phase 2 and T010 (`IkeSaNegotiationStuck` is one of the held reasons).
- **Phase 5 (US2)** needs Phase 4 (`UnderlayView`, the tracker, the hold) and T008's `send_dpd`.
- **Phase 6 (US4)** needs T019 (the transitions that emit events). T026 and T029 can start any time after Phase 2.
- **Phase 7** needs Phase 4 (what the switch turns off). It can run in parallel with Phase 6.
- **Phase 8** needs Phases 3–7.
- **Phase 9** needs everything, including an image with T003's patch.

## Parallel examples

- **Phase 2:** T004, T005, T006 and T007 are separate pure functions with their own tests in `engines.rs`. Write them as separate test modules to avoid edit conflicts, then T008 wires them.
- **Phase 6:** T026 (`control/protocol.rs`) and T029 (`ims/pcscf_files.rs`) touch different files and can go in parallel. T027 waits on T026, T031 on T029.
- **Phase 8:** T037's doc files are independent of each other.

## Implementation strategy

1. **MVP = Phase 2 + Phase 3 + Phase 4.**
   - The half-open fix alone removes the self-inflicted churn.
   - The hold removes the rest for the *no route* case and for any *no response* evidence from retransmits.
   - Without Phase 5, a no-response suspicion with no retransmits yet would never trigger a DPD. Rows 7–8 fall through to existing behaviour, which is safe and simply today's behaviour.
2. **+ Phase 5:** the upstream-dead case is caught early, and real faults are proven unaffected (US2).
3. **+ Phases 6–8:** observability, the kill switch and docs, so it can ship to production.
4. **Phase 9** gates the merge.
