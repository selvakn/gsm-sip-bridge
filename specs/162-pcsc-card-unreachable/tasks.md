# Tasks: A Card-Reader Line Rides Out an Unreachable SIM

**Input**: Design documents from `specs/162-pcsc-card-unreachable/` (spec.md, plan.md, research.md, data-model.md, contracts/status-and-logs.md, quickstart.md)
**Prerequisites**: plan.md (Blast Radius filled with grep output), research.md decisions D1–D9

**Tests**: requested. SC-007 asks for a deterministic test for every US1–US3 scenario, and the constitution makes TDD the default. Write each story's tests first, watch them fail, then implement.

**Paths**: the crate root is `gsm-sip-bridge/`, so source is `gsm-sip-bridge/src/...`; docs are at the repo root. Use `CARGO_TARGET_DIR=$PWD/target-162` for every cargo/make command, so that another worktree's build cannot clobber this one.

**Never** put a real IMSI or MSISDN in code, tests or docs. Tests use the synthetic `001010000000001`.

**Rig conventions** (`gsm-sip-bridge/src/ims/agent/registration_tests.rs`):
- `Rig::with_sa(replies, delay)` sets up a registration with a Gm association and a scripted SIM.
- `rig.drive()` is one dispatch pass. `rig.received()` reads what the fake P-CSCF got. `rig.challenge(&request)` builds the 401 for a refresh, and `rig.answer(&resp)` feeds it.
- A card-reader line is `rig.cfg.pcsc_reader = true` plus `rig.st.reauth_env.timing = CardTiming::fast()`.
- Loop on `rig.drive()` with a 5 s safety deadline and `sleep(5 ms)` between passes, as `a_sim_that_cannot_answer_falls_back_to_a_full_registration_and_installs_nothing` does.

## Format: `[ID] [P?] [Story] Description`

## Phase 1: Setup

- [X] T001 Confirm the branch is `162-pcsc-card-unreachable` with no upstream (`git rev-parse --abbrev-ref @{u}` must fail). Then run `CARGO_TARGET_DIR=$PWD/target-162 make test` from the repo root to record a green baseline, and note the test count for the PR description.

---

## Phase 2: Foundational (blocks US1–US3)

**Purpose**: the typed failure (research D1), then the generalised worker and timing seam (data-model `SimWorker`, `CardTiming`). Two commits.

### Typed "card unreachable" failure (commit 1)

- [X] T002 [P] In `gsm-sip-bridge/src/modules/pcsc_card.rs` `mod tests`, write failing tests for a `pcsc_failure(context: &str, e: pcsc::Error) -> BridgeError` helper:
  - `unreachable_pcsc_errors_are_typed`: for each of `ResetCard, RemovedCard, NoSmartcard, UnpoweredCard, UnresponsiveCard, ReaderUnavailable, UnknownReader, CommError, CommDataLost, NoReadersAvailable, NoService, ServiceStopped, Shutdown, InvalidHandle, SharingViolation`, `pcsc_failure("PC/SC transmit failed", e).is_card_unreachable()` is true, and its `to_string()` ends with `PC/SC transmit failed: {e}`.
  - `other_pcsc_errors_stay_untyped`: `Timeout, InvalidParameter, ProtoMismatch, NotTransacted, InternalError` give `BridgeError::Ims`, with text identical to today's `format!("{context}: {e}")`.
  - `typed_text_keeps_every_classifier_verdict`: for every mapped variant, `crate::ims::session::map_registration_error` and `crate::ims::classify_register_failure` return the same value for the typed error as for `BridgeError::Ims(format!("PC/SC transmit failed: {e}"))`. Do the same for the no-matching-IMSI text. Make those functions `pub(crate)`-reachable from the test if needed, without changing their behaviour.
