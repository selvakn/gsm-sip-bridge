# Tasks: Carrier Signaling Connection Liveness & Automatic Reconnect

**Input**: Design documents from `/specs/028-gm-tcp-reconnect/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/

**Tests**: Included and non-optional — Constitution Principle I (Integration-First
Testing) and the Development Workflow's TDD default both require them. No new
mocks are introduced; tests use real `TcpListener`/`SipTransport` peers.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel — different files, no dependency on a sibling `[P]` task
- **[Story]**: US1–US4 per spec.md, or `FOUND`/`POLISH`
- Paths are repo-relative

## Constitution gate — applies to every phase

`make format && make lint && make test` must pass before **every** commit
(CLAUDE.md, Principle II). `make lint` covers the whole workspace including test
targets. Each phase below ends at a commit-sized checkpoint that is green on its
own.

---

## Phase 1: Foundational (Blocking Prerequisites)

**Purpose**: The three primitives every story needs. Nothing is wired into
`dispatch_loop` yet, so this phase changes no runtime behaviour.

**⚠️ No user story work can begin until this phase is complete.**

- [x] T001 [P] [FOUND] Add `alive: Arc<AtomicBool>` to `GmServer` and a `pub fn is_alive(&self)` accessor in `gsm-sip-bridge/src/ims/sip_client.rs` (~line 937). Initialise `true`. Keep it **separate** from the existing `stop` flag — `stop` is an instruction to the loop, `alive` is a report from it; conflating them makes a clean shutdown look like a crash (contracts/observability-protocol.md §3).
- [x] T002 [FOUND] Store `alive = false` on the fatal-exit path of `spawn_gm_tcp_server`'s accept loop in `gsm-sip-bridge/src/ims/sip_client.rs` (~line 1023, the `"Gm server accept failed; stopping"` branch) and on the UDP loop's equivalent. Depends on T001.
- [x] T003 [P] [FOUND] Add `build_options()` to `gsm-sip-bridge/src/ims/sip_client.rs`, modelled on `build_in_dialog_request` (`call.rs:792`). Out-of-dialog framing exactly per contracts/observability-protocol.md §2 — `To` carries no tag, `CSeq` method token is `OPTIONS`, `Content-Length: 0`.
- [x] T004 [P] [FOUND] Add constants to `gsm-sip-bridge/src/ims/agent.rs` beside `RENEWAL_HEADROOM` (~line 99): `PING_INTERVAL = 120s`, `PING_RESPONSE_TIMEOUT = 10s`, `MAX_RECONNECT_ATTEMPTS = 3`. Document the rationale inline per data-model.md.
- [x] T005 [FOUND] Add `PingState` / `PendingPing` structs and the pure `fn verdict(&self, now: Instant, call_in_progress: bool) -> PingVerdict` to `gsm-sip-bridge/src/ims/agent.rs`. No I/O — takes `now`, so tests never sleep. Depends on T004.
- [x] T006 [P] [FOUND] Add `GmConnectionState` enum (`Up` / `Reconnecting { since, attempts }` / `Failed { since }`) to `gsm-sip-bridge/src/ims/mod.rs` beside `RegistrationState` (~line 327), plus a `Display`/render helper producing the strings in contracts/status-reply.md.
- [x] T007 [FOUND] Add `gm_connection: GmConnectionState` to `ims::RegistrationStatus` (`gsm-sip-bridge/src/ims/mod.rs:339`), defaulting to `Up` — **not** an `Unknown` state, since the registration that just completed is itself a successful round trip (data-model.md). Depends on T006.

### Tests for Phase 1

- [x] T008 [P] [FOUND] Unit tests for `PingVerdict` in `gsm-sip-bridge/src/ims/agent.rs`'s `#[cfg(test)]` module: `Idle` during a call, `Send` when the interval elapsed, `Await` inside the response deadline, `Dead` past it, and that no second ping is sent while one is pending. Depends on T005.
- [x] T009 [P] [FOUND] Unit test for `build_options()` in `sip_client.rs`'s test module: asserts the `CSeq` method token, the absent `To` tag, and `Content-Length: 0`. Depends on T003.
- [x] T010 [FOUND] New integration test `gsm-sip-bridge/tests/test_gm_connection_liveness.rs`: bind a real port, `spawn_gm_server`, force the accept loop's fatal path, assert `is_alive()` flips to `false`; and assert a normal `Drop` does **not** flip it (regression guard for the stop/alive conflation in T001). Depends on T002.

