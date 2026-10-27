---

description: "Task list for IMS registration refresh without call interruption"
---

# Tasks: IMS Registration Refresh Without Call Interruption

**Input**: Design documents from `/specs/123-ims-registration-refresh/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/status-output.md, quickstart.md

**Tests**: Included. The constitution makes integration-first TDD the default, and the spec's acceptance scenarios are written as tests. Write each test first and see it fail.

**Organization**: grouped by user story. Delivery is in three slices (see plan.md): **Slice A** = US1, **Slice B** = US2 + US3 + US4 + status, **Slice C** = US5 (gated on Slice B live evidence). Every commit must pass `make format`, `make lint`, `make test` (CLAUDE.md pre-commit checklist).

## Format: `[ID] [P?] [Story] Description`

- **[P]**: can run in parallel (different files, no dependency on an unfinished task)
- **[Story]**: US1..US6 from spec.md
- Paths are relative to the repo root; `ims/` means `gsm-sip-bridge/src/ims/`
- Fixtures use synthetic identifiers only (`+919000000000`, placeholder IMSI); never real subscriber data

---

## Phase 1: Setup

- [X] T001 Confirm a clean baseline on branch `123-ims-registration-refresh`: run `make format && make lint && make test` and note the pass count in `specs/123-ims-registration-refresh/live-verification.md` (create it with a header and an empty run table)
- [X] T002 [P] Read PR #168 commit `3e914c3` (`gh api` / `git fetch origin 3e914c3`) and list in `specs/123-ims-registration-refresh/research.md` §R8 the exact hunks to reuse per file, so credit (`Co-authored-by: Kirill Mironov`) is applied commit by commit

---

## Phase 2: User Story 1 - Standard refresh schedule (Priority: P1) — Slice A

**Goal**: refresh becomes due at 600 s before expiry for grants over 1200 s, half the grant otherwise, never closer than 60 s apart.
**Independent Test**: unit tests on the scheduler; no network.

### Tests (write first)

- [X] T003 [P] [US1] Table-driven tests in `ims/mod.rs` tests: 3600 s → due at 3000 s; 600 s → 300 s; 1200 s → 600 s; 1201 s → 601 s; a later different grant uses its own value (acceptance 1-4)
- [X] T004 [P] [US1] Test for the 60 s floor: grants of 10 s, 100 s and 119 s refresh at 60 s and report a warning flag; 120 s refreshes at 60 s with no warning (FR-003, edge case)

### Implementation

- [X] T005 [US1] Change `renewal_headroom_for` in `ims/mod.rs` to the TS 24.229 §5.1.1.4.1 rule (implemented as the new `registration_refresh_offset`; `renewal_headroom_for` stays, because the reg-event subscription still uses it); add the 60 s minimum spacing and a warn-once log for grants under 120 s
- [X] T006 [US1] Remove `RENEWAL_HEADROOM` (`ims/agent/mod.rs:158`) and update its three uses (initial headroom around lines 994 and 3817, and the `granted`-derived value) to call the new helper with the granted lifetime
- [X] T007 [P] [US1] Update `docs/vowifi-bridge.md` (Agent A bullet) and add a `RELEASE_NOTES.md` Unreleased entry for the timing change (bold only the feature name, plain description after, per project convention)
- [X] T008 [US1] Run `make format && make lint && make test`; commit as its own atomic commit (credit the reporter if the helper is taken from `3e914c3`)

**Checkpoint (Slice A)**: a one-hour grant renews at 50 minutes. Shippable alone.

---

## Phase 3: Foundational for Slice B (blocks US2, US3, US4)

**Purpose**: state and test infrastructure the in-place refresh needs. No behaviour change yet.

- [X] T009 Add `registration_cseq` (u32, starts after the initial registration's last CSeq) and the cached `registration_auth` (no `Debug`, never logged) to `RegisteredSession` in `ims/mod.rs`; populate both where the initial AKA response is built (around `build_authorization`, ~line 1284) and in the test constructor (~line 1562)
- [X] T010 [P] Split `Maintenance` in `ims/lifecycle.rs`: add `RegistrationRecovery`, make `MaintenancePolicy::decide` return `Proceed` for `Renewal` regardless of calls, keep `RegistrationRecovery` and `Reattachment` deferred, `Reattachment` subsuming recovery; port/replace the existing lifecycle tests (reuse the PR's tests with credit)
- [X] T011 [P] Build the loopback P-CSCF simulator in `gsm-sip-bridge/tests/common/` (UDP and TCP): scriptable answers (200 with Contact expires, 423 + Min-Expires, 503 + Retry-After, 403/408/500/504, 401 challenge), delayed/dropped answers, retransmission capture, and an OPTIONS answer that reuses a pending REGISTER's CSeq number
- [X] T012 Add the new status fields to `RegistrationStatus` in `ims/mod.rs` (`refresh_mode`, `consecutive_rejections`, `last_refresh`, `next_refresh_at`) with defaults per data-model.md; no rendering yet

**Checkpoint**: builds and passes; behaviour unchanged.

---

## Phase 4: User Story 2 - Refresh during a call without disturbing it (Priority: P1) — Slice B

**Goal**: scheduled refresh goes out over the live Gm connection as an asynchronous transaction, during calls and call set-up, without touching dialogs, media or the subscription.
**Independent Test**: loopback simulator + agent loop with an active call (acceptance 1-7).

### Tests (write first)

- [X] T013 [P] [US2] `gsm-sip-bridge/tests/test_ims_registration_refresh.rs`: idle refresh keeps Call-ID, increments CSeq by one, replays the cached Authorization verbatim, extends expiry, keeps the same reg-event subscription (acceptance 4, 6)
- [X] T014 [P] [US2] Same file: refresh with an active call and with a pending origination proceeds within 2 s of due; call dialog identity, route, CSeq and media are unchanged afterwards (acceptance 1, 2; SC-008)
- [X] T015 [P] [US2] Same file: while the refresh answer is delayed, inbound call signalling and reg-event NOTIFYs are still handled (acceptance 3)
- [X] T016 [P] [US2] `ims/agent/registration_tests.rs`: matching requires Call-ID, CSeq number, method and exactly one top-Via branch; a REGISTER answer never confirms an OPTIONS probe with the same number and the reverse; stale answers are ignored (acceptance 5)
- [X] T017 [P] [US2] Same file: UDP retransmits the identical request on 2 s → 16 s timers, a provisional answer does not extend the 128 s deadline, TCP does not retransmit (acceptance 7)
- [X] T018 [P] [US2] Same file: Contact lifetime parsing (own contact only, zero = lost binding, missing or malformed = failure, response `Expires` fallback only when our contact omits it) and the single `423` retry with `Min-Expires` (edge cases; FR-011, FR-012)

### Implementation

- [X] T019 [US2] Create `ims/agent/registration_refresh.rs` with `PendingRegister` (start, send, matches, poll for retransmit/Timer F, retry_min_expires) and the lifetime/Contact parsing; reuse the sound parts of `3e914c3` with credit. Protected-port and SA-negotiation headers are **not** part of Slice B (Slice B sends no `Security-Client`; it uses the existing SA)
- [X] T020 [US2] Wire into `ims/agent/mod.rs`: hold `registration_refresh: Option<PendingRegister>`; in `on_idle_tick`, start a refresh when due and not already pending, honouring `next_renewal_attempt`; route REGISTER answers from the dispatch/response handler to `PendingRegister::matches` before the keepalive/report matchers; make the OPTIONS probe matcher reject REGISTER answers
- [X] T021 [US2] On a matching 200: validate the grant (T018), update `granted`/`registered_at`/`expires_at`/P-Associated-URI-derived routing, call `on_renewal_success`, report metrics, keep `session`, `inbound`, `active_call` and the subscription untouched; refresh the reg-subscription timer only if the registrar changed the lifetime
- [X] T022 [US2] In `on_idle_tick`, stop calling `maintenance.decide(Maintenance::Renewal, busy)` for the refresh path; keep `decide(RegistrationRecovery, busy)` in front of the old full re-registration code, and route `force_renewal`/Gm-liveness escalation to it
- [X] T023 [US2] Make the 401 answer to a refresh a full re-registration (deferred while busy) for now, with a log line naming Slice C as the follow-up
- [X] T024 [US2] Run `make format && make lint && make test`; commit in logical pieces (T009/T010 earlier; T019-T023 together or split by file), each green

**Checkpoint**: refresh works in tests during idle, call and set-up. Not yet failure-complete.

---

## Phase 5: User Story 4 - Refresh keeps working while a call rings (Priority: P1) — Slice B

**Goal**: a timely refresh answer is honoured while a PBX reply is held.
**Independent Test**: incoming call rings 40 s with 100 s of the 128 s transaction elapsed; the answer at 110 s confirms the refresh.

- [X] T025 [US4] Audit `ims/agent/inbound.rs` (and `call.rs`, `origination.rs`) on current `main` to find where non-call traffic is held during the PBX wait and replayed; write the finding as a comment block in `specs/123-ims-registration-refresh/research.md` §R6 (file, function, hold and replay points)
- [X] T026 [P] [US4] Test in `tests/test_ims_registration_refresh.rs`: the 100 s / 40 s ring scenario above, and a variant where the answer arrives before the hold begins (acceptance 1; SC-007)
- [X] T027 [P] [US4] Test: registration timers (retransmit and Timer F) fire on time during a held PBX reply (acceptance 2)
- [X] T028 [US4] Fix the ordering found in T025: deliver registration answers and evaluate refresh timers before/independently of the held replay, or timestamp held answers and treat an answer received before the deadline as timely; do not add a second reader thread
- [X] T029 [US4] `make format && make lint && make test`; commit

---

## Phase 6: User Story 3 - A failed refresh fails over and recovers (Priority: P1) — Slice B

**Goal**: failures use the existing pool, backoff and budget; calls are never cut; fallback after 2 rejections.
**Independent Test**: simulator scripted failures against a two-P-CSCF pool.

### Tests (write first)

- [X] T030 [P] [US3] Table test for R3 mapping in `ims/agent/registration_tests.rs`: 408/500/504/403 → force `RegistrationRecovery`; 503 with Retry-After > 128 s → mark P-CSCF + fail over; 503 ≤ 128 s → retry same P-CSCF after Retry-After; other final error → rejection count + 1; Timer F → no rejection count (acceptance 1, 4)
- [X] T031 [P] [US3] Agent test: failure while a call is active keeps the call, keeps reporting registered until actual expiry, defers recovery (acceptance 2)
- [X] T032 [P] [US3] Agent test: binding expires mid-call → health `Failed`, call untouched, refresh retries continue on backoff, recovery starts when the call ends (acceptance 3; FR-015)
- [X] T033 [P] [US3] Agent test: 2 consecutive rejections → next cycle uses full re-registration (deferred while busy); the cycle after a successful fallback tries in-place again; counter resets on an accepted refresh (acceptance 6, 7; FR-018a; SC-011)
- [X] T034 [P] [US3] Agent test: the existing failure budget still ends in exit-for-restart, deferred during a call (acceptance 5; FR-018)

### Implementation

- [X] T035 [US3] Replace the PR-style stand-alone failure handler with one function in `ims/agent/mod.rs` that maps a refresh outcome (`RegisterFailure` plus status and Retry-After) onto `on_renewal_failure` / `schedule_renewal_retry` / `force_renewal` per R3; keep `count_loss` and the budget semantics
- [X] T036 [US3] Implement the rejection counter and `refresh_mode` switch (FR-018a) in `LoopState`; log the rejecting status when the fallback engages; reset on success
- [X] T037 [US3] Implement expiry-mid-call behaviour: status `Failed` on actual expiry, no call teardown, `RegistrationRecovery` released by `maintenance.release()` when the call ends
- [X] T038 [US3] `make format && make lint && make test`; commit

---

## Phase 7: Status and observability — Slice B

**Goal**: operators can see the refresh mode and last result (FR-027).

- [X] T039 [P] Test: `vowifi-status` and `volte-status` render the contract in `contracts/status-output.md`, including the "older agent" defaults (`gsm-sip-bridge/src/vowifi/control.rs` tests and `tests/test_vowifi_*.rs`)
- [X] T040 Publish the new fields from the agent into the status reply (`ims/agent/mod.rs`, `vowifi/control.rs`) and render them in `vowifi/mod.rs` and `volte/bridge.rs`; add `registration-recovery` to `Maintenance::as_str` rendering
- [X] T041 [P] Update `docs/vowifi-bridge.md` (refresh, fallback, deferral rules), `docs/operations.md` (how to read the new status lines), and add the Slice B `RELEASE_NOTES.md` entry
- [X] T042 `make format && make lint && make test`; commit

---

## Phase 8: Slice B live verification (gate for Slice C)

- [ ] T043 Build the arm64 image (arm-build-200 pipeline) and deploy to the Jio Pi; run the quickstart steps 1-3, recording each run in `live-verification.md` (SC-001, SC-002, SC-003)
- [ ] T044 Run the quickstart steps 4-5 on the local Vodafone VoWiFi rig (SC-005a) including a forced rejection to see `refresh_mode: fallback`; run the P-CSCF failover check (SC-006)
- [X] T045 Record the outcome and any carrier-specific findings in `live-verification.md`; if in-place refresh is rejected on a carrier, open an issue before starting Slice C

**Checkpoint (Slice B)**: stories US2, US3, US4 are done, documented and live-verified. Open the PR; credit the reporter in the PR body and commits. Respond to the reporter on #163/#168 (human reply, not to the Greptile bot).

---

## Phase 9: User Story 5 - Network-requested re-authentication (Priority: P2) — Slice C

**Gate**: do not start until T045 is recorded. T045 is recorded in `live-verification.md`: in-place refresh was accepted on both carriers and no carrier has been seen to challenge a refresh. Slice C was started on 2026-10-27 at the maintainer's request (#163 reopened) rather than on a seen challenge. Vodafone was found not to challenge a protected refresh at all, so the live path cannot be forced there (see `live-verification.md`).
**Goal**: answer a fresh AKA challenge without blocking signalling, with staged SAs, stable server port, drain and rollback.

### Tests (write first)

- [X] T046 [P] [US5] `tests/test_ims_registration_refresh.rs` (TCP and UDP): a 401 challenge during an active call → background worker answers, call signalling and NOTIFYs keep being handled, registration extended, server port unchanged, client port changed (acceptance 1, 2)
- [X] T047 [P] [US5] Failure and cancel: staged SAs removed, working SAs untouched, a late answer from the abandoned attempt changes nothing (acceptance 3, 6; FR-026)
- [X] T048 [P] [US5] Drain: a transaction started on the old connection (including a pending origination) still completes after the switch; old connection retired after ≥ 128 s, extended while an origination is pending (acceptance 4; FR-023)
- [X] T049 [P] [US5] UDP: authentication reply addressed to the advertised server port instead of the new client port is still received (acceptance 5; FR-025)
- [X] T050 [P] [US5] AUTS resynchronisation still works through the background path (acceptance 7)
- [X] T051 [P] [US5] XFRM stage/activate/rollback in the privileged container (`gm_ipsec.rs` backend trait): activation only after success, rollback on drop, SA lifetime no shorter than registration + 30 s (R4)

### Implementation

- [X] T052 [US5] Add `ims/registration_auth.rs`: `ReauthRequest`/`Reauthenticated`/staged association with rollback-on-drop, SIM/AKA work off the dispatch thread, result channel, cancel flag, 600 s work timeout; reuse sound parts of `3e914c3` with credit
- [X] T053 [US5] Extend `ims/gm_ipsec.rs` with stage / activate / rollback of replacement SAs keeping the protected server port; keep the SA backend trait mockable only where hardware is unavailable, with the justification comment required by the constitution
- [X] T054 [US5] Extend `ims/session.rs` `Inbound` with adoption of a new client reader (single live reader), and `ims/agent/mod.rs` to start the worker on a 401 to a refresh, poll it, commit or roll back, move the old transport to a retired list, update active call and origination signalling to the new transport
- [X] T055 [US5] Implement UDP reply reading from both the new client socket and the dispatcher channel (do not repeat PR #168's finding 1)
- [X] T056 [US5] Replace T023's "401 = full re-registration" with the background re-authentication; keep full re-registration as the failure fallback
- [X] T057 [US5] `make format && make lint && make test`; commit in logical pieces

### Live verification (Slice C)

- [ ] T058 (blocked: no carrier challenges a refresh; see `live-verification.md`) Quickstart steps 6-7 on Jio VoWiFi (mid-call AKA; UDP) and record in `live-verification.md` (SC-004, SC-005); repeat on Vodafone VoWiFi if the carrier issues a challenge

---

## Phase 10: Polish and cross-cutting

- [X] T059 [P] Update `docs/vowifi-bridge.md` for the re-authentication and drain behaviour; add the Slice C `RELEASE_NOTES.md` entry
- [X] T060 [P] Add anything learned to `docs/greptile-review-learnings.md` (ring-wait ordering, shared Call-ID/CSeq collision) so the next review does not rediscover it
- [X] T061 [P] Final scan for real identifiers: `grep` the diff and new fixtures for MSISDN/IMSI/ICCID patterns (CLAUDE.md rule)
- [ ] T062 Fix valid Greptile findings in code and reply "Addressed in <sha>" on each thread. Local reviewers (`/greploop`, `/code-review`) run only when the maintainer asks.
- [ ] T063 Confirm SC-009: `make format && make lint && make test` green on the final tip; comment on issue #163 with the slice PRs and evidence links

---

## Dependencies and Execution Order

- **Slice A** (Phase 2) depends only on Phase 1; ship first.
- **Phase 3** blocks Phases 4-7. T009, T010, T011, T012 touch different files and run in parallel except T009 → T012 (both `ims/mod.rs`).
- **Phase 4 (US2)** before Phases 5 and 6 (they build on the transaction). Phases 5 (US4) and 6 (US3) can proceed in parallel once T021 is done; they touch `inbound.rs` vs the failure handler in `agent/mod.rs`, so coordinate merges.
- **Phase 7** after Phase 6 (needs the rejection counter). **Phase 8** after Phase 7.
- **Phase 9 (Slice C)** only after T045.
- Within a story: tests first and failing, then implementation, then `make format lint test`, then commit.

### Parallel opportunities

```text
Phase 2 tests:     T003 + T004
Phase 3:           T010 + T011 (T009 → T012 serial)
Phase 4 tests:     T013 T014 T015 T016 T017 T018
Phase 6 tests:     T030 T031 T032 T033 T034
Phase 9 tests:     T046 .. T051
```

## Implementation Strategy

1. **MVP = Slice A.** It is two small code changes with tests and removes part of the lapse window on its own.
2. **Slice B** delivers the issue's actual outcome (a call neither cut by nor outliving its registration). It is the largest piece of risk-bearing work and is gated on live evidence.
3. **Slice C** is optional until a carrier is seen challenging during a call: until it ships, a 401 to a refresh falls back to the deferred full re-registration, which is today's behaviour limited to that case.
4. Commit after every task or tight group; never leave `make test` red.
