# Tasks: A Slow SIM Answer No Longer Costs a Modem Line Its Registration, or Corrupts the Next Session on the Port

**Input**: `specs/164-modem-csim-port-sync/` (spec.md, plan.md, research.md F1–F15 and D1–D11, data-model.md, contracts/operator-surface.md, quickstart.md)

**Tests**: requested. The constitution makes TDD the default, and every acceptance scenario in the spec is testable without hardware. Write each story's tests first and watch them fail for the right reason. Once a test passes, check it against a mutation of the code it covers (for example, revert the sync to the old drain, or send `AUTHENTICATE` with the default budget), and record which mutation fails which test in that task's line.

**Paths**: source is `gsm-sip-bridge/src/...`; docs are at the repo root. Use `CARGO_TARGET_DIR=$PWD/target-161` for every cargo/make command, run from the repo root (an isolated, warm target dir; memory: shared target dir clobbered by worktrees).

**Test doubles**:
- The AT port is a real `UnixStream` pair, scripted by `spawn_modem` (`at_worker.rs` tests): real fds, real blocking and real timeouts, as the existing tests there do.
- The open path goes through `AtCommander::start_for_test` (T011), which runs the same worker spawn and `Request::Sync` as `open_with_timeout`.
- Justification for not using a real serial port: CI has no modem.
- `try_reopen`'s path can't run without a tty. It shares `start_worker` with the open path, so it is covered through that.

**Never** put a real IMSI, IMEI or MSISDN in code, tests or docs. Tests use `001010000000001` and `350000000000001`.

**Commits** (no dead code at any commit; `clippy -D warnings`):
1. `Session::sync` replaces the drain and resync;
2. sync on open and reopen, `PortOutOfStep`, the retry loops, sweep budgets;
3. the AKA budget, the trait hook, the bridge, the watchdog derivation;
4. logging;
5. docs, release notes, learnings.

## Phase 1: Setup

- [X] T001 Confirm the branch is `164-modem-csim-port-sync` with no upstream (`git rev-parse --abbrev-ref @{u}` fails). Run `CARGO_TARGET_DIR=$PWD/target-161 make test` from the repo root and record the passed/ignored counts here as the baseline.

  **Done.** No upstream (`fatal: no upstream configured`). Baseline at `744348b`: 3526 passed, 0 failed, 11 ignored.

---

## Phase 2: Foundational: a label on every port session (blocks US1 and US3)

**Purpose**: the sync's warning (D10) and its error detail name the port. `Session` has no idea which port it drives today.

- [X] T002 In `gsm-sip-bridge/src/modules/at_worker.rs` add `label: String` to `Session` and make `Session::new(port, timeout, label: impl Into<String>)`. Update the three callers:
  - `at_commander.rs:274` (`open_session`): pass `path.display().to_string()`;
  - `at_commander.rs:282` (`from_stream`): pass `"stream"`;
  - `at_worker.rs:407` (test `port()`): pass `"test"`.

  No behaviour change; it is folded into commit 1.

---

## Phase 3: User Story 1 - A session on a modem port never reads another session's reply (P1) 🎯 MVP

**Goal**: FR-001, FR-001a, FR-002, FR-003 and FR-004. Research D1, D2, D3, D4, D8.

**Independent test**: a socket modem that leaves a stale reply (whole, a lone `OK`, still running, or from an earlier sync) before or during a session. The first command gets its own reply and every later command stays in step. A silent port fails within the bound with a typed timeout.

### Tests for User Story 1, part A: the sync inside a session (write first, in `gsm-sip-bridge/src/modules/at_worker.rs` tests)