**Checkpoint**: Primitives exist and are tested; runtime behaviour unchanged. Commit.

---

## Phase 2: User Story 1 — Silently-dropped connection recovers on its own (P1) 🎯 MVP

**Goal**: A registered idle line whose Gm connection is dropped without notice
detects it within ~130s and rebuilds itself, with no restart and no operator action.

**Independent Test**: Kill a registered line's Gm connection from outside the
process without a graceful close; confirm the line detects, reconnects, and can
place/receive calls again unaided.

### Detection (FR-001, FR-002, FR-003, FR-022)

- [x] T011 [US1] Wire the ping send into `dispatch_loop`'s idle branch in `gsm-sip-bridge/src/ims/agent.rs` (~line 1706, the `RecvTimeoutError::Timeout` arm), after the renewal-due check so a proceeding renewal wins (FR-011/R11). Send via `SipTransport::send` only. **`send_and_recv` is prohibited here** — the reader thread owns the read half and a second reader corrupts SIP framing (research.md R1). Depends on T003, T005.
- [x] T012 [US1] Match the ping response at the existing `SipMessage::Response` arm (`agent.rs:1688`), comparing the numeric part of the response's `CSeq` against `PendingPing.cseq`. Treat **any** final response including 4xx/5xx as alive; ignore non-matching CSeqs. Depends on T011.
- [x] T013 [US1] Fold send errors into a `Dead` verdict at the T011 call site (FR-022) — a reset is not always visible on `send`, and a blackholed connection is only revealed by the absent response, so both paths must converge. Depends on T011.
- [x] T014 [US1] Clear `PingState.pending` wherever `*session` is replaced (`agent.rs:1770-1774`, the renewal success path). **Mandatory, not defensive**: a CSeq from the old session can never be answered on the new one and would score a spurious failure ~10s after a *successful* renewal (research.md R11). Depends on T011.
- [x] T015 [US1] Poll `inbound._server.is_alive()` in the same idle branch and score a listener death as a connection failure (FR-021). Depends on T002, T011.

### Repair (FR-004, FR-005, FR-008, FR-009)

- [x] T016 [US1] Add `restart_gm_server(session, inbound)` to `gsm-sip-bridge/src/ims/session.rs`, mirroring `restart_client_reader` (~line 139): re-run `spawn_gm_server` on the same `gm_server_addr()` with a fresh `tx.clone()`, replace `inbound._server`. Document that the port is free because the `TcpListener` is moved into the accept thread. Depends on T001.
- [x] T017 [US1] On a dead verdict, call `RegisteredSession::reconnect_transport` (`ims/mod.rs:235`) then `restart_client_reader` for the client half, or `restart_gm_server` for the listener half. Set `gm_connection = Reconnecting { since, attempts }`. Depends on T013, T015, T016.
- [x] T018 [US1] Implement the confirming ping (FR-009/R7): after a successful reconnect do **not** mark the connection up — send a fresh ping immediately and mark `Up` only when its response arrives. Without this, a rebuild over a dead Gm SA reports a false recovery, resets the failure timer, and suppresses the alert — strictly worse than not reconnecting. Depends on T017.
- [x] T019 [US1] Rate-limit repair attempts using the loop's existing `backoff` / `next_renewal_attempt` state (`agent.rs:1391`) rather than a second backoff scheme (FR-005). Depends on T017.
- [x] T020 [US1] Verify `hangup_carrier`'s existing reactive reconnect (`agent.rs:2375-2388`) still works unchanged and does not double-reconnect with the new path (FR-008). Depends on T017.

