# Tasks: A Hung Modem No Longer Costs a Modem Line Its Registration

**Input**: `specs/165-modem-open-bounded/` (spec.md, plan.md, research.md F1–F5 and D1–D9, data-model.md, quickstart.md)

**Tests**: requested (constitution I). Write each story's tests first and watch them fail for the right reason. Then check each passing test against a mutation of the code it covers, and record which mutation fails which test in that task's line.

**Paths**: source is `gsm-sip-bridge/src/...`; docs are at the repo root. Build and test with `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_DEV_STRIP=symbols CARGO_BUILD_JOBS=6 PJSUA_SYS_STATIC=1 PKG_CONFIG_PATH=/usr/local/lib/pkgconfig` (memory: test build disk limits).

**Test doubles**:
- The device is an opener closure passed to `open_session_with`. It either blocks on a channel the test holds (released later or never), returns an error, or returns one half of a `UnixStream` pair as the port. The thread, channel, timeout, map and drop are all real.
- Justification: a USB serial device whose `open(2)` never returns (a DTR control transfer the kernel cannot cancel) cannot be produced in CI.

**Never** put a real IMSI, IMEI or MSISDN in code, tests or docs.

**Commits** (each green under `make format`, `make lint`, `make test`):
1. the bounded open, the outstanding-open map and `try_reopen` (US1–US3, with tests);
2. the budget re-derivation (sweep and watchdog);
3. docs, release notes and learnings.

## Phase 1: Setup

- [x] T001 Confirm the branch is `165-modem-open-bounded` on top of `a2ef631`, the tree is clean except `specs/165-modem-open-bounded/`, `.specify/feature.json` and `CLAUDE.md`, and the disk has at least 3 GB free (`df -h /`). Deviation: 2.5 GB free; the builds fitted. Later rebased onto `main` (`5334fa8`), so the two #239 PRs do not depend on each other; part 1's docs are not assumed.

## Phase 2: Foundational: an injectable, bounded open (blocks US1–US3)

- [x] T002 In `gsm-sip-bridge/src/modules/at_commander.rs`, add `fn open_bound(timeout: Duration) -> Duration` (`timeout + WORKER_GRACE`) and `pub(crate) const DEFAULT_OPEN_BOUND: Duration` (7 s, pinned to `open_bound(DEFAULT_TIMEOUT)` by a test) (D2)
- [x] T003 In `gsm-sip-bridge/src/modules/at_commander.rs`, split `open_session` into `open_session_with(path, timeout, opener)`, where `opener: FnOnce() -> Result<Box<dyn AtPort>, String> + Send + 'static`. `open_session` passes the `serialport` open and keeps today's error text `failed to open serial <path>: <e>` (FR-009)

## Phase 3: User Story 1 + 2 — every open completes or fails within its bound (P1) 🎯 MVP

**Goal**: FR-001–FR-004, FR-007–FR-009. **Independent test**: an opener that never returns makes `open_session_with` fail within `open_bound` with `PortOutOfStep`, and the loops do not retry it.

### Tests (write first, `modules/at_commander.rs` tests)

- [x] T004 [US1] `an_open_that_never_returns_fails_within_its_bound`: the opener blocks forever; the call fails with `is_port_out_of_step()`, its text contains `AT command timeout` and `did not open within`, and it takes at least the bound and less than the bound plus 500 ms (short test timeout)
- [x] T005 [US1] `an_open_in_time_is_handed_over_as_today`: the opener returns a `UnixStream` half; the session works (a scripted `AT` → `OK` exchange through `start_worker` with the sync replies)
- [x] T006 [US1] `an_open_error_keeps_its_text`: the opener returns `Err("No such file or directory")`; the error is `Discovery("failed to open serial <path>: No such file or directory")`, not out of step
- [x] T007 [US2] `a_port_that_opens_after_its_caller_left_is_closed`: the opener blocks until the test releases it after the caller timed out, then returns a `UnixStream` half; the test's other half reads EOF (the thread dropped the port)
- [x] T008 [US2] `try_reopen`'s loop needs a real tty, so it was extracted into `reopen_retrying(path, open, sleep)` and tested as `reopen_does_not_retry_a_port_that_is_not_answering` and `reopen_still_waits_out_a_busy_port`

### Implementation