- [X] T003 [US1] Add a `sync_modem` helper beside `spawn_modem`: a scripted far end that answers `AT+CMEE?` with `\r\n+CMEE: 1\r\n\r\nOK\r\n` and `AT+CSCS?` with `\r\n+CSCS: "GSM"\r\n\r\nOK\r\n`. Its options: bytes to send before the first command, a delay before an answer, echo on, and an `ERROR` mode. Then write these tests against `Session::sync(bound)` (it doesn't exist yet, so they fail to compile; that is the first red):
  - `sync_discards_a_stale_reply_sent_before_it`: `\r\n+CSIM: 4,"FFFF"\r\n\r\nOK\r\n` is waiting in the socket. The sync returns `Ok(n)` with `n` equal to those bytes, and the next `send_command("AT+CSQ")` gets its own scripted reply;
  - `sync_is_not_fooled_by_a_lone_stale_ok`: a stale `\r\nOK\r\n`. The next command gets its own reply;
  - `sync_waits_out_a_command_the_modem_is_still_running`: the far end delivers a stale `+CSIM` reply 300 ms *after* receiving `AT+CMEE?`, then answers both queries. With the bound at 2 s, the sync succeeds and the next command is in step;
  - `sync_survives_a_stale_cmee_reply_from_an_earlier_sync`: a stale `+CMEE: 1`/`OK` ahead of our own replies. The next command is in step;
  - `sync_survives_a_stale_cscs_reply_from_an_earlier_sync`: a stale `+CSCS: "GSM"`/`OK` ahead. The next command is in step;
  - `sync_ignores_command_echo`: echo on (each command line is echoed first). It succeeds, and only the two replies count as ours;
  - `sync_with_nothing_stale_reports_zero_and_sends_exactly_two_queries`: the far end records what it received, `AT+CMEE?\r\n` then `AT+CSCS?\r\n`, and nothing else;
  - `sync_fails_within_its_bound_on_a_silent_port`: no answers, bound 300 ms. It returns `Err` with text `AT command timeout` after ≥ 300 ms and < 300 ms + one read timeout, and `desynced` stays set;
  - `sync_fails_when_the_modem_refuses_both_queries`: `ERROR` to both. It returns `Err` within the bound;
  - `sync_sends_the_second_query_only_after_the_first_reply`: the far end checks that `AT+CSCS?` arrives after it wrote the `+CMEE` reply.

  **Done.** `test_modem` (in `at_worker.rs`, shared by every sync-answering test) is the `sync_modem` helper: real `UnixStream`, `Script { echo, refuse_sync, sync_delay, before_first_sync, late_after, replies }`, and an rx/tx event log. Mutations: accepting `OK` without a `+CMEE:`/`+CSCS:` line first fails 5 tests (stale-before, lone OK, still-running, stale CMEE, stale CSCS); dropping the second query fails 3 (stale CMEE, ordering, exactly-two-queries); writing both queries before reading fails `sync_sends_the_second_query_only_after_the_first_reply`.
- [X] T004 [US1] Rewrite the drain and resync tests in `gsm-sip-bridge/src/modules/at_worker.rs` (the late-reply tests around `:430`–`:480` and `resync_*` at `:729`–`:762`) to the new contract:
  - `a_late_reply_after_a_timeout_is_not_read_as_the_next_answer`: the first command times out; its reply arrives **after** the next command's write (the case the old drain missed, F3); the next command gets its own reply. This test must fail against today's `discard_pending`; record that;
  - `resync_succeeds_when_the_modem_answers_again`: scripted with the two sync replies;
  - `resync_fails_on_a_silent_modem`;
  - the third resync test, scripted the same way.

  Keep every assertion on the `AT command timeout` text.

  **Done.** `a_late_reply_after_a_timeout_is_not_read_as_the_next_answer` failed against today's `discard_pending` (the next command read `+CSIM: 4,"9000"`), recorded before the fix. `a_command_after_an_abandoned_flood_reads_its_own_answer` was rescripted with `test_modem` too.

### Implementation for User Story 1, part A

- [X] T005 [US1] Implement `pub(crate) fn sync(&mut self, bound: Duration) -> BridgeResult<usize>` in `gsm-sip-bridge/src/modules/at_worker.rs`, following research D2 and data-model "Sync exchange":
  1. One deadline, `now + bound`.
  2. Count and clear `rx`.
  3. Write `AT+CMEE?\r\n`. Read lines with `fill`/`take_line` until a line starting `+CMEE:` is followed by `OK`.
  4. Then write `AT+CSCS?\r\n` and read the same way for `+CSCS:`.
  5. Count every other line's bytes (plus its line ending) as discarded.

  On success, clear `desynced` and return the count. On the deadline, set `desynced` and return `BridgeError::Discovery("AT command timeout".into())`. A write error sets `desynced` and returns the existing `AT write failed: …` error. Put the two query strings in `const SYNC_FIRST: &str` / `SYNC_SECOND: &str`, with a doc comment: nothing else may send them (research F12), and they write no modem setting (FR-004).
- [X] T006 [US1] In `gsm-sip-bridge/src/modules/at_worker.rs`:
  - in `send_command_within`, replace the `if self.desynced { discard_pending; desynced = false }` block with `if self.desynced { self.sync(self.timeout)?; }`;
  - make `Request::Resync` in `run` call `session.sync(session.timeout).map(|_| ())`;
  - delete `discard_pending`, `resync` and `RESYNC_TIMEOUT`;
  - update the doc comments on `desynced`, `fill` and the module doc's "Why the read buffer lives here" so they describe the sync, not the drain.

  `read_line_raw` is unchanged (it writes nothing). Run the T003/T004 tests (green), then `make format && make lint && make test`. Commit: `fix(at): bring a modem AT channel back into step before trusting it after a timeout (#237)`.

  **Done.** Commit `ad33628`. Gate: 3536 passed, 0 failed.

### Tests for User Story 1, part B: sync on open and reopen (write first)

- [X] T007 [P] [US1] In `gsm-sip-bridge/src/error.rs` tests:
  - `port_out_of_step_reads_as_a_timeout_to_every_classifier`: `BridgeError::PortOutOfStep("x".into())`;
    - displays as `discovery error: AT command timeout: x`;
    - `is_port_out_of_step()` is true, and `Discovery("x")` gives false;
    - `crate::ims::session::map_registration_error` gives `RegistrationStatus::Timeout`;
    - `crate::supervise::sim_recovery::has_csim_failure(&e.to_string())` is false;
    - the text contains `timeout`, which is what `modules/worker.rs:253`, `:829` and `bin/gsm_echo.rs:154` test.
- [X] T008 [P] [US1] In `gsm-sip-bridge/src/modules/at_commander.rs` tests, against `AtCommander::start_for_test(stream, timeout)` (T011) with a `UnixStream` far end:
  - `a_fresh_open_discards_the_previous_holders_late_reply`: the far end writes `\r\n+CSIM: 4,"FFFF"\r\n\r\nOK\r\n` as soon as it is connected, then answers the sync and an `AT+CSQ`. `start_for_test` returns `Ok`, and `check_signal()` parses the real `+CSQ`;
  - `a_fresh_open_on_a_silent_port_fails_typed_within_its_bound`: no answers, timeout 300 ms. It returns `Err(e)` with `e.is_port_out_of_step()` true, in < 300 ms + `WORKER_GRACE` + 200 ms;
  - `an_open_that_fails_its_sync_releases_the_worker`: after the failed open, the far end's socket sees EOF within the bound plus 1 s (the worker exited and dropped the port).

  **Done.** Mutations: not sending `Request::Sync` at all fails all three; sending it but not waiting for it fails the silent-port and releases-the-worker tests (the stale-reply test still passes then, because the queued sync still runs before the first command).
- [X] T009 [P] [US1] In `gsm-sip-bridge/src/ims/mod.rs` tests (next to the `open_modem_waiting` tests at `:2313`–`:2355`):
  - `open_modem_waiting_does_not_retry_an_out_of_step_port`: `retryable` returns false for the first error, so there is exactly one attempt and no sleep;
  - `open_modem_waiting_measures_its_wait_on_the_clock`: an `open` that blocks 50 ms per attempt plus a 10 ms interval, with `max_wait` 200 ms, gives up by about 200 ms on the clock (not after 20 sleeps);
  - update the existing three tests to pass `retryable = |_| true`.
- [X] T010 [P] [US1] Retry-loop tests:
  - in `gsm-sip-bridge/src/volte/sms.rs` tests, `open_with_retry_gives_up_at_once_on_an_out_of_step_port`. If `open_with_retry` takes a path, make its open function injectable the way `usim_bridge::try_open_with_backoff` already is (an `FnMut() -> BridgeResult<AtCommander>`), keeping the production wrapper;
  - in `gsm-sip-bridge/src/vowifi/usim_bridge.rs` tests, `try_open_with_backoff_gives_up_at_once_on_an_out_of_step_port`: the opener returns `PortOutOfStep` and is called once, with no sleep.

  **Done.** `volte/sms.rs`: `open_with_retry` now wraps an injectable `open_with_retry_using`. Mutations: removing the `is_port_out_of_step` arm fails each loop's test.

### Implementation for User Story 1, part B

- [X] T011 [US1] In `gsm-sip-bridge/src/error.rs` add `#[error("discovery error: AT command timeout: {0}")] PortOutOfStep(String)`, with a doc comment in the style of `CardUnreachable`: built only by `AtCommander::start_worker`, recognised with `is_port_out_of_step()` and never by its text, worded so the `contains("timeout")` classifiers keep reading it as a timeout. Add `pub fn is_port_out_of_step(&self) -> bool`.

  In `gsm-sip-bridge/src/modules/at_worker.rs`, add `Request::Sync { reply: mpsc::Sender<BridgeResult<()>> }`, handled in `run` as `session.sync(session.timeout).map(|_| ())`.

  In `gsm-sip-bridge/src/modules/at_commander.rs`, extract `fn start_worker(session, path: Option<PathBuf>, timeout, thread_name) -> BridgeResult<Transport>`. It spawns the worker, sends `Request::Sync`, and waits `timeout + WORKER_GRACE`.
  - On `Ok(Ok(()))` it returns `Transport::Worker { state: Healthy }`.
  - On any failure it drops `tx` and returns `PortOutOfStep(format!("{} did not answer its sync within {} ms", label, timeout.as_millis()))`.

  Use it in:
  - `open_with_timeout`;
  - `try_reopen`, where a failed sync returns `false`;
  - a new `#[cfg(test)] pub(crate) fn start_for_test<S: Read + Write + Send + 'static>(stream: S, timeout: Duration) -> BridgeResult<Self>`. `UnixStream` gets its read timeout set by the caller.

  Update `open_with_timeout`'s doc comment (`:203`–`:239`): it now proves the channel is in step before returning, and fails typed if it cannot.
- [X] T012 [US1] In `gsm-sip-bridge/src/ims/mod.rs`, change `open_modem_waiting(port, max_wait, interval, open, retryable, sleep)`:
  - add `retryable: impl Fn(&E) -> bool`;
  - replace `waited` with a deadline (`Instant::now() + max_wait`, checked after each failed attempt);
  - return a non-retryable error at once;
  - keep the "modem serial busy" warning for retryable errors only, and the "acquired after waiting" info line (computed from the clock).

  The production caller at `:1122` passes `|e: &BridgeError| !e.is_port_out_of_step()`. Fix the `Clock`/`sleep` injection so T009's clock test is deterministic (pass a `now: impl FnMut() -> Instant` alongside `sleep` if needed).

  **Done.** `open_modem_waiting` takes `retryable` and `now` (a `FakeClock` in tests). Mutation: ignoring `retryable` fails `open_modem_waiting_does_not_retry_an_out_of_step_port`.
- [X] T013 [P] [US1] In `gsm-sip-bridge/src/volte/sms.rs` `open_with_retry` and `gsm-sip-bridge/src/vowifi/usim_bridge.rs` `try_open_with_backoff`: on `Err(e) if e.is_port_out_of_step()`, return `Err(e)` without sleeping or retrying. Update each doc comment: the loop waits out another *holder*, not an unresponsive modem.
- [X] T014 [US1] Sweep derived budgets (research D7) in `gsm-sip-bridge/src/volte/sms.rs`:
  - `PER_MESSAGE_PORT_BUDGET` 30 → 35 s;
  - `SWEEP_PASS_BUDGET` 60 → 72 s.

  In all four derivation tests (`a_signal_poll_fits…` `:1263`, `the_listing_step_fits…` `:1314`, `the_per_step_budgets_cover…` `:1339`, and `a_sweep_pass_cannot_outrun…` `:1280` via the constant), make `open_worst` `OPEN_RETRY_BASE_DELAY * (1 + 2 + 3) + crate::modules::at_commander::DEFAULT_TIMEOUT`, with a comment (one attempt can reach the sync; an out-of-step port is not retried). Update the constants' doc comments.

  In `gsm-sip-bridge/src/ims/agent/watchdog.rs` `derived_renewal_worst_case`, add the open's sync (`DEFAULT_TIMEOUT`) and `AT+CIMI`/`AT+CGSN` (2 × `DEFAULT_TIMEOUT`), with comments. It must still pass with ≥ 20% margin.

  **Done.** Margins: signal poll 36.8 s, listing step 61.8 s, per-message 31.8 s ≤ 35 s, pass 72 s (25% under 90 s). Renewal at this commit: 229 s.
- [X] T015 [US1] Grep the class (CLAUDE.md "fix the class"):
  - `grep -rn 'serialport::new\|at_worker::Session::new\|Session::new(' gsm-sip-bridge/src`: every real port session goes through `open_session` → `start_worker`;
  - `grep -rn 'AtCommander::open' gsm-sip-bridge/src`: all 25 production sites (research F11) reach `open_with_timeout`;
  - `grep -rn 'fn .*open.*retry\|OPEN_RETRY\|open_modem_waiting' gsm-sip-bridge/src`: no other open-retry loop.

  Record the output, and any site that needs a change, as a Blast Radius addendum in `specs/164-modem-csim-port-sync/plan.md`. Confirm `spawn_fallback_restart` (D8) needs no code change. Run T007–T010 (green), then `make format && make lint && make test`. Commit: `fix(at): prove a freshly opened modem port is in step before its first command (#237)`.

  **Done.** Grep output and decisions: plan.md, "Addendum: class grep during implementation (T015)". No site needed a change beyond the three loops; `spawn_fallback_restart` and discovery need none. Commit `6987e62`. Gate: 3544 passed, 0 failed.

**Checkpoint**: US1 is complete: no session on a modem port can read another session's reply, apart from the case recorded in the spec's Assumptions.

---

## Phase 4: User Story 2 - A SIM that answers a challenge slowly but correctly keeps the registration (P1)

**Goal**: FR-005, FR-006, FR-007, FR-008, FR-009 and FR-010. Research D5, D6, D7.

**Independent test**: through the real `AtCommander` worker, with a port timeout far shorter than the SIM's answer time, AUTHENTICATE and its GET RESPONSE succeed while any other APDU times out. The bridge relays a slow AUTHENTICATE successfully. Card-reader transports take the default path.

### Tests for User Story 2 (write first)

- [X] T016 [P] [US2] In `gsm-sip-bridge/src/modules/usim.rs` tests, using `AtCommander::start_for_test` with a port timeout of 200 ms and a socket far end that answers the sync and then:
  - `authenticate_waits_for_a_slow_sim_beyond_the_port_timeout`: `AT+CSIM=…008800…` answered after 1 s with `+CSIM: …,"DB08…9000"` (a synthetic RES/CK/IK). `authenticate` returns `AkaResult::Success`;
  - `the_get_response_after_authenticate_gets_the_aka_budget_too`: AUTHENTICATE answered `6135` at once, and the GET RESPONSE `00C0000035` answered after 1 s. Success;
  - `other_apdus_keep_the_port_timeout`: a SELECT (`select_usim`) answered after 1 s fails with text containing `AT command timeout`;
  - `an_explicit_csim_error_keeps_its_wording`: AUTHENTICATE answered `ERROR`. The error text contains `AT+CSIM failed`;
  - `the_aka_budget_is_twenty_seconds`: `AKA_RESPONSE_BUDGET.timeout == Duration::from_secs(20)` and `max_lines == at_worker::MAX_RESPONSE_LINES`.
- [X] T017 [P] [US2] In `gsm-sip-bridge/src/modules/usim.rs` tests: `a_transport_without_an_override_sends_aka_through_transmit_apdu`. `RecordingTransport` (`:522`) records the AUTHENTICATE and the GET RESPONSE through `transmit_apdu`, which pins FR-010 for `PcscTransport`, which does not override the hook.
- [X] T018 [P] [US2] In `gsm-sip-bridge/src/vowifi/usim_bridge.rs` tests, with `start_for_test` (port timeout 200 ms):
  - `a_relayed_authenticate_gets_the_aka_budget`: `forward_apdu` with an `00 88 00 81 22 …` APDU answered after 1 s returns the SIM's bytes;
  - `the_chased_get_response_after_a_relayed_authenticate_gets_the_aka_budget`: AUTHENTICATE answered `6135` at once, and the chase answered after 1 s;
  - `a_relayed_select_keeps_the_port_timeout`: a SELECT answered after 1 s gives `Err` from `forward_apdu`.

  **Done.** The chase test scripts `6103` and a GET RESPONSE `00C0000003` (the bridge chases with the SIM's own `Le`).

### Implementation for User Story 2

- [X] T019 [US2] In `gsm-sip-bridge/src/modules/at_worker.rs`, make `MAX_RESPONSE_LINES` `pub(crate)`. In `gsm-sip-bridge/src/modules/usim.rs`:
  - add `pub(crate) const AKA_RESPONSE_BUDGET: ResponseBudget = ResponseBudget { max_lines: MAX_RESPONSE_LINES, timeout: Duration::from_secs(20) }`, with a doc comment covering the incident numbers, the registrar's wait and the 600 s thread bound (research D5);
  - add `pub(crate) fn csim_within(at, apdu_hex, budget)` (today's `csim` body using `send_command_within`), and make `csim` call it with the port default by calling `send_command`. Keep both error strings byte-for-byte;
  - add the trait method `fn transmit_aka_apdu(&mut self, apdu_hex: &str) -> BridgeResult<String> { self.transmit_apdu(apdu_hex) }`, documented: AUTHENTICATE and the GET RESPONSE that collects its result;
  - make `authenticate` use it for both APDUs (`:313`, `:320`);
  - override it in `impl ApduTransport for AtCommander` with `csim_within(self, apdu_hex, AKA_RESPONSE_BUDGET)`.
- [X] T020 [US2] In `gsm-sip-bridge/src/vowifi/usim_bridge.rs`, give `send_raw_apdu` a `budget: ResponseBudget` parameter, and update its doc comment. In `forward_apdu`, pick `usim::AKA_RESPONSE_BUDGET` when `apdu.len() >= 5 && apdu[1] == 0x88`, otherwise the port default (a `usim::DEFAULT_APDU_BUDGET`, or pass `None` through a small enum; choose the form that keeps `csim`'s default path identical). Use the same budget for the `61XX` chase of that APDU. The SELECT P2 retry stays default. Update `forward_apdu`'s doc comment.
- [X] T021 [US2] In `gsm-sip-bridge/src/ims/agent/watchdog.rs` `derived_renewal_worst_case`, add `crate::modules::usim::AKA_RESPONSE_BUDGET.timeout * 2` (AUTHENTICATE and GET RESPONSE), with a comment. It gives about 269 s against 360 s and must keep ≥ 20%. Run T016–T018 (green), then `make format && make lint && make test`. Commit: `fix(usim): give a modem SIM's AKA answer 20 s instead of the port's 5 s (#237)`.

  **Done.** Renewal derivation: 30 + 5 + 10 + 100 + 40 + 64 + 20 = 269 s against 360 s (34%). Mutations: the `AtCommander` override reverted to `csim` fails both slow-AKA tests; `authenticate`'s GET RESPONSE through `transmit_apdu` fails `the_get_response_after_authenticate_gets_the_aka_budget_too`; the bridge's chase at the default budget fails the chase test; the AKA budget on every relayed APDU fails `a_relayed_select_keeps_the_port_timeout`. Commit `ba28ded`. Gate: 3553 passed, 0 failed.

**Checkpoint**: US2 is complete: a SIM answering in 5–20 s completes the refresh in place, the bridge's EAP-AKA too, and card readers are unchanged.

---

## Phase 5: User Story 3 - The operator can see how long the SIM takes and how often a port was out of step (P2)

**Goal**: FR-011 and FR-012. Research D9, D10. Contract: `contracts/operator-surface.md`.

**Independent test**: a sync that discarded bytes logs exactly one warning naming the port, and a clean sync logs nothing above debug. Each AKA run produces one timing line with the three durations and the outcome.

### Tests for User Story 3 (write first)

- [X] T022 [P] [US3] In `gsm-sip-bridge/src/modules/at_worker.rs` tests, add a small log-capture helper: `tracing_subscriber::fmt()` with a `MakeWriter` over `Arc<Mutex<Vec<u8>>>`, installed with `tracing::subscriber::with_default`. It is the first in the crate, so keep it `#[cfg(test)]` in this module. Tests:
  - `a_sync_that_discarded_bytes_warns_once_with_port_and_count` (label `"test"`, context `open`; one `WARN` line containing `discarded_bytes=` and `port=test`);
  - `a_clean_sync_logs_nothing_above_debug`;
  - `the_sync_after_a_timeout_reports_context_after_timeout`.
- [X] T023 [P] [US3] In `gsm-sip-bridge/src/ims/registration_auth.rs` tests, using the existing scripted `Sim`:
  - `each_aka_run_logs_one_timing_line` (refresh path: one `SIM challenge timings` debug line, with `authenticate_ms` ≥ the scripted delay and `outcome=success`; with a scripted AUTS, `outcome=sync-failure`);
  - `a_card_reader_aka_logs_no_select_time` (an `OpenedCard` with `select: None` gives no `select_ms` value, or a `select_ms` of `none`; pick one and pin it to the contract).

  **Done.** `select_ms` is absent on a card-reader line (a `None` field is not recorded), pinned by `a_card_reader_aka_logs_no_select_time` and matching the contract.

### Implementation for User Story 3

- [X] T024 [US3] In `gsm-sip-bridge/src/modules/at_worker.rs`, give `sync` a `context: SyncContext` parameter (`Open`, `Resync`, `AfterTimeout`), passed from `Request::Sync`, `Request::Resync` and `send_command_within`. When it discarded more than 0 bytes, it emits exactly the contract's `tracing::warn!(port = %self.label, discarded_bytes, context = context.as_str(), "discarded stale bytes before the AT channel was in step")`. Otherwise it emits `tracing::debug!`.
- [X] T025 [US3] In `gsm-sip-bridge/src/ims/mod.rs`:
  - add `#[derive(Debug, Clone, Copy, Default)] pub(crate) struct OpenTimings { pub(crate) open: Duration, pub(crate) select: Option<Duration> }` and the field `opened: OpenTimings` on `OpenedCard`;
  - fill it in `open_card`: time the identity/connect part, and the modem's `discover_usim_aid` + `select_usim`;
  - add `pub(crate) fn log_aka_timings(opened: &OpenTimings, aka: Duration, outcome: &BridgeResult<AkaResult>)` with the contract's debug line;
  - call it after `run_aka` at `ims/mod.rs:1310` (full registration, including the AUTS retry) and in `registration_auth::answer_challenge` (refresh), which means threading `opened` from `answer_once`'s destructure;
  - add `opened: Default::default()` to the test literals at `registration_auth.rs:426`, `:440`, `:441` and `:585`.

  Run T022/T023 (green), then `make format && make lint && make test`. Commit: `feat(observability): log a SIM's AKA timings and any stale AT bytes discarded (#237)`.

  **Done.** Mutations: the warning at debug fails the discarded-bytes and after-timeout tests; no `log_aka_timings` call on the refresh path fails both timing tests; logging `select_ms=0` for a card reader fails `a_card_reader_aka_logs_no_select_time`. Commit `fcecec1`. Gate: 3558 passed, 0 failed.

**Checkpoint**: all three stories are complete.

---

## Phase 6: Polish & Cross-Cutting Concerns

- [X] T026 [P] `docs/operations.md`: next to the stall-watchdog section (around `:120`), add a short "Modem AT port sync" subsection, written from `contracts/operator-surface.md`. Cover:
  - what the stale-bytes warning means and when to worry;
  - that an unresponsive modem now fails at open;
  - the 20 s AKA budget and the `SIM challenge timings` debug line, and the `RUST_LOG` that shows it;
  - the fallback-restart behaviour change (D8).
- [X] T027 [P] `RELEASE_NOTES.md`: add one bullet under `## Unreleased`, formatted `- **Summary** -- plain text` (only the summary bold; memory: release notes bold scope). Cover:
  - the reporter-visible fix (a slow SIM no longer forces a full registration on a modem line);
  - the port fix (a late reply from an abandoned command no longer shifts the next session's replies, which caused `SELECT 3F00 failed: SW=6A83`);
  - the bridge's EAP-AKA budget;
  - the two sweep budget changes;
  - the fallback-restart change.

  State what was verified live and what by tests only (filled in from T030). Reference #237. No real identifiers.
- [X] T028 [P] `docs/greptile-review-learnings.md` Part 1, "Concurrency & shared devices": add a checklist item. A new reader of a modem AT port trusts nothing until `Session::sync` has run: a reply abandoned by another holder (or by our own timeout) arrives on the next open, or after a drain, and shifts every reply by one. Never add a second way to open a port, or a drain-only recovery. Cite #237 and `tmp/reopen-test/`.
- [X] T029 Self-review over the whole diff with the CLAUDE.md checklist, and record the answers in a "Self-review answers" section of this file. Specifically:
  - every new error string against `grep -rn 'contains(\|starts_with(' gsm-sip-bridge/src/ims gsm-sip-bridge/src/modules gsm-sip-bridge/src/supervise`;
  - every constant against `grep -rn 'MAX_\|_BUDGET\|_TIMEOUT' gsm-sip-bridge/src/modules gsm-sip-bridge/src/volte/sms.rs`;
  - every blocking wait (the open's sync wait, the AKA wait) and whether `busy()` or the watchdog phase sees it;
  - every `take()`/drop of `tx` in `start_worker` and `try_reopen`, and who releases the port on error;
  - docs claims against the evidence.
- [X] T030 Live verification on the local EC200U, following `quickstart.md` §2, §3, §4 and §6. Build the branch image from `git archive HEAD` (memory: docker build context). Record each result in `quickstart.md` under a "Results (2026-…)" heading:
  - the stale reply discarded and the warning seen;
  - the open overhead median;
  - discovery timings;
  - EAP-AKA through the bridge.

  §5 (a slow SIM) can't be reproduced here. Record it as tests-only plus a request to the reporter (the two sync queries, and the timing line).

  **Status:** §2, §3, §4, §6 and FR-010 verified live, after a SIM was put in the modem. §5 is recorded as tests-only. The #237 failure (`SELECT 3F00 failed: SW=6A83`) was reproduced through the line on `main` and does not happen on the branch.
- [X] T031 Final `make format && make lint && make test` with `CARGO_TARGET_DIR=$PWD/target-161`, and compare with the T001 baseline (more passed, 0 failed). Commit the docs: `docs: modem AT port sync and the AKA budget (#237)`.

---

## Traceability

| Requirement | Tasks |
|---|---|
| FR-001 (sync on every open and reopen) | T008, T011, T015 |
| FR-001a (sync after a timeout in a session) | T004, T006 |
| FR-002 (two queries, ordered, not fooled by one earlier abandoned sync) | T003, T005 |
| FR-003 (bounded by the session timeout; typed failure; no out-of-step session) | T003, T007, T008, T009, T010, T011, T012, T013 |
| FR-004 (≤ two exchanges, no setting written) | T003 (`…exactly_two_queries`), T005 |
| FR-005 / FR-006 (20 s AKA budget, refresh and full registration) | T016, T019 |
| FR-007 (bridge relay) | T018, T020 |
| FR-008 (error text unchanged) | T007, T016 (`…keeps_its_wording`), T019 |
| FR-009 (watchdog derivation) | T014, T021 |
| FR-010 (card reader unchanged) | T017 |
| FR-011 (timing line) | T023, T025 |
| FR-012 (stale-bytes warning) | T022, T024 |
| SC-001 | T003, T004, T008, T030 |
| SC-002 | T016 (tests), quickstart §5 (reporter) |
| SC-003 | T030 |
| SC-004 | T007, existing `sim_recovery` tests |
| SC-005 | T022, T023 |
| SC-006 | T017, existing `pcsc_card` tests |

## Dependencies & Execution Order

- **Setup (T001)** comes first, then **Foundational (T002)**.
- **US1 part A** (T003–T006, commit 1) comes before **US1 part B** (T007–T015, commit 2). Part B's open sync reuses `Session::sync`.
- **US2** (T016–T021, commit 3) depends on T011 (`start_for_test`), used by its tests. Its code does not depend on US1 otherwise.
- **US3** (T022–T025, commit 4) depends on T005/T006 (the sync it logs from) and on T002 (the label).
- **Polish** (T026–T031) comes after all stories. T030 needs the full branch.

Within a phase, the tests come before the implementation. `[P]` tasks touch different files and can run in parallel.

## Parallel Example: User Story 1, part B

```text
T007 error.rs tests        T008 at_commander.rs tests
T009 ims/mod.rs tests      T010 sms.rs + usim_bridge.rs tests
→ then T011 (error + worker + start_worker), T012 (open_modem_waiting), T013 [P] (sms/bridge loops), T014 (budgets), T015 (grep + commit)
```

## Parallel Example: User Story 2

```text
T016 usim.rs tests   T017 usim.rs RecordingTransport test   T018 usim_bridge.rs tests
→ then T019 (usim) → T020 (bridge) → T021 (watchdog + commit)
```

## Implementation Strategy

- **MVP = US1** (commits 1–2). It removes the out-of-step channel, which caused the failed attempt and the counted renewal failure in #237, on every shared port. It ships value alone.
- **US2** (commit 3) removes the full registration itself for a SIM answering in 5–20 s.
- **US3** (commit 4) adds the data to check the 20 s choice and how often the fault happens.
- Then polish and live verification.

## Self-review answers (T029, for the PR)

- **New error strings vs. classifiers.** One new string, `PortOutOfStep`'s `discovery error: AT command timeout: <port> did not answer its sync within <N> ms`. `grep -rn 'contains(\|starts_with(' src/ims src/modules src/supervise`: the readers are `contains("timeout")` (`modules/worker.rs:253`, `:829`, `ims/session.rs:43`; `bin/gsm_echo.rs:154` too), which read it as a timeout, and `has_csim_failure` (`AT+CSIM failed`), which it does not match. `map_registration_error` tests `auth`/`aka`/`challenge` first; the text contains none of them (the label is a device path). Pinned by `port_out_of_step_reads_as_a_timeout_to_every_classifier`. The in-session sync failure reuses `AT command timeout`, and `csim_within` keeps both `AT+CSIM failed: …` and `unexpected +CSIM reply` byte for byte. The stale-bytes warning replaces `discarded a stale AT reply…`, which nothing read (`grep -rn 'discarded a stale'` found only its definition).
- **Constants.** `grep -rn 'MAX_\|_BUDGET\|_TIMEOUT' src/modules src/volte/sms.rs`: `AKA_RESPONSE_BUDGET` reuses `MAX_RESPONSE_LINES` (made `pub(crate)`, not copied) and is the only 20 s budget. The sync bound is the session's own `timeout` (`DEFAULT_TIMEOUT` or `PROBE_TIMEOUT`), the same bound the first command had, and the open's sync wait reuses `WORKER_GRACE` like `dispatch`. `RESYNC_TIMEOUT` (2 s) is gone: a resync now gets the port's timeout, and `try_resync` already waited `timeout + WORKER_GRACE`. Every derived budget that counts the open or an APDU is recomputed from these constants (watchdog renewal 269 s / 360 s; sweep 31.8 s ≤ 35 s, pass 72 s ≤ 90 s with 25%).
- **Blocking waits.** The open's sync runs on the worker; the caller waits `timeout + WORKER_GRACE` on a channel, exactly as for a command. It happens inside each caller's existing open, so it is covered by the phase that caller already runs under: `Phase::Renewal` for `register_session` (counted), `Phase::SmsSweep` for the sweep (counted), the refresh challenge's own `SimWorker` thread (600 s bound), discovery's `run_bounded` (5 s, 2.8 s worst case). `Phase::Startup` (420 s) has no derivation; it adds at most a few 5 s syncs. The AKA wait (20 s per APDU) runs on the challenge thread for a refresh and under `Phase::Renewal` for a full registration (counted). No new wait runs inside the dispatch loop apart from the full registration, which already blocked there.
- **`take()`/drop of `tx`.** `start_worker` drops its `tx` on any sync failure before returning `PortOutOfStep`; the worker finishes its bounded sync, sees the channel closed and drops the port and its flock (`an_open_that_fails_its_sync_releases_the_worker`). `try_reopen` takes the old `tx` first (unchanged); if the new worker's sync fails, `start_worker` drops the new `tx`, `try_reopen` returns `false`, the transport is left with `tx: None`, and `ensure_usable` marks the channel `Dead`, as when the reopen itself fails. A worker wedged in `read(2)` during a sync is stranded exactly as a wedged command is (module doc).
- **Threads and sockets.** No new thread kind, socket or kernel object: the sync runs on the port's existing worker. Test-only: `test_modem` threads exit on EOF.
- **Fields changed.** `Session.desynced` (`grep -n desynced at_worker.rs`): every writer still sets it on error; it is cleared only by a successful sync now. `Session.label` is new, read only by logs and the error detail. `OpenedCard.opened` is new; its two destructures take it and nothing copies it. `BridgeError` has no exhaustive match (`grep -rn 'match .*BridgeError'`).
- **After a transition, the next request of every kind.** Open and reopen: the first command runs on a synced channel. After a timeout: the worker path goes `Suspect` → `Request::Resync` (sync) before the next `Command` or `ReadLine`; the `Direct` path syncs inside `send_command_within`. `read_line_raw` never syncs (it writes nothing, and the URC poll must not write), as before.
- **Docs.** `docs/operations.md`, the learnings item and the release note are checked against the live results in `quickstart.md` (T030) and the tests; the release note says what was live-verified and what by tests only, with only the summary in bold under `## Unreleased`, which the repo's release-note comment allows until release time.

## Deviations (recorded during implementation)

- **The echo of a sync query is not counted as stale** (T003, D2). D2 says echoed command lines are "discarded like anything else", and T003 says "only the two replies count as ours". They are still discarded, but a line equal to the query just sent is not added to `discarded_bytes`. Echo is the power-on default and only the CS pool sends `ATE0`, so counting it would log the stale-bytes warning on every open of an echo-on modem, which contradicts the contract ("frequent `context=open` lines point at another holder"). Blank lines around the sync's own replies are not counted either, so a clean sync reports 0. Pinned by `sync_ignores_command_echo` (0 discarded with echo on).
- **`start_worker` takes no `path`** (T011). The port label for the error detail is read from the session (`Session::label`), so the path parameter would be unused.
- **§6 was verified once a SIM was put in the modem** (T030). Until then the rig's modem had no SIM, and this was recorded as a deviation. It is now verified live, together with the agent-restart reproduction, in `quickstart.md`, "With a SIM in the modem".

## PR review fixes (#238)

- **Reads bounded by their loop's deadline** (Greptile P2, `at_worker.rs`). Each loop checked its deadline, then made a read that could wait the port's whole timeout. A sync whose first reply came just before its bound held the port for up to twice the bound, after `start_worker` had stopped waiting.
  - Fixed for the whole class: `send_command_within`, `read_line_raw` and `read_sync_reply` all read through `Session::fill_by(deadline)`.
  - The port is now an `AtPort`, which can bound its next read: serial ports and `UnixStream` do, and mock streams keep their own timeout through `FixedTimeout`. The port's timeout is put back after a bounded read, because `serialport` uses the same timeout for writes.
  - The sync no longer writes its second query once the deadline has passed.
  - Pinned by four new tests and the restore test. Each fails with its part of the fix removed.
- **Renewal worst case counts every resync** (Greptile P2, `watchdog.rs`).
  - The AKA pairs (`MAX_RESYNC_ATTEMPTS + 1`) and REGISTER round trips (`+ 2`) are now derived. The modem-lock wait, never counted before, is added.
  - Worst case 433 s, so `Phase::Renewal` goes from 360 to 540 s.
  - Same class: `Phase::Startup` ("renewal's work plus PLMN derivation") was 420 s, under the new renewal worst case. It is now 600 s, pinned by `startup_budget_covers_a_registration_after_plmn_derivation`.