### Escalation (FR-010, FR-010a, FR-010b)

- [x] T021 [US1] Add a `force_renewal` flag set at `MAX_RECONNECT_ATTEMPTS` consecutive failures, bypassing **only** the `renewal_due(...)` early-`continue` at `agent.rs:1714`. Everything downstream — maintenance deferral, modem lock, pre-renewal attach hook, `attempt_renewal`, backoff, status — runs unchanged (research.md R6). Depends on T018, T019.
- [x] T022 [US1] Confirm escalation never returns `Err` from `dispatch_loop` (FR-010a) — the process must not exit and drop other lines' calls. Reset `attempts` and clear `force_renewal` on a successful renewal. Depends on T021.
- [x] T023 [US1] On a failed escalated renewal set `gm_connection = Failed { since }` and keep retrying on backoff — `Failed` is **not** terminal (FR-010b). Depends on T021.

### Tests for US1

- [x] T024 [US1] In `gsm-sip-bridge/tests/test_gm_connection_liveness.rs`: real `TcpListener` as a stand-in P-CSCF answers one `OPTIONS` with `200 OK` (echoing `CSeq`/branch), then closes the accepted stream abruptly; assert the next verdict is `Dead`. Real sockets, no mock. Depends on T012, T013.
- [x] T025 [P] [US1] Test that a 4xx response scores **alive** — the question is whether the connection carries signaling, not whether the carrier likes the request. Depends on T012.
- [x] T026 [P] [US1] Test that a non-matching CSeq is ignored and does not revive a connection already scored dead. Depends on T012.
- [x] T027 [US1] Test that a successful reconnect whose confirming ping goes unanswered does **not** report `Up`, and increments `attempts` (the R7 false-recovery guard). Depends on T018.
- [x] T028 [US1] Test that `MAX_RECONNECT_ATTEMPTS` failures set `force_renewal` and that `dispatch_loop` returns no error (FR-010a regression guard). Depends on T022.
- [x] T029 [US1] Test that replacing the session clears `pending`, so no spurious failure is scored after a successful renewal (T014's regression guard). Depends on T014.

**Checkpoint**: US1 is independently deliverable — dead lines now self-heal, with no status/metrics/alert surface yet. Commit.

---

## Phase 3: User Story 2 — A drop during an active call does not break the call (P1)

**Goal**: Liveness and repair never disturb a call in progress, and deferred work
runs the moment the call ends.

**Independent Test**: Establish a call, trigger the liveness machinery during it,
confirm audio and teardown are unaffected and the held work runs afterwards.

> **Implementation-discovered simplification (2026-08-07).** Once US1 was wired,
> call-safety turned out to hold *by construction*, and the `Maintenance::GmReconnect`
> variant T031–T033 called for would have been dead code. The chain:
> `probe_gm_connection` is gated to run only when `active_call.is_none()`, and
> `PingState::verdict` independently returns `Idle` when `call_in_progress` — so a
> failure is never *detected* during a call, hence never needs *deferring*. A client
> connection that dies mid-call is still covered by the pre-existing reactive path
> (`hangup_carrier`'s BYE-failure reconnect, FR-008, untouched). And the escalation
> (`force_renewal`) still flows through the existing `maintenance.decide(Maintenance::Renewal, active_call.is_some())`
> gate (agent.rs:2014), so it defers during a call and resumes on the next idle poll
> after `maintenance.release()` — FR-006/FR-007 satisfied via the *existing* policy,
> no new variant. Adding `GmReconnect` would violate Principle V (a deferral path
> nothing ever exercises). T031–T033 are therefore intentionally not implemented;
> T034 (subsumption of a variant that doesn't exist) is dropped with them.

- [x] T030 [US2] `PingState::verdict` returns `Idle` while a call is in progress (FR-006/R10), and `probe_gm_connection` is additionally gated on `active_call.is_none()` at the call site. Both layers present.
- [~] T031 [US2] **Not needed** — see the note above. Call-safety is provided by the idle-only gate + the existing `Maintenance::Renewal` deferral, not a new repair-deferral path.
- [~] T032 [US2] **Not needed** — no `Maintenance::GmReconnect` variant; nothing is ever deferred through it, so it would be dead code (Principle V).
- [~] T033 [US2] **Not needed** — there is no held reconnect to release; the escalation resumes via the existing renewal path on the first idle poll after the call ends.

### Tests for US2

- [~] T034 [US2] **Dropped** with T032 (no variant to test).
- [x] T035 [US2] Covered by `ping_verdict_idle_during_a_call` (no repair action is taken while a call is up) plus the existing `Maintenance::Renewal` deferral tests in `lifecycle.rs` (the escalation's deferral path).
- [x] T036 [US2] `ping_verdict_idle_during_a_call` in `agent.rs` asserts no probe is sent while a call is in progress.

**Checkpoint**: US1+US2 — recovery works and is call-safe. Commit.

---

## Phase 4: User Story 3 — Operators can see connection health (P2)

**Goal**: Connection health is visible in `vowifi-status` / `volte-status` and on
the metrics endpoint, not just in logs.

**Independent Test**: Drop a connection; confirm status and the metrics endpoint
both report not-up while reconnecting, and both return to healthy on recovery.

### Health surface (FR-012, FR-018)

- [x] T037 [US3] Add `gm_connection_up: bool` to `ServiceHealth` (`gsm-sip-bridge/src/ims/lifecycle.rs:402`) and fold it into `can_answer()`. Per that function's stated doctrine it "must never be optimistic" — a card on this path has no circuit-switched fallback, so a false yes means silently missed calls. Depends on T007.
- [x] T038 [US3] Add the `blocked_reason()` arm — `"the carrier signaling connection is down"` — ordered **after** `attached` and `registered`, **before** `pbx_registered`. Both layers underneath are reported first; surfacing the symptom over the cause sends an operator to the wrong place. Depends on T037.
- [x] T039 [US3] Add `gm_connection: String` with `#[serde(default)]` to `RegistrationStatusReply` in `gsm-sip-bridge/src/control/protocol.rs` (~line 59). Older peers omitting it must still parse. Depends on T006.
- [x] T040 [US3] Populate it from the status listener in `gsm-sip-bridge/src/ims/agent.rs` (~line 546, beside `blocked_reason`). Depends on T039.
- [x] T041 [P] [US3] Add the `gm_connection:` line to the `vowifi-status` printer in `gsm-sip-bridge/src/vowifi/mod.rs` (~line 1890), after `expires_at`, before `last_failure`. Print `unknown` when the field is empty. Depends on T039.
- [x] T042 [P] [US3] Same line, same position, in the `volte-status` printer at `gsm-sip-bridge/src/volte/bridge.rs` (~line 535). Depends on T039.

### Metrics (FR-013)

- [x] T043 [US3] Add `gm_connection_up: Option<bool>` to `AgentState` in `gsm-sip-bridge/src/control/protocol.rs` (~line 122) with `#[serde(skip_serializing_if = "Option::is_none", default)]`, matching every sibling.
- [x] T044 [US3] Add `set_gm_connection_up(bool)` to `AgentObservability` in `gsm-sip-bridge/src/ims/observability.rs`, mirroring `set_tunnel_up` (~line 81). Depends on T043.
- [x] T045 [US3] Call it from `dispatch_loop` on state changes only — not per poll: `false` on a dead verdict or listener death, `true` on a confirming-ping success, and `true` alongside the existing `set_registered(true)`/`set_tunnel_up(true)` on a successful renewal (`agent.rs:1789-1790`). Depends on T044, T018.
- [x] T046 [P] [US3] Register `VOWIFI_GM_CONNECTION_UP` `GaugeVec` (`module` label) in `gsm-sip-bridge/src/metrics/mod.rs` beside `VOWIFI_TUNNEL_UP` (~line 386). Keep the `vowifi_` prefix for VoLTE too, per the precedent already documented at `ingest.rs:396`.
- [x] T047 [US3] Apply the reported state to the gauge in `gsm-sip-bridge/src/metrics/ingest.rs` beside the `tunnel_up` handling (~line 421). **`None` means "not reported" and must leave the gauge unchanged** — treating absent as `false` would report every line down on any partial report. Depends on T043, T046.

### Tests for US3

- [x] T048 [P] [US3] `lifecycle.rs` unit tests: `can_answer` is false with the connection down and all else healthy; `blocked_reason` reports the **attachment** when both it and the connection are down (T038's ordering guard). Depends on T038.
- [x] T049 [P] [US3] Test in `gsm-sip-bridge/tests/test_volte_bridge.rs` or the liveness test: a reply serialised without `gm_connection` deserialises and renders `unknown`, not `up`. Depends on T039.
- [x] T050 [US3] Extend `gsm-sip-bridge/tests/test_vowifi_health_metrics.rs`: the gauge appears with the right `module` label for a VoWiFi and a VoLTE line, reads `0` while reconnecting, and `gm_connection_up: None` leaves it unchanged. Depends on T047.

**Checkpoint**: US1–US3 — recovery is call-safe and fully observable. Commit.

---

## Phase 5: User Story 4 — Sustained failure raises a proactive alert (P3)

**Goal**: An unrecoverable connection pages once, and pairs with a recovery notice.

**Independent Test**: Make a connection unrecoverable; confirm exactly one alert
past the threshold, then one recovery notice when reachability returns.

### Alert category

- [x] T051 [P] [US4] Add `AlertCategory::GmConnectionLost` to `gsm-sip-bridge/src/alerts/mod.rs` (~line 24), its `as_str` → `"gm_connection_lost"` (~line 57), and its `category_config` arm (~line 150).
- [x] T052 [US4] Add it to the critical-alert allowlist `matches!` at `gsm-sip-bridge/src/alerts/mod.rs:190`. Depends on T051.

### Config plumbing (contracts/config.md)

- [x] T053 [P] [US4] `gsm-sip-bridge/src/config/raw.rs`: `pub gm_connection_lost: Option<RawUnhealthyCategory>` (~line 351) — `RawUnhealthyCategory`, which carries `unhealthy_sec`, **not** `RawAlertCategory` — plus the `("alerts.gm_connection_lost", RawUnhealthyCategory::KEYS)` entry in the known-keys table (~line 596). Missing the second means the section parses but is reported as unknown.
- [x] T054 [US4] `gsm-sip-bridge/src/config/mod.rs`: `pub gm_connection_lost: CategoryAlertConfig` (~line 403), a `GmConnectionLostThresholds` struct with `unhealthy_sec: u64` defaulting to `300` (~line 446), and both default entries (~line 482). Depends on T053.
- [x] T055 [US4] `gsm-sip-bridge/src/config/build.rs`: `category(raw.gm_connection_lost, true)` — **enabled by default**, unlike `line_discovery_failed` (~line 490) — and the thresholds block with the `30..=3600` `in_range_or_warn` check, mirroring `registration_loss_thresholds` (~line 472). Depends on T054.

### Ingest wiring (contracts/metrics.md — two of these fail if missed)

- [x] T056 [US4] Add `gm_connection_unhealthy_since: Option<Instant>` and `gm_connection_alert_phase: AlertPhase` to the per-module liveness record in `gsm-sip-bridge/src/metrics/ingest.rs`, initialised from `existing.map_or(AlertPhase::Idle, ...)` like its siblings (~line 140). Depends on T047.
- [x] T057 [US4] Add the third `decide_transition` arm in the `ALERTS_CONFIG` block (~line 174-208), pushing `AlertCategory::GmConnectionLost` into `pending_transitions`. Depends on T056, T055.
- [x] T058 [US4] ⚠️ Extend the `description` match (~line 297-313) — it ends in `unreachable!("only RegistrationLoss/TunnelFailure transitions are produced here")`, so producing the new transition without this **panics the ingest path**. Failure: `"{module} line's carrier signaling connection down for over {n}s"`; Recovered: `"{module} line's carrier signaling connection re-established"`. Depends on T057.
- [x] T059 [US4] ⚠️ Extend the `record_alert_outcome` match (~line 355-366) — it ends in `_ => return`, so a missing arm fails **silently**: the phase never advances `Pending → Alerted` and the recovery notice never fires. Return `(&mut record.gm_connection_alert_phase, record.gm_connection_unhealthy_since)`. Depends on T057.

### Tests for US4

- [x] T060 [US4] Extend `gsm-sip-bridge/tests/test_ingest_critical_alerts.rs`: one sustained episode → exactly one `Failure`; repeated unhealthy reports in between → `Transition::Suppressed`, not extra sends; recovery → exactly one `Recovered` (FR-015, FR-016, SC-007). Depends on T058.
- [x] T061 [P] [US4] Test that a failure shorter than `unhealthy_sec` produces no transition at all (FR-014, SC-008). Depends on T057.
- [x] T062 [P] [US4] Regression test that `record_alert_outcome` advances the new category's phase — the guard for T059's silent-failure mode. Depends on T059.
- [x] T063 [P] [US4] Extend `gsm-sip-bridge/tests/test_config.rs`: the new keys parse, default to enabled/300, and an out-of-range `unhealthy_sec` warns and falls back. Depends on T055.

**Checkpoint**: All four user stories complete. Commit.

---

## Phase 6: Polish & Verification

- [x] T064 [POLISH] Document `[alerts.gm_connection_lost]` in `docs/configuration.md`. **`tests/test_config_docs.rs` fails until this lands** — the suite enforces that every config key is documented, so this is a gate, not an afterthought. Depends on T055.
- [x] T065 [P] [POLISH] Document the new gauge and the `gm_connection` status field in `docs/observability.md`. Depends on T046, T041.
- [x] T066 [P] [POLISH] Note the default-on alerting behaviour change for upgrades in `RELEASE_NOTES.md` — an existing `config.toml` with no such section gets alerting enabled at 300s.
- [x] T067 [P] [POLISH] Tick the Gm-TCP-reconnect item in `docs/todo.md` and point it at `specs/028-gm-tcp-reconnect/`, matching how the item's plan link is written today.
- [x] T068 [POLISH] Full-suite verification: `make format && make lint && make test`. Confirm `make lint` is clean across all test targets — rustfmt line-length violations in test files have broken commits before (CLAUDE.md).
- [ ] T069 [POLISH] **Hardware verification (SC-010)** — re-run the specs/025 T072 pass-1 scenario on real Airtel/Vodafone VoWiFi: bring line 0 up, leave it idle for the duration that previously killed it, confirm the drop is detected, the reconnect succeeds, and calls remain placeable. This was never reproduced synthetically, so this is the only task that actually closes the original gap. Use the privileged container for anything needing `CAP_NET_ADMIN`.

---

## Dependencies & Execution Order

```
Phase 1 (FOUND) ──▶ Phase 2 (US1) ──▶ Phase 3 (US2)
                          │                  │
                          └──────────────────┴──▶ Phase 4 (US3) ──▶ Phase 5 (US4) ──▶ Phase 6
```

- **Phase 1 blocks everything.** T001→T002 and T004→T005 are the two chains inside it; T003, T006 are independent.
- **US1 is the MVP.** It is shippable alone: lines self-heal, just without a status or alert surface.
- **US2 depends on US1** — it constrains machinery US1 introduces, so it cannot precede it despite sharing P1 priority.
- **US3 depends on US1** for the state it reports (T007's `gm_connection`), not on US2.
- **US4 depends on US3** — the alert is evaluated from the reported metric state (T047), so the metrics path must exist first.
- **T064 must precede T068**, or the full-suite run fails on `test_config_docs.rs`.

## Parallel Opportunities

- Phase 1: T001, T003, T004, T006 in parallel; then T002, T005, T007.
- Phase 2 tests: T025, T026 in parallel once T012 lands.
- Phase 4: T041/T042 (two CLI printers) and T046 in parallel; T048/T049 in parallel.
- Phase 5: T051 and T053 in parallel; T061/T062/T063 in parallel at the end.
- Phase 6: T065, T066, T067 all in parallel.

## Traceability

| Requirement | Tasks |
|---|---|
| FR-001, FR-002, FR-003 | T011, T012, T005 |
| FR-004, FR-005 | T017, T019 |
| FR-006, FR-007 | T030, T031, T032, T033 |
| FR-008 | T020 |
| FR-009 | T018, T027 |
| FR-010, FR-010a, FR-010b | T021, T022, T023, T028 |
| FR-011 | T011, T014, T029 |
| FR-012 | T037–T042 |
| FR-013 | T043–T047, T050 |
| FR-014, FR-015, FR-016 | T051–T059, T060, T061 |
| FR-017 | T014, T022 (state is stack-local by construction) |
| FR-018 | T030, T045, T068 |
| FR-019 | T004 (interval), T030 (idle-only) |
| FR-020 | Satisfied structurally — VoLTE runs the same `dispatch_loop` (research.md R5); verified by T050's VoLTE label assertion |
| FR-021 | T001, T002, T015, T016, T010 |
| FR-022 | T013, T024 |
| SC-010 | T069 (hardware only) |

## Known Gaps Carried Forward

- **Listener reachability**: T015 detects "the accept loop died," not "the listener is alive but unreachable from the network." No cheap signal exists for the latter (research.md R4). Documented, not fixed.
- **SC-010 cannot be closed by T001–T068.** Every synthetic test bounds the logic; only T069 confirms the fix against the failure that actually occurred.

## Implementation notes (where the tests actually live)

- **Test placement.** The planned external file `tests/test_gm_connection_liveness.rs` (T010/T024) was **not** created: `ims::sip_client` is `pub(crate)`, so `spawn_gm_server`, `PingState`, and `send_gm_ping` are unreachable from an external `tests/` crate. The tests instead live in-crate, where they can reach those items:
  - `PingVerdict`/`PingState` state machine, full-cycle alive→dropped→dead, CSeq matching, reset, 4xx-is-alive, and `gm_episode_since` → `#[cfg(test)] mod tests` in `src/ims/agent.rs` (covers T005/T008/T024/T025/T026/T027-logic/T029).
  - `build_options` framing, `is_transient_accept_error` classification, and a real-socket `spawn_gm_server` liveness+delivery+clean-drop test → `src/ims/sip_client.rs` tests (covers T003/T009/T010).
  - `ServiceHealth` can_answer/blocked_reason (incl. down-gm and attachment-outranks-gm ordering) → `tests/test_volte_bridge.rs` (T037/T038/T048).
  - Older-peer `gm_connection` omission parses → `src/vowifi/control.rs` tests (T049).
  - Gauge for VoWiFi and VoLTE labels → `tests/test_vowifi_health_metrics.rs` (T050).
  - Alert failure/suppression/recovery pairing → `tests/test_ingest_critical_alerts.rs` (T060/T061); `record_alert_outcome` phase advance is exercised by the same recovery assertion (T062).
  - Config parse/default/range → `tests/test_config.rs` and `tests/test_config_docs.rs` (T063/T064).
- **`probe_gm_connection` / `dispatch_loop` orchestration is not unit-tested in isolation** — it needs a live `RegisteredSession`, which has no test constructor without a real IMS registration (hardware). Its logic is glue over primitives that *are* individually tested; end-to-end confirmation is T069.
- **Verification run (T068):** `make format` + `make lint` (exit 0, clippy `-D warnings` clean across all targets) + `make test` (exit 0, 59/59 test binaries pass) all green as of 2026-08-07.