- [x] T009 [US1] Implement D1/D4/D5 in `open_session_with` in `gsm-sip-bridge/src/modules/at_commander.rs`: the outstanding-open check, a thread `at-open-<file name>`, `recv_timeout(open_bound(timeout))`, on timeout take the map's mutex and either `try_recv` the finished result or insert `path → id` and return `PortOutOfStep("<path> did not open within <ms> ms")`. The thread sends its result, drops it if `send` fails, then removes its own entry under the mutex and sets `finished`
- [x] T010 [US2] In `try_reopen` (`gsm-sip-bridge/src/modules/at_commander.rs`), stop on `Err(e) if e.is_port_out_of_step()` with the existing warning (D6)
- [x] T011 [US1] Update the `open_with_timeout` doc (`:228`–`:236`) with one paragraph on the bounded open and the stranded open thread

## Phase 4: User Story 3 — a long hang does not pile up stuck threads (P2)

**Goal**: FR-005, FR-006. **Independent test**: while one open of a port is outstanding, further opens of that port fail at once and start no thread; another port is unaffected; after the outstanding open returns, opens work again.

### Tests (write first)

- [x] T012 [US3] `a_second_open_while_one_is_outstanding_fails_at_once`: after T004's timeout, another open of the same path fails with `is_port_out_of_step()` and `an earlier open of … has not returned`, in under 100 ms, and its opener is never called
- [x] T013 [US3] `an_outstanding_open_does_not_block_another_port`
- [x] T014 [US3] `once_the_outstanding_open_returns_opens_work_again`: release the blocked opener; wait until the entry is gone (poll a test-only `is_outstanding(path)`, bounded); the next open is attempted and succeeds
- [x] T015 [US3] `concurrent_opens_in_time_are_not_marked`: two openers that each take 50 ms, run at once on the same path, both succeed and leave no entry (D4: only a given-up open is marked)

### Implementation

- [x] T016 [US3] Covered by T009's map; `is_outstanding(path)` lives in the test module (it reads the private map), and each test uses its own path. Added `a_panicking_open_leaves_no_record_behind` (the record is removed by a drop guard, so a panicking opener cannot latch a port as outstanding)

## Phase 5: Budgets count the open (FR-011)

- [x] T017 In `gsm-sip-bridge/src/volte/sms.rs`, add `DEFAULT_OPEN_BOUND` to `open_worst` in `the_per_step_budgets_cover_their_worst_legitimate_case` and in `the_listing_step_fits_inside_the_watchdogs_sweep_budget`; raise `PER_MESSAGE_PORT_BUDGET` 35 → 40 s and `SWEEP_PASS_BUDGET` 72 → 80 s and update their docs (`:939`–`:945`)
- [x] T018 In `gsm-sip-bridge/src/ims/agent/watchdog.rs`, raise `Phase::SmsSweep` 90 → 100 s (comment updated); add `DEFAULT_OPEN_BOUND` to `derived_renewal_worst_case` and to the startup derivation (once per open). Renewal (540 s, worst 440 s) and Startup (600 s, worst 477 s) keep their margins and are unchanged. The signal poll's derivation (`a_signal_poll_fits_the_watchdogs_sweep_budget`) counts the open too

## Phase 6: Polish & Cross-Cutting