- [X] T003 In `gsm-sip-bridge/src/error.rs` add `#[error("IMS error: SIM card unreachable: {0}")] CardUnreachable(String)` with a doc comment: built only by the PC/SC transport, recognised by `is_card_unreachable()`, and never matched by text (#233). Add `impl BridgeError { pub fn is_card_unreachable(&self) -> bool { matches!(self, BridgeError::CardUnreachable(_)) } }`. Add a test in `error.rs` that `CardUnreachable("x").to_string() == "IMS error: SIM card unreachable: x"`.
- [X] T004 Implement `pub(crate) fn pcsc_failure` and a private `fn means_unreachable(e: pcsc::Error) -> bool` (the exact list in T002) in `gsm-sip-bridge/src/modules/pcsc_card.rs`, with a doc comment naming the spec conditions (research D1 table). T002 now passes.
- [X] T005 In `gsm-sip-bridge/src/modules/pcsc_card.rs` route every PC/SC failure through `pcsc_failure`, keeping each context string:
  - `Context::establish` (`PC/SC context establish failed`, `:112`);
  - `list_readers` (`PC/SC list_readers failed`, `:117`);
  - `with_transaction`'s begin (`PC/SC transaction begin failed`, `:220`);
  - `PcscTransport::transmit_apdu` (`PC/SC transmit failed`, `:234`);
  - `Transaction::transmit_apdu` (`PC/SC transmit failed`, `:263`).
  - Also change the two final `connect` errors (`no real PC/SC reader found…` `:123`, and `no PC/SC reader has a card matching IMSI…` `:186`) to `BridgeError::CardUnreachable(<same text>)`.
  - Leave the `reader name … embedded NUL` error, and the probe-loop `PC/SC transaction begin failed on reader …` (logged and skipped, never returned), as `Ims`.
  - Leave `gsm-sip-bridge/src/modules/pcsc_list.rs` untouched (plan Blast Radius).
- [X] T006 [P] Add two hardware-gated tests to the existing `#[cfg(test)] mod hardware` in `gsm-sip-bridge/src/modules/pcsc_card.rs`, with the same `#[ignore = …]` text as #234's tests:
  - `a_reset_by_another_program_is_card_unreachable`: connect a `PcscTransport` to the reader's card, using the card's own IMSI via the existing `card_imsi` helper. On a second `Context`, connect a bare `Card` and drop it, which resets the card. Assert our next `transmit_apdu("00A4000C023F00")` is `Err` with `is_card_unreachable()`.
  - `no_reader_with_this_imsi_is_card_unreachable`: `PcscTransport::connect("001010000000001")` is `Err` with `is_card_unreachable()`.
- [X] T007 Grep the whole tree for code that relied on the old text of these errors: `grep -rn 'IMS error: PC/SC\|IMS error: no PC/SC\|PC/SC transmit failed\|no PC/SC reader' gsm-sip-bridge/src gsm-sip-bridge/tests docs`. Update any test that pins the full `to_string()`. Record any production reader found in the PR description. Then run `make format && make lint && make test` and commit: `feat(pcsc): report an unreachable card as a typed error` (commit 1).

### Generalised SIM worker and timing seam (commit 2, no behaviour change)

- [X] T008 In `gsm-sip-bridge/src/ims/registration_auth.rs` add `pub(crate) struct CardTiming { pub(crate) retry_window, retry_interval, attempt_bound, check_initial, check_max: Duration }`:
  - Add the constants `CARD_RETRY_WINDOW = 30 s`, `CARD_RETRY_INTERVAL = 2 s`, `CARD_ATTEMPT_BOUND = 15 s` and `CARD_CHECK_MAX_BACKOFF = 60 s`, each with a doc comment citing research D2/D4.
  - `impl Default` uses those, with `check_initial` = the agent's existing `RETRY_INITIAL_BACKOFF` (5 s). Pass it in, or move the value to a shared `pub(crate)` constant without changing it.
  - `#[cfg(test)] pub(crate) fn fast() -> Self`: window 300 ms, interval 20 ms, attempt bound 200 ms, check initial 20 ms, check max 80 ms.
- [X] T009 In `gsm-sip-bridge/src/ims/registration_auth.rs` generalise `Reauthentication` into `pub(crate) struct SimWorker<T>`:
  - Fields: `result: Receiver<BridgeResult<T>>`, `cancelled: Arc<AtomicBool>`, `done: Arc<AtomicBool>`, `deadline: Instant`.
  - Methods: `poll()`, `overdue(now) -> bool` (`now >= deadline`), `cancel()`, `finished_flag() -> Arc<AtomicBool>`. `Drop` cancels.
  - The thread sets `done` through a small drop guard, so a panic sets it too.
  - `pub(crate) type Reauthentication = SimWorker<Answer>`. `Reauthentication::spawn(job, open)` keeps its signature and sets `deadline = now + WORK_TIMEOUT`.
  - `open`'s bound changes from `FnOnce` to `Fn + Send + 'static`. Callers already pass an `Arc<dyn Fn>` closure.
  - Update the doc comments at `:22-25` and `:100-101` (plan Blast Radius).
  - All existing `registration_auth` and `registration_tests` tests pass unchanged.
- [X] T010 In `gsm-sip-bridge/src/ims/registration_auth.rs` add `pub(crate) type CardCheck = SimWorker<()>` with `CardCheck::spawn(cfg: ImsRegisterConfig, open, bound: Duration)`. The thread runs `open(&cfg).map(|_| ())` (dropping the card at once releases it leaving the card, #234), and `deadline = now + bound`. Add a unit test that a check whose `open` blocks past `bound` is `overdue`, and that its `finished_flag()` turns true once `open` returns.
- [X] T011 In `gsm-sip-bridge/src/ims/registration_auth.rs` `mod scripted` add `pub(crate) fn card_after(failures: usize, replies: Vec<String>, delay: Duration) -> (impl Fn(&ImsRegisterConfig) -> BridgeResult<OpenedCard> + Send + Sync + 'static, Arc<AtomicUsize>)`. The first `failures` opens return `Err(BridgeError::CardUnreachable("PC/SC transmit failed: The smart card has been reset, so any shared state information is invalid".into()))`; later ones return the scripted `Sim`, which shares one script across opens like `card`. The returned counter is the total number of opens. Add a `Sim` reply sentinel `"!unreachable"` that makes `transmit_apdu` return the same `CardUnreachable`, for a reset in the middle of an AKA.
  - Mock justification at the site: CI has no reader. The real mapping is covered by T006.
- [X] T012 Add `pub(super) timing: CardTiming` to `ReauthEnv` in `gsm-sip-bridge/src/ims/agent/registration_refresh.rs` (`Default` = `CardTiming::default()`), and extend the doc comment at `:1077-1081`. Update the `ReauthEnv { … }` literal in `registration_tests.rs:1196` with `timing: CardTiming::default()`. Run `make format && make lint && make test` and commit: `refactor(ims): generalise the SIM worker and give it a per-worker deadline` (commit 2).

**Checkpoint**: no behaviour change. Typed errors exist, and the worker has a deadline and a done flag.

---

## Phase 3: User Story 1 - A brief card outage during a challenged refresh goes unnoticed (P1) 🎯 MVP

**Goal**: on a card-reader line the same challenge is put to the card again for the retry window (FR-005–FR-008). Until US2 lands, a window that runs out still falls back to full, as today.

**Independent Test**: with `card_after(2, [success_reply()], 0)`, a challenged refresh sends the Answer REGISTER, with no `force_renewal`.

### Tests for User Story 1 (write first, watch them fail)

- [X] T013 [P] [US1] `card_reader_challenge_survives_two_resets` in `gsm-sip-bridge/src/ims/agent/registration_tests.rs`:
  - Setup: `Rig::with_sa`, `pcsc_reader = true`, `fast()` timing, `open_card = card_after(2, vec![success_reply()], ZERO)`, and an active call (`test_active_call`).
  - Drive until the second REGISTER arrives (`next_register`). It carries an `Authorization` with `response=`.
  - Assert `!force_renewal`, `challenged.is_none()`, `opens == 3`, and that the call is still there.
- [X] T014 [P] [US1] `card_reader_reset_mid_aka_is_retried` in `gsm-sip-bridge/src/ims/agent/registration_tests.rs`: replies `vec!["!unreachable", success_reply()]`, `card_after(0, …)`. The Answer REGISTER goes out, and `opens == 2`.
- [X] T015 [P] [US1] `card_reader_retry_that_finds_the_sqn_moved_resynchronises` in `gsm-sip-bridge/src/ims/agent/registration_tests.rs`: `card_after(1, vec![sync_failure_reply(), success_reply()], ZERO)`. The next REGISTER carries `auts=`, as in `a_sequence_mismatch_is_resynchronised_and_the_fresh_challenge_answered`.
- [X] T016 [P] [US1] `card_reader_card_error_is_not_retried` in `gsm-sip-bridge/src/ims/agent/registration_tests.rs`: a SIM reply of `"6982"` (an error status word from the card itself). Drive until `force_renewal`. Assert `opens == 1` and no REGISTER sent (FR-002).
- [X] T017 [P] [US1] `modem_line_unreachable_error_still_falls_back_at_once` in `gsm-sip-bridge/src/ims/agent/registration_tests.rs`: `pcsc_reader = false`, `card_after(5, …)`. Drive until `force_renewal`. Assert `opens == 1` (FR-019).
- [X] T018 [P] [US1] `card_reader_window_is_the_challenge_deadline` in `gsm-sip-bridge/src/ims/agent/registration_tests.rs`: SIM delay 1 s, longer than `fast()`'s window plus bound (500 ms). Assert the challenge is given up within 900 ms, not after `WORK_TIMEOUT`. For now it ends in `force_renewal`; T027 changes the expectation to waiting. Assert no second open happened while the first was still inside the SIM (FR-015, D3).

### Implementation for User Story 1

- [X] T019 [US1] In `gsm-sip-bridge/src/ims/registration_auth.rs` add `Reauthentication::spawn_with(job, open, timing: Option<CardTiming>)`, and make `spawn` call it with `None`.
  - With `Some(t)`: `deadline = now + t.retry_window + t.attempt_bound`.
  - `answer()` parses the challenge once, then loops over `open` + `answer_challenge`. On `Err(e) if e.is_card_unreachable()`, while not cancelled and `Instant::now() + t.retry_interval < window_end`, it logs at INFO `the SIM is out of reach; asking it the same challenge again` (`attempt`, `error`), sleeps `retry_interval`, and retries.
  - With `None`, behaviour is exactly today's single attempt.
  - Each attempt's `OpenedCard` drops before the sleep.
- [X] T020 [US1] In `gsm-sip-bridge/src/ims/agent/registration_refresh.rs::begin_challenge`, spawn with `timing = p.reg_cfg.pcsc_reader.then(|| self.reauth_env.timing.clone())`. Update the `drive_challenge` doc comment (`:1201-1202`). T013–T018 pass with today's `fall_back_to_full` at the end of the window.
- [X] T021 [US1] Run `make format && make lint && make test`. US1 is not committed alone: it ships with US2 in commit 3, because without US2 a long outage still goes to full. Make sure the tree is green anyway.

**Checkpoint**: brief outages are ridden out (SC-001).

---

## Phase 4: User Story 2 - A longer card outage keeps the registration (P1)

**Goal**: the waiting-for-card state (FR-009–FR-015, research D4/D5, data-model `AwaitingCard`).

**Independent Test**: the card stays out past the window and then comes back. No full registration, no REGISTER while waiting, and exactly one in-place REGISTER after the card returns.

### Tests for User Story 2 (write first, watch them fail)

Shared helper (write with T022): `fn reader_rig_waiting(opens_fail: usize) -> (Rig, Kernel, Arc<AtomicUsize>)`. It builds a `Rig::with_sa` card-reader rig with `fast()` timing and `card_after(opens_fail, vec![success_reply()], ZERO)`, drives the refresh, answers it with `rig.challenge`, then drives until `rig.st.awaiting_card.is_some()` (5 s deadline).

- [X] T022 [P] [US2] `card_reader_outage_past_the_window_keeps_the_registration` in `gsm-sip-bridge/src/ims/agent/registration_tests.rs`, using `reader_rig_waiting(usize::MAX)`:
  - Assert `!force_renewal`, `challenged.is_none()`, `registration_refresh.is_none()`, the session's transport and `gm_state` unchanged, and the kernel's states unchanged.
  - Drive for 300 ms more (the refresh is due the whole time, per the rig's 3100 s offset) and assert `rig.received().is_none()` (FR-011, SC-003).
- [X] T023 [P] [US2] `card_reader_card_back_sends_one_in_place_refresh` in `gsm-sip-bridge/src/ims/agent/registration_tests.rs`: build with an open counter that fails until the test flips an `AtomicBool`; give `card_after` a closure variant or wrap it. After waiting, flip it. Drive until a REGISTER arrives: it is an ordinary refresh with the cached `Authorization`, a new CSeq and no `auts=`. Assert `awaiting_card.is_none()`, and that no second REGISTER arrives within 200 ms.
- [X] T024 [P] [US2] `card_reader_margin_or_lapse_when_idle_registers_in_full` in `gsm-sip-bridge/src/ims/agent/registration_tests.rs`, with two cases:
  - (a) While waiting, set `status.expires_at = now + 30 s` (under `REFRESH_FLOOR`). The next pass gives `force_renewal == true`, `awaiting_card.is_none()`, and no REGISTER.
  - (b) Set `expires_at = now - 1 s`. Same outcome.
- [X] T025 [P] [US2] `card_reader_margin_during_a_call_keeps_waiting_and_refreshes_in_place` in `gsm-sip-bridge/src/ims/agent/registration_tests.rs`: while waiting with `active_call` set, set `expires_at = now - 1 s`. Assert `!force_renewal` and still waiting. Then bring the card back: an in-place REGISTER goes out (D5, FR-013).
- [X] T026 [P] [US2] `card_reader_waiting_ends_on_every_takeover` in `gsm-sip-bridge/src/ims/agent/registration_tests.rs`, one sub-case per exit (FR-014), each starting from `reader_rig_waiting(usize::MAX)`:
  - (a) `rig.st.force_renewal = true`;
  - (b) `rig.st.refresh_fallback = true`;
  - (c) `rig.st.on_network_deregistration(now, false, &Deregistration{ event: Unregistered, retry_after: None }, cooldown)`;
  - (d) `rig.st.on_renewal_failure(…)` failover with `rig.pool("127.0.0.2")` and a Timer F failure.
  - After one `drive()`, `awaiting_card.is_none()` and no REGISTER was sent.
- [X] T027 [US2] Update T018 in `gsm-sip-bridge/src/ims/agent/registration_tests.rs`: the window running out now ends in `awaiting_card.is_some()` with `!force_renewal`. Add `card_reader_no_second_worker_while_one_is_stuck`: SIM delay 600 ms (a hung attempt). After entering waiting, drive for 400 ms with the check backoff at 20 ms and assert `opens == 1`. Once the stuck worker returns, a check starts (`opens == 2`) (FR-015).
- [X] T028 [P] [US2] `card_reader_check_backoff_doubles_to_its_cap` in `gsm-sip-bridge/src/ims/agent/registration_tests.rs`: record the open timestamps over about 400 ms of waiting with `fast()`. Gaps grow 20 → 40 → 80 → 80 ms (±tolerance). Alternatively unit-test the pure helper that computes the next backoff.
- [X] T029 [P] [US2] `card_reader_check_card_error_falls_back` in `gsm-sip-bridge/src/ims/agent/registration_tests.rs`: while waiting, the check's `open` returns `BridgeError::Ims("card answered 6F00")`. Next pass: `force_renewal`, and `awaiting_card.is_none()`.

### Implementation for User Story 2

- [X] T030 [US2] In `gsm-sip-bridge/src/ims/agent/registration_refresh.rs` add `pub(super) struct AwaitingCard { since: Instant, check: Option<CardCheck>, next_check_at: Instant, backoff: Duration, previous: Option<Arc<AtomicBool>> }` per data-model.md. Add `awaiting_card: Option<registration_refresh::AwaitingCard>` to `LoopState` in `gsm-sip-bridge/src/ims/agent/mod.rs` (initialised to `None`, next to `challenged` at `:2154`/`:2217`), with a doc comment stating the invariants (data-model "Invariants").
- [X] T031 [US2] In `gsm-sip-bridge/src/ims/agent/registration_refresh.rs` add `fn await_card(&mut self, p: &RefreshCtx, previous: Option<Arc<AtomicBool>>)`:
  - Clear `registration_refresh` and `challenged`. Dropping `Challenged` releases `carry.reserved`.
  - Set `awaiting_card = Some(AwaitingCard { since: now, check: None, next_check_at: now + timing.check_initial, backoff: timing.check_initial, previous })`.
  - Log WARN `the SIM stayed out of reach; keeping the registration and waiting for the card` (`window_secs`, `expires_in_secs` from `p.status`).
  - Do not touch `force_renewal`.
- [X] T032 [US2] In `drive_challenge` (`gsm-sip-bridge/src/ims/agent/registration_refresh.rs`):
  - When `challenged.work.overdue(now)` and `p.reg_cfg.pcsc_reader`, call `await_card(p, Some(work.finished_flag()))`.
  - On `Err(e)` with `p.reg_cfg.pcsc_reader && e.is_card_unreachable()`, call `await_card(p, None)`.
  - Every other branch is unchanged: modem lines and card errors still go to `fall_back_to_full`.
- [X] T033 [US2] In `gsm-sip-bridge/src/ims/agent/registration_refresh.rs` add `fn drive_awaiting_card(&mut self, p: &RefreshCtx, now: Instant) -> bool` (`true` = the card is back, start the refresh now), implementing steps 1–4 of research D4 in order:
  1. If `force_renewal || refresh_fallback`, clear and return `false`.
  2. If `!self.busy()` and the binding has lapsed (`now_wall >= expires_at`, the same comparison as `in_place_refresh_possible`) or `expires_at - now_wall < REFRESH_FLOOR`: clear, set `force_renewal`, log WARN with `reason = lapsed | margin`, and return `false`.
  3. Poll the check:
     - `Ok` clears, logs INFO `the SIM is reachable again; refreshing the registration in place` (`waited_secs`), and returns `true`.
     - Unreachable sets `next_check_at = now + backoff` and `backoff = next_backoff(backoff, timing.check_max)`.
     - Any other `Err` clears and calls `fall_back_to_full(format!("the SIM answered the check with an error: {e}"))`.
     - Overdue sets `previous = Some(check.finished_flag())`, drops the check, and schedules as unreachable.
  4. If there is no check, `now >= next_check_at`, and `previous` is `None` or set: spawn `CardCheck::spawn(p.reg_cfg.clone(), open, timing.attempt_bound)` and clear `previous`.

  Make `next_backoff` reachable (`pub(super)`) if it isn't.
- [X] T034 [US2] In `drive_registration_refresh` (`gsm-sip-bridge/src/ims/agent/registration_refresh.rs:729-733`), right after the `challenged` branch, add `if self.awaiting_card.is_some() && !self.drive_awaiting_card(p, now) { return Ok(()); }`. When it returns `true`, fall through to the existing due check and `PendingRegister::start`, unchanged.
- [X] T035 [US2] In `gsm-sip-bridge/src/ims/agent/mod.rs`, in the block before a full renewal that clears `registration_refresh` and `challenged` (`:4324-4326`), also set `self.awaiting_card = None`.
- [X] T036 [US2] Run T022–T029 and every existing `registration_tests` test. Then `make format && make lint && make test`, and commit US1 and US2 together: `fix(ims): ride out an unreachable card-reader SIM during a challenged refresh` (commit 3). Body: the retry window, the waiting state, the exit paths and the modem-unchanged guarantee.

**Checkpoint**: SC-001, SC-002 and SC-003 are met by tests.

---

## Phase 5: User Story 3 - A reader outage never makes the agent exit (P2)

**Goal**: card-unreachable full-registration failures on card-reader lines are retried without counting (FR-016–FR-018, research D6).

**Independent Test**: twelve consecutive card-unreachable failures on a card-reader line never return `Err`, and the budget stays at 0.

### Tests for User Story 3 (write first, watch them fail)

Mock justification at the site: `on_idle_tick`'s full renewal calls the real `attempt_renewal`, which needs a SIM CI does not have, so these tests call the extracted `on_full_renewal_failure` directly with the `RegisterFailure` `attempt_renewal` would return.

- [X] T037 [P] [US3] `card_reader_unreachable_full_registration_never_spends_the_budget` in `gsm-sip-bridge/src/ims/agent/registration_tests.rs`: call `on_full_renewal_failure(target, CardUnreachable failure, routed = true, pcsc_reader = true)` `2 * MAX_CONSECUTIVE_RENEWAL_FAILURES` times. Assert:
  - every call is `Ok`;
  - `consecutive_renewal_failures == 0`;
  - `next_renewal_attempt.is_some()`;
  - `backoff` reached `RETRY_MAX_BACKOFF`;
  - the pool has no marks (`rig.pool("127.0.0.2")` first, then check `pool.snapshot`) (SC-004).
- [X] T038 [P] [US3] `card_reader_other_failures_still_count` in `gsm-sip-bridge/src/ims/agent/registration_tests.rs`: with `pcsc_reader = true`, alternate `CardUnreachable` and `Ims("renewal REGISTER rejected: 403 Forbidden")` (status 403) failures. Only the 403s count. The 6th 403 returns `Err` (FR-017).
- [X] T039 [P] [US3] `modem_line_unreachable_failures_still_count` in `gsm-sip-bridge/src/ims/agent/registration_tests.rs`: `pcsc_reader = false`, six `CardUnreachable` failures, and the 6th returns `Err` (FR-019).
- [X] T040 [P] [US3] `card_reader_success_after_exempt_failures_clears_the_ladder` in `gsm-sip-bridge/src/ims/agent/registration_tests.rs`: after three exempt failures, `on_renewal_success()` resets `backoff` to `RETRY_INITIAL_BACKOFF` and `next_renewal_attempt` to `None`.

### Implementation for User Story 3

- [X] T041 [US3] In `gsm-sip-bridge/src/ims/agent/mod.rs`:
  - Add `fn schedule_card_retry(&mut self)`: `next_renewal_attempt = Some(now + backoff)`, `backoff = next_backoff(backoff, RETRY_MAX_BACKOFF)`, no `count_renewal_failure`.
  - Add `fn on_full_renewal_failure(&mut self, target: IpAddr, failure: &RegisterFailure, routed: bool, pcsc_reader: bool) -> BridgeResult<()>`. If `pcsc_reader && failure.error.is_card_unreachable()`, it calls `schedule_card_retry`, logs WARN `the SIM is out of reach; retrying the registration without counting toward the restart budget` (`retry_in_secs`, `error`), and returns `Ok(())`. Otherwise it returns `self.on_renewal_failure(target, failure, routed, SystemTime::now())`.
- [X] T042 [US3] In `on_idle_tick`'s `Err(failure)` arm (`gsm-sip-bridge/src/ims/agent/mod.rs:4484`), replace the final `return self.on_renewal_failure(…)` with `return self.on_full_renewal_failure(target, &failure, routed, p.reg_cfg.pcsc_reader)`. Everything before it (status `Failed`, `last_failure`, `set_registered(false)`, `set_tunnel_up(false)`, Gm `Failed`) stays. Do not touch `register_via_pool` (`:2640`, startup, out of scope), the modem-lock timeout or the `pre_renewal` failure paths.
- [X] T043 [US3] Amend the doc comment "Why a failing renewal must eventually exit" (`gsm-sip-bridge/src/ims/agent/mod.rs:2273`) and the doc of `MAX_CONSECUTIVE_RENEWAL_FAILURES` (`:162-170`). Exiting buys `AT+CFUN`, which a card reader cannot use, so on card-reader lines a full registration that fails only because the card is unreachable is exempt (#233, Clarification Q1). The registration-loss alert is the escalation.
- [X] T044 [US3] Run `make format && make lint && make test` and commit: `fix(ims): do not spend the restart budget on an unreachable card reader` (commit 4).

**Checkpoint**: SC-004 by tests; SC-005 follows from the backoff cap (≤ 120 s).

---

## Phase 6: User Story 4 - The operator can tell a card outage from a failed SIM (P3)

**Goal**: the status value and log lines (FR-020, FR-021, contracts/status-and-logs.md).

- [X] T045 [P] [US4] In `gsm-sip-bridge/src/vowifi/mod.rs` tests (next to `:2891-2920`), add a `format_refresh_lines` case with `last = Some((t, "awaiting-card"))` that renders `last_refresh: awaiting-card <format_unix(t)>`.
- [X] T046 [P] [US4] In `gsm-sip-bridge/src/ims/agent/registration_tests.rs` extend T022: after waiting starts, `rig.st.last_refresh` has the value `awaiting-card`. Extend T023: after the card is back and the refresh is answered `200`, it reads `ok`.
- [X] T047 [US4] In `await_card` (`gsm-sip-bridge/src/ims/agent/registration_refresh.rs`) set `self.last_refresh = Some((SystemTime::now(), "awaiting-card".to_string()))`. Check that the five log lines in contracts/status-and-logs.md exist with those exact messages and fields, and that none logs the IMSI: `grep -n 'out of reach\|reachable again\|about to end' gsm-sip-bridge/src`.

---

## Phase 7: Polish & Cross-Cutting Concerns

- [X] T048 [P] `docs/observability.md:173`: list the full `last_refresh` value set: `ok`, `rejected <status>`, `timeout`, `error`, `awaiting-card` (a card-reader line is waiting for its SIM and keeping its registration), and `none`.
- [X] T049 [P] `docs/omnikey-pcsc-vowifi.md` Troubleshooting: add an entry "The card was out of reach". Describe what the line does (asks again for 30 s, keeps the registration and checks the card, registers in full only near expiry, never restarts for a reader outage). Name the log lines and `last_refresh: awaiting-card` to look for, and note that the start-up case still restarts the agent.
- [X] T050 [P] `docs/greptile-review-learnings.md` Part 1, "Concurrency & shared devices": add a line saying that a card-reader line's "card out of reach" is `BridgeError::CardUnreachable`. Test it with `is_card_unreachable()`, and never with PC/SC message text. Build new PC/SC failures through `pcsc_card::pcsc_failure`.
- [X] T051 [P] `RELEASE_NOTES.md` `## Unreleased`: one bullet per `.cursor/rules/release-notes.mdc`, with only the summary bolded (memory: release-note bold scope). Cover:
  - a card-reader line now rides out a SIM that is briefly out of reach during a challenged refresh, and keeps its registration while the card is out;
  - it never restarts the agent for a reader outage;
  - the new `last_refresh: awaiting-card`;
  - modem lines are unchanged;
  - the challenged path is covered by tests only, because no available carrier challenges refreshes (#233, completes #232).
- [X] T052 Self-review the whole diff against CLAUDE.md's checklist and docs/greptile-review-learnings.md Part 1, recording the answers in the PR description:
  - every `awaiting_card` reader and writer (`grep -rn 'awaiting_card' gsm-sip-bridge/src`);
  - every new error string against `grep -rn 'contains(\|starts_with(' gsm-sip-bridge/src/ims`;
  - every new constant against `grep -rn 'MAX_\|_BACKOFF\|_WINDOW\|_BOUND' gsm-sip-bridge/src/ims`;
  - every spawned thread: who cancels it, and who observes `done`;
  - the next REGISTER after the card is back carries the current Via, Security-Verify and connection.
- [X] T053 On the reader host: `CARGO_TARGET_DIR=$PWD/target-162 cargo test -p gsm-sip-bridge --lib pcsc_card::hardware -- --ignored --test-threads=1`. T006's tests and every one of #234's hardware tests must pass. If no reader is available, say so in the PR and leave this unticked.
- [ ] T054 Live, quickstart §3 (no regression on the local card-reader rig) and, with the maintainer, §4 (a reader outage past the registration's expiry does not exit the agent). Record exactly what was and was not exercised under Deviations. Never claim the challenged path was tested live.
- [X] T055 Run `make format && make lint && make test` and commit the docs: `docs: card-reader outage behaviour, status value and release note` (commit 5). Push with an explicit refspec (`git push -u origin 162-pcsc-card-unreachable:162-pcsc-card-unreachable`; memory: worktree push tracking trap). Open the PR with `Closes #233`, the test counts, the T052 answers and what T053/T054 covered. `greploop` and `/code-review` run only when the maintainer asks.

---

## Dependencies & Execution Order

- **Phase 1 → T002–T007 (commit 1) → T008–T012 (commit 2) → US1 → US2 (commit 3) → US3 (commit 4) → US4 → Polish (commit 5) → PR.**
- US1 depends on Phase 2 (typed error, `spawn_with`, `CardTiming`, `card_after`).
- US2 depends on US1 (it changes what happens when US1's window ends) and on T010 (`CardCheck`).
- US3 depends only on Phase 2's typed error (T003–T005). It touches `mod.rs`'s failure tail, which is disjoint from US1 and US2, so it can be developed in parallel and committed after commit 3.
- US4's T047 depends on T031 (`await_card`). T045 is independent.
- Within each story, the tests marked [P] are written together first (all in `registration_tests.rs`, so write them in one pass). Implementation tasks are sequential.

## Parallel Example: User Story 1

```text
T013  registration_tests.rs  two resets, then the answer
T014  registration_tests.rs  reset in the middle of the AKA
T015  registration_tests.rs  retry finds the SQN moved, so resync
T016  registration_tests.rs  card error, no retry
T017  registration_tests.rs  modem line unchanged
T018  registration_tests.rs  window is the deadline
(same file: write them together, then run them and watch them fail)
```

## Parallel Example: Polish

```text
T048 docs/observability.md   T049 docs/omnikey-pcsc-vowifi.md   T050 docs/greptile-review-learnings.md   T051 RELEASE_NOTES.md
```

## Implementation Strategy

- **MVP = Phase 2 + US1 + US2 (commits 1–3).** That fixes #232's observed failure and every outage shorter than the registration. It is shippable alone.
- **Increment = US3 (commit 4).** It stops the restart loop when an outage outlasts the registration.
- **US4 + Polish (commit 5).** Operator visibility, docs and the release note.

## Notes

- Hardware-gated tests share one physical card: always `--test-threads=1`.
- `tmp/` is scratch and never committed.
- Timing-based rig tests use `CardTiming::fast()` and generous safety deadlines (5 s), never tight sleeps, so CI load cannot flake them.

## Deviations (recorded during implementation)

- **Four commits, not five: the worker refactor ships with the retry and the waiting state.** T008–T012 were planned as a separate "no behaviour change" commit 2. `CardTiming`, `CardCheck` and `SimWorker::finished_flag` have no production caller until US1/US2 use them, and `make lint` (`clippy -D warnings`) rejects that dead code, so they are in the same commit as US1 and US2. The status value and its test (T045–T047) are in that commit too, because `await_card` sets it. Commits:
  1. typed error;
  2. worker, retry and waiting state;
  3. budget exemption;
  4. docs.
- **`Reauthentication::spawn` keeps one attempt; card-reader lines use `spawn_retrying`.** T009/T019 planned one `spawn_with(job, open, Option<CardTiming>)` with `open: Fn`. Instead `spawn` (modem lines) keeps its `FnOnce` signature and exact behaviour, and `spawn_retrying(job, open: Fn, CardTiming)` is the card-reader path. `begin_challenge` picks by `pcsc_reader`. No requirement changed.
- **The test opener lives in `registration_auth::gated`, not `scripted`.** `card_after(n, …)` is built on `card_when(predicate, …)`, which T023/T025 use to bring the card back on a flag.
- **T018 and T027 are one test**, `card_reader_stuck_card_ends_the_challenge_at_its_deadline_and_blocks_the_next_check`. The SIM is stuck for 1.2 s, not 600 ms: at 600 ms the stuck attempt would return within the 400 ms observation window the task described, so the assertion "no second worker" would not hold for the whole window.
- **T028 is a unit test of the backoff step** (`the_card_check_backoff_doubles_to_its_cap`, both `fast()` and production timing), not a measurement of open timestamps, which would be timing-flaky in CI.
- **The waiting log line also carries `error`** (the last card error, or `the SIM did not answer in time`). `contracts/status-and-logs.md` is updated.
- **Not tested by a dedicated test:** clearing the wait before an `on_idle_tick` full registration (T035). `on_idle_tick` needs a real `attempt_renewal`, which needs a SIM. Every path that wants a full registration sets `force_renewal` or `refresh_fallback`, and `card_reader_waiting_ends_on_every_takeover` covers that check.
- **T053 ran on this host's OMNIKEY reader (host pcscd) on 2026-11-24: all 10 hardware tests pass, including the two new ones.** The live `pr228-reader` container sharing the card stayed `Registered` with `last_failure: none` through the run.
- **T055: PR #235 opened on 2026-11-24.**
- **T054 (live), run on 2026-11-24 on the local rig (`gsm-sip-bridge:pr235`, built from `git archive` of a8a0944). Left unticked: §4 is only partly shown.** Times are IST.
  - **§3, no regression: passed.** It registered (`outcome="registered"`, `connected to PC/SC reader`) on three container starts. An in-place refresh cycle completed at 19:41:30 (`registration refreshed in place`, `last_refresh: ok`). No `has been reset` and no `SIM card unreachable` appeared while the card was in place.
  - **§4, outage across a full registration: the wiring works; the agent did not run past 6 failures.**
    - Setup: the reader was unbound from USB via a throwaway privileged container (`echo 5-1.1.3.1 > /sys/bus/usb/drivers/usb/unbind`). The P-CSCF was routed to the silent veth peer. `[vowifi].gm_options_keepalive` was temporarily on: it is off by default, and without it nothing escalates.
    - At 18:40:30 the keepalive escalation started a full registration. It failed as `IMS error: SIM card unreachable: no real PC/SC reader found …`, and the agent logged `the SIM is out of reach; retrying the registration without counting toward the restart budget` with `retry_in_secs=5`, then `10`. The route was removed after the first failure.
    - At 18:40:35 the supervisor saw PcscfPoolExhausted: the escalation itself had marked the line's only P-CSCF, not the card failure. It tore down the IKE SA. IKE re-authentication needs EAP-AKA from the SIM (`SCardConnect: No smart card inserted`, `AUTHENTICATION_FAILED`). At 18:40:55 it restarted the agent, and an agent started with no card exits (startup is out of scope). So the line restart-looped every 5 s until the reader came back.
    - With the reader rebound at 18:51:23, the line was registered again at 18:51:30.
    - Not shown live: the agent keeping its own process past 6 card-unreachable failures (SC-004). No trigger for a full registration on this rig leaves the P-CSCF unmarked. Vodafone's in-place refreshes are unchallenged and do not touch the card, and every Gm-trouble escalation marks the only P-CSCF, which makes the supervisor rebuild a tunnel that needs the SIM. SC-004 rests on the unit tests.
  - The challenged paths (US1, US2) cannot be tested live: the local carrier does not challenge refreshes.
  - The rig config was restored afterwards (keepalive off), and the rig left running `pr235`.
- **Tests were written after the code, not before.** Each was checked against a mutation of the code it covers: removing the wait gate fails 6 tests; using the modem spawn for card readers fails 4; removing the takeover check fails 1; removing the "previous worker done" guard fails 1.
