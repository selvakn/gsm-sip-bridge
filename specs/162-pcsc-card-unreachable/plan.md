# Implementation Plan: A Card-Reader Line Rides Out an Unreachable SIM

**Branch**: `162-pcsc-card-unreachable` | **Date**: 2026-11-24 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `specs/162-pcsc-card-unreachable/spec.md` (issue #233, the resilience half of #232)

## Summary

- **Typed failure (D1)**: a new `BridgeError::CardUnreachable`, built only in `PcscTransport` through one `pcsc_failure` helper, and recognised with `is_card_unreachable()`. Its text keeps every existing classifier's verdict.
- **Retry the challenge (D2, D3)**: on a card-reader line the SIM worker puts the same challenge to the card again for 30 s. The loop abandons it at 45 s. Modem lines run one attempt with the 600 s timeout, as today.
- **Keep the registration (D4, D5)**: when the window ends, `LoopState.awaiting_card` holds the line on its still-valid registration and checks the card locally on a 5 → 60 s backoff. When the card answers it sends one in-place REGISTER. It registers in full only at the 60 s margin or a lapse, and only with no call in progress. One `force_renewal || refresh_fallback` check clears it for every other path.
- **No exit for a reader outage (D6)**: a full registration that fails with `CardUnreachable` on a card-reader line steps the backoff, but neither counts toward the budget nor marks the P-CSCF.
- **Status (D7)**: `last_refresh: awaiting-card`, plus five log lines. No new metric or alert.

## Technical Context

**Language/Version**: Rust (workspace toolchain, `rust-toolchain.toml`)  
**Primary Dependencies**: `pcsc` 2.9.0 (its `Error` enum is mapped, not re-exported). No new dependencies.  
**Storage**: N/A (in-memory agent state only)  
**Testing**: `make test` (nextest). Deterministic agent tests in `ims/agent/registration_tests.rs` with the scripted SIM and `CardTiming::fast()`. Hardware-gated `#[ignore]` tests in `modules/pcsc_card.rs::hardware` on the reader host.  
**Target Platform**: Linux (container and native), pcsc-lite 2.4.x  
**Project Type**: single binary crate in a workspace (`gsm-sip-bridge/`)  
**Performance Goals**: the dispatch loop never blocks on the card for this feature (worker threads only). A card that is back is noticed within 60 s.  
**Constraints**:
- Modem lines are unchanged in behaviour and text.
- No `unsafe`.
- No new operator configuration.
- The existing classifiers' verdicts are unchanged (FR-003).

**Scale/Scope**: 1 error variant, 1 helper and 7 construction sites; 1 worker generalisation; 1 new `LoopState` field with its driver; 1 extracted failure handler. About 400 lines of code plus about 25 tests.

## Constitution Check

| Principle | Status |
|---|---|
| I. Integration-first testing | Pass, with justification. The agent tests drive the real `LoopState` refresh code against a real UDP peer (the existing rig). Only the SIM is scripted, and only `on_full_renewal_failure` is called directly, because `attempt_renewal` needs a SIM CI does not have. Justified at the mock sites. The real pcsc-lite mapping is checked by hardware-gated tests on the reader host. |
| II. Green-on-commit | Pass: `make format`, `make lint` and `make test` before every commit (CLAUDE.md). |
| III. Atomic commits | Pass: five commits, with the worker refactor kept apart from behaviour changes (see Project Structure). |
| IV. Makefile-driven | Pass: no new targets. Hardware tests use the documented `cargo test -- --ignored` invocation, as #234's do. |
| V. Simplicity | Pass. One `Option<AwaitingCard>` instead of scattered flags. `Reauthentication` is generalised instead of duplicated. Existing constants and `next_backoff` are reused (research, Constants). |

Post-design re-check: unchanged, pass.

## Blast Radius

### State and invariants inventory

Grep output (2026-11-24, `gsm-sip-bridge/src`, test files excluded):

```
$ grep -rn 'challenged' src
ims/agent/registration_refresh.rs:731,1179,1181,1210,1213,1214,1218,1225,1437  ims/agent/mod.rs:2154,2217,4325
  (sip/server/mod.rs:537 is an unrelated comment)
$ grep -rn 'force_renewal' src
ims/agent/registration_refresh.rs:694,913,965,979,1052,1061,1440  ims/agent/ping.rs:220,236,306
ims/agent/reg_subscription.rs:366,379,713,761,881,883 (passed in as a read-only argument)
ims/agent/mod.rs:2122,2215,2365,2539,3996,4018,4026,4252,4272,4300,4407,4476,4501 (+ tests)
$ grep -rn 'refresh_fallback' src
ims/agent/registration_refresh.rs:694,1042,1048  ims/agent/mod.rs:2165,2221,2313,2879
$ grep -rn 'last_refresh' src
ims/agent/registration_refresh.rs:971,1012,1439  ims/agent/mod.rs:2170,2223,2881  vowifi/mod.rs:2785,2800,2801 (+ tests 2900-2920)
$ grep -rn 'reauth_env' src
ims/agent/registration_refresh.rs:1183,1346  ims/agent/mod.rs:2159,2219  ims/agent/registration_tests.rs:1196,1439
$ grep -rn 'WORK_TIMEOUT\|\.overdue(' src/ims/registration_auth.rs src/ims/agent/registration_refresh.rs
ims/registration_auth.rs:25,95,96,384,385  ims/agent/registration_refresh.rs:1214
$ grep -rn 'Reauthentication' src
ims/registration_auth.rs:54,60,106,241..383 (tests)  ims/agent/registration_refresh.rs:18,1112,1184
$ grep -rn 'consecutive_renewal_failures\|count_renewal_failure\|schedule_renewal_retry' src
ims/agent/mod.rs:2128,2228,2305,2310,2376,2398,2399,2421,2457,2483-2496,2518,2527,2540,2543,2565,4378,4413 (+ tests)
$ grep -rn 'map_err(|e| BridgeError::Ims(format!("PC/SC' src
modules/pcsc_card.rs:112,117,220,234,263  modules/pcsc_list.rs:115,120
$ grep -rn 'drive_registration_refresh\|begin_pass(' src
ims/agent/mod.rs:2756 (loop)  ims/agent/registration_refresh.rs:643 (RingRefresh::tick), 716 (begin_pass)
$ grep -rn 'Err(failure) =>' src/ims/agent/mod.rs
2680 (register_via_pool: startup, out of scope)  4484 (full renewal in on_idle_tick)
```

| Changed item | Readers | Copies made at construction | Invariant comments | Decision |
|---|---|---|---|---|
| New `BridgeError::CardUnreachable` | every `?` from `PcscTransport`. Text readers: `map_registration_error` (`session.rs:39`), `classify_register_failure` (`ims/mod.rs:244`), `last_failure` in status (`mod.rs:4495`, `registration_refresh.rs:1022`) | `RegisterFailure.error` (`session.rs:342`) moves it intact | `error.rs:21-30` (why variants exist, not strings): consistent | new variant. Text keeps both classifiers' verdicts (research D1), pinned by a test. |
| 5 `PC/SC … failed` `map_err` sites + 2 final `connect` errors (`pcsc_card.rs`) | as above | none | `pcsc_card.rs:84-94`, `:205-212`: still hold | route through `pcsc_failure`. Inner wording unchanged. `pcsc_list.rs:115,120` deliberately left alone (not a registration path). |
| `Reauthentication` becomes `SimWorker<Answer>` with `done` and a per-worker `deadline` | `drive_challenge` (`:1213-1214`), `Challenged.work` | none | `registration_auth.rs:22-25` (`WORK_TIMEOUT` "generous: a modem…"): still true for modems; doc extended for readers. `:100-101` ("a thread already inside the SIM finishes that call"): still holds, and FR-015 now relies on it. | `spawn` gains `timing: Option<CardTiming>`. `None` is byte-identical behaviour (one attempt, 600 s). |
| `answer()` loops on `CardUnreachable` (reader lines) | worker only | `ChallengeJob` cloned once into the worker | `registration_auth.rs:4-9` ("the thread does nothing but the SIM"): still holds (it sleeps and logs) | `open: FnOnce` becomes `Fn`. Parse once, retry the open+AKA. |
| `drive_challenge` error / overdue branches | — | — | `:1201-1202` "gives up on one that takes too long": extended | reader line + `CardUnreachable` or overdue goes to `await_card`. Everything else goes to `fall_back_to_full` as today. |
| New `LoopState.awaiting_card` | `drive_registration_refresh` (new branch after `challenged`), the full-renewal block (`mod.rs:4324-4326`) | none (no status snapshot copies it; `last_refresh` is set separately) | `mod.rs:4302-4305` ("An ordinary refresh … is never held for a call"): still true | cleared by the step-1 check (every `force_renewal` setter: `mod.rs:2365,2539,4026`, `ping.rs:306`, `registration_refresh.rs:913,965,1052,1440`), at the margin/lapse, when the card is back, in the full-renewal block, and on drop |
| `ReauthEnv` gains `timing` | `begin_challenge`, `drive_awaiting_card` | `ReauthEnv` is `Clone`; one per `LoopState` | `:1077-1081` ("the tests substitute a SIM…"): extended to timing | `Default` = production constants |
| Full-renewal `Err` tail extracted to `on_full_renewal_failure` | `on_idle_tick` only | none | `mod.rs:2273-2290` (why a failing renewal must eventually exit): **amended**. Exit buys `AT+CFUN`, which a reader cannot use, so reader card outages are exempt. | `CardUnreachable` on a reader line goes to `schedule_card_retry` (ladder steps, no count, no mark). Else `on_renewal_failure`, unchanged. |
| `last_refresh` value `awaiting-card` | `sync_shared_status` (`mod.rs:2881`) → `format_refresh_lines` (`vowifi/mod.rs:2800`) | `RefreshSnapshot.last` cloned each poll | none | free string, rendered verbatim. Docs list updated. |

**Implicit contracts**:

- **Error text**: `grep -rn 'contains(\|starts_with(' src/ims src/vowifi src/supervise` found `classify_register_failure`, `map_registration_error` and `refresh_not_carried`'s `contains("Timer F")` (`:818`, `:1016`). None matches the new prefix or the pcsc texts (research F9, D1). `sim_recovery.rs:121` (`AT+CSIM failed`) is modem-only. `runner.rs:993` is a test of its own string.
- **Limits**:
  - `MAX_RESYNCS` (2) is unchanged and is still checked in `drive_challenge` on `Answer::Resynchronise`. A retry that yields AUTS spends one, as any AUTS does.
  - `MAX_CONSECUTIVE_RENEWAL_FAILURES` (6) is unchanged for every failure but the exempt one.
  - `WORK_TIMEOUT` is unchanged for modems.
  - The check backoff reuses `RETRY_INITIAL_BACKOFF` and `next_backoff`. The margin reuses `REFRESH_FLOOR`.
- **Ordering**: the waiting branch runs after `drive_retired` and the `challenged` branch, and before the pending-refresh poll and the due check, so nothing can start a REGISTER while waiting. `begin_pass` still drains held messages first.
- **Startup**: `register_via_pool` (`mod.rs:2640`) is unchanged. A card outage at agent start still exits (out of scope). Its `cause_of` still returns `None` for the typed error, because the text is unchanged in the words it matches.

### Concurrency and ownership map

- **Challenge worker thread** (`SimWorker<Answer>`):
  - Owned by `Challenged.work`, then dropped (cancelled) when the challenge ends.
  - On a reader line it may run up to `retry_window`, sleeping `retry_interval` between attempts and checking `cancelled` before each one.
  - Each attempt's `PcscTransport` (a `SharedCard`) drops at the end of the attempt, leaving the card (#234), so a failed attempt never leaks a handle.
  - On entering the waiting state, its `done` flag moves to `AwaitingCard.previous`. The thread is never joined.
- **Check worker thread** (`SimWorker<()>`):
  - Owned by `AwaitingCard.check`.
  - Opens the card and drops it at once (LeaveCard).
  - Abandoned at `attempt_bound`: `cancel()`, and its `done` flag goes to `previous`.
  - When the state clears with a check in flight, the `SimWorker` drops and cancels, and the thread ends on its own.
- **No new sockets or kernel objects.** Entering the waiting state drops `Challenged.carry.reserved`, the reserved client port, as `fall_back_to_full` does today. Nothing has been staged yet: `stage_replacement` runs only after `Answer::Authenticated`.
- **Error after disarm**: none. No guard is disarmed early. `done` is set by a drop guard inside the thread, so a panic still sets it.
- **Shutdown**: nothing joins a SIM worker. `Drop for SimWorker` only sets `cancelled`.
- **The full renewal while a hung worker exists**: a full registration (synchronous) may queue in pcscd behind a hung abandoned worker. It is bounded by the existing renewal watchdog (research D3, F10), as a hung card was before.

### Where else does this logic run?

- **Refresh drivers**: the loop (`begin_pass`) and the ring wait (`RingRefresh::tick`). Both call `drive_registration_refresh`, which owns the new branch. No copy. Origination is non-blocking state, so it also goes through the loop pass.
- **`busy()`**: the margin rule reads `self.busy()` (active call, origination or ringing). No new predicate.
- **UDP and TCP**: the waiting state sends nothing. The fresh refresh is the ordinary `PendingRegister::start`, which carries the current Via, Security-Verify and connection like any refresh. No state transition happens while waiting, so in-flight BYE/UPDATE/CANCEL/PRACK/SUBSCRIBE are unaffected: the session, connection and SAs are untouched.
- **Other lines**: per-line agents. `CardUnreachable` is per open. One line's reader outage does not affect another's state.
- **Other callers of `PcscTransport`**: `vowifi-plmn` and the agent's start-up PLMN read get the new error prefix on unreachable. They do not branch on it.

### Situations checklist

| Situation | Answer |
|---|---|
| idle | margin or lapse forces full; card back sends an in-place refresh |
| in a call / ringing / origination pending | retries and checks run off-loop; the margin and lapse never force full while `busy()`; card back refreshes in place even after a lapse (existing busy rule) |
| UDP and TCP | no change; no REGISTER while waiting |
| error and rollback | a card error, not unreachable, goes to `fall_back_to_full` as today; a staged swap only exists after `Authenticated`, which never enters the waiting state |
| cancel | `SimWorker` drop sets `cancelled`; results are discarded; at most one unfinished worker (FR-015) |
| shutdown | no joins; threads end on their own |
| limits / off-by-one | the worker starts a new attempt only if `now + interval < window_end`; loop deadline = window + bound; `remaining < REFRESH_FLOOR` (strict); lapse is `now >= expires_at`, the same comparison as `in_place_refresh_possible` |
| blocks the dispatch loop | no. Polls only. The full renewal's synchronous card access is unchanged. |
| docs / release notes / repo rules | `docs/observability.md` value list; `docs/omnikey-pcsc-vowifi.md` troubleshooting entry; learnings Part 1 line (match `is_card_unreachable()`, never PC/SC text); `## Unreleased` bullet per `.cursor/rules/release-notes.mdc`; synthetic IMSI `001010000000001` only |

## Project Structure

### Documentation (this feature)

```text
specs/162-pcsc-card-unreachable/
├── spec.md
├── plan.md                    # this file
├── research.md                # facts F1–F12, decisions D1–D9, constants
├── data-model.md              # CardUnreachable, CardTiming, SimWorker, AwaitingCard + state machine
├── quickstart.md              # automated, hardware-gated, live steps
├── contracts/status-and-logs.md
├── checklists/requirements.md
└── tasks.md                   # /speckit-tasks
```

### Source Code (repository root)

```text
gsm-sip-bridge/src/
├── error.rs                         # CardUnreachable variant, is_card_unreachable()
├── modules/pcsc_card.rs             # pcsc_failure() mapping; 7 sites; mapping + classifier tests; hardware tests
├── ims/registration_auth.rs         # CardTiming + constants; SimWorker<T>; Reauthentication retry loop; CardCheck;
│                                    # scripted::card_after
├── ims/agent/registration_refresh.rs# ReauthEnv.timing; begin_challenge timing; drive_challenge → await_card;
│                                    # AwaitingCard + drive_awaiting_card; branch in drive_registration_refresh
├── ims/agent/mod.rs                 # LoopState.awaiting_card; full-renewal clear; on_full_renewal_failure;
│                                    # schedule_card_retry; budget doc amended
├── ims/agent/registration_tests.rs  # card_reader_* scenarios (US1–US3, FR-014, FR-015)
└── vowifi/mod.rs                    # format test for awaiting-card
docs/observability.md, docs/omnikey-pcsc-vowifi.md, docs/greptile-review-learnings.md, RELEASE_NOTES.md
```

**Commits** (each green on its own):

1. `feat(pcsc): report an unreachable card as a typed error`
2. `refactor(ims): generalise the SIM worker and give it a per-worker deadline` (no behaviour change)
3. `fix(ims): ride out an unreachable card-reader SIM during a challenged refresh`: the retry window and the waiting state together. The retry alone would still fall back to full when its window runs out, so the two are one behaviour change.
4. `fix(ims): do not spend the restart budget on an unreachable card reader`
5. `docs: card-reader outage behaviour, status value and release note`

**Structure Decision**: existing single-crate layout. The change stays in the PC/SC transport, the SIM worker, the refresh state machine and the renewal-failure tail.

## Complexity Tracking

No violations.