- [x] T019 [P] `docs/operations.md` stall section: what a hung but attached modem now looks like (`did not open within`, `an earlier open … has not returned`, no restart), and the `at-open-<port>` thread for a kernel stack; update the example report's `budget_secs` to 100
- [x] T020 [P] `docs/observability.md`: the two failure texts under the SMS sweep's existing warning, and the new `SmsSweep` budget wherever 90 s is stated
- [x] T021 [P] `docs/greptile-review-learnings.md` Part 1, Concurrency & shared devices: one line — the device open is bounded too; a new opener goes through `open_session`, never `serialport::new(..).open()` directly
- [x] T022 `RELEASE_NOTES.md` under `## Unreleased`: one bullet with every moved budget (#239). After review it was shortened to the user-facing change, the moved budgets and a link to `docs/operations.md`; the live results are in the PR description
- [x] T023 `grep -rn 'serialport::new' gsm-sip-bridge/src` finds only `open_session` (FR-007); record the output in the PR
- [x] T024 Run `make format`, `make lint`, `make test`; commit per the plan above
- [x] T025 Self-review over the whole diff with plan.md's Blast Radius and the learnings Part 1; write the PR text to `tmp/pr-239-modem-open-bounded.md`

## Dependencies & Execution Order

- T001 → T002 → T003 → (T004–T008 tests) → T009–T011 → (T012–T015) → T016 → T017–T018 → T019–T025.
- T019–T021 are independent of each other.

## Implementation Strategy

MVP is Phases 2–3 (the bound alone stops the watchdog restart, as long as the hang is shorter than the budget minus one bound). Phase 4 makes a long hang safe. Phase 5 keeps the derivation tests honest.

## Deviations

- **D3 revised** (found in self-review, T025): the open's failures are a new `BridgeError::PortOpenStalled` that `is_port_out_of_step()` also recognises, not `PortOutOfStep` itself, so discovery's AT and SIM probes keep counting a hung open as a probe timeout toward quarantine (specs/030). Tests: `error.rs::a_stalled_open_is_out_of_step_and_reads_the_same`, `discovery/probe.rs::a_stalled_open_counts_as_a_probe_timeout`, `discovery/sim.rs::a_stalled_open_is_a_sim_probe_timeout`.
- **`DEFAULT_OPEN_BOUND` is `#[cfg(test)]`**: only the budget derivations read it.
- **Concurrent opens at the moment of the hang** (found in self-review): opens already under way when the port hangs each leave their own entry; no new open starts after the first. FR-005, SC-004 and data-model.md state this bound; test `opens_already_in_flight_when_a_port_hangs_are_the_only_ones_left_behind`.
- **One commit for code and docs** instead of three: the bounded open cannot land without the budget changes (the derivations read `DEFAULT_OPEN_BOUND`), and the docs describe that one change.

## Mutation record

- M2, the timed-out open is not recorded (`opens.push(attempt)` removed): fails `an_open_that_never_returns_fails_within_its_bound`, `a_second_open_while_one_is_outstanding_fails_at_once`, `a_port_that_opens_after_its_caller_left_is_closed`, `a_panicking_open_leaves_no_record_behind`.
- M5, `reopen_retrying` without its `PortOutOfStep` arm: fails `reopen_does_not_retry_a_port_that_is_not_answering`.
- M3, the finishing thread does not remove its record: fails `a_port_that_opens_after_its_caller_left_is_closed`, `a_panicking_open_leaves_no_record_behind`, `once_the_outstanding_open_returns_opens_work_again`.

## Live verification (2026-11-28, `my-fixes` at `0fef8d6`, quickstart §3–§4)

The modem's USB/IP connection was dropped at the router in both directions, with the device left attached and no challenged refresh due. Line 0 had registered at 14:58:43.

| Run | Drop | Open gave up | Repeats of `an earlier open … has not returned` | First clean pass | Agent restarts / full registrations / watchdog reports |
|---|---|---|---|---|---|
| 1 | 90 s | 15:00:45.9 (7.0 s after the open began) | 4 (15:01:05–15:02:05) | 15:02:25 | 0 / 0 / 0 |
| 2 | 180 s | 15:05:28.9 (7.0 s after) | 7 (15:05:48–15:07:48) | 15:08:08 | 0 / 0 / 0 |
| 3 | about 90 s | 15:11:35.9 (7.0 s after) | 4 (15:11:55–15:12:55) | 15:13:15 | 0 / 0 / 0 |

- Before this feature, the same 90 s drop (14:07–14:09) ended with the watchdog stopping the agent after 94 s and a full registration at 14:09:26.
- During each drop the agent had exactly one `at-open-ttyUSB0` thread; afterwards none.
- Kernel stack of `at-open-ttyUSB0` during run 2: `tty_lock_interruptible ← tty_open ← … ← __x64_sys_openat`, waiting for the tty lock; the holder was not captured.
- Run 3 (every thread's stack taken): `at-open-ttyUSB0` in `usb_kill_urb ← usb_control_msg ← usb_wwan_send_setup ← usb_wwan_dtr_rts ← tty_port_block_til_ready ← tty_port_open ← tty_open`: raising DTR on open, a USB control transfer that cannot be cancelled on the silent link. No `at-port-*` worker existed, and no other thread was in the driver (research F1).
- **Deviation from SC-001**: two runs at 90 s, but one at 180 s instead of two.
