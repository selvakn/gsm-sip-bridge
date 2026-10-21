---

description: "Task list for bounding swanctl --initiate (#139)"
---

# Tasks: Bound the tunnel-initiation work so a silent ePDG cannot freeze the shared IPsec daemon

**Input**: Design documents from `specs/118-bound-swanctl-initiate/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/initiate-command.md, quickstart.md
**Tests**: Included. The constitution is integration-first and SC-007 requires a build-failing regression check.
**Commit rule** (CLAUDE.md): `make format && make lint && make test` green before every commit; one logical change per commit; synthetic data only (documentation addresses such as 192.0.2.0/24, 198.51.100.0/24, and `172.29.139.0/24` for the rig).

## Format: `[ID] [P?] [Story] Description`

All source paths are under `gsm-sip-bridge/src/supervise/` unless stated. Use `CARGO_TARGET_DIR` if another worktree shares the target dir.

---

## Phase 1: Setup

- [X] T001 Confirm a green baseline on branch `118-bound-swanctl-initiate`: run `make test` and note the pass count in the first commit message.
- [X] T002 [P] Smoke-run the committed rig per `quickstart.md` ("Container rig") against the unmodified binary's image and confirm the baseline reproduces (log freezes near the 12th initiate, `list_sas_rc=143`). Record the numbers in `specs/118-bound-swanctl-initiate/live-verification.md`.

---

## Phase 2: Foundational (blocks every story)

- [X] T003 In `engines.rs` add constants `INITIATE_DETACH_SECS = 20`, `INITIATE_HARD_KILL_SECS = 25`, `INITIATE_KILL_GRACE_SECS = 2` and a compile-time assertion that `HARD_KILL + GRACE < STRONGSWAN_REINITIATE_EVERY * ESTABLISH_POLL_INTERVAL` (30s) from `line_supervisor.rs`.
- [X] T004 In `engines.rs` add a pure `fn initiate_argv(shared_conf: &str, conn: &str) -> Vec<String>` producing `timeout -k 2 25 env STRONGSWAN_CONF=<conf> swanctl --initiate --child <conn> --timeout 20` exactly as in `contracts/initiate-command.md`. Not wired in yet.
- [X] T005 In `runner.rs` check `MockCommandRunner` can (a) hand out a tracked `ChildHandle` from `spawn`, (b) report `is_alive` false after a test-controlled "exit", and (c) return an exit code from `wait`. Extend it only if one is missing, and keep the conformance suite against `RealCommandRunner` passing.

**Checkpoint**: nothing user-visible changes yet; `make test` green.

---

## Phase 3: User Story 1 - A silent ePDG leaves the line down but the system alive (P1) 🎯 MVP

**Goal**: every initiate has a finite lifetime and at most one client per line is live.
**Independent test**: the rig run with the fixed binary (`quickstart.md`): `list_sas_rc=0` and the log still advancing at tick 22; unit tests below.

### Tests

- [X] T006 [P] [US1] In `engines.rs` tests: `initiate_argv` satisfies the contract (has `--timeout n` with 0<n<30, wrapped in `timeout -k k t` with n<t and t+k<30, `--child` is this line's connection only).
- [X] T007 [P] [US1] In `engines.rs` tests: with a mock runner, calling `engine.initiate()` twice while the first client is alive spawns exactly one process and logs the skip; after the mock child exits, the next call `wait`s it and spawns a new one.
- [X] T008 [P] [US1] In `engines.rs` tests (real process, no mock): with `RealCommandRunner`, run the wrapper shape against a command that never exits (`timeout -k 1 2 sleep 1000`) and assert it is gone within ~4s and that no entry is left in the runner's table after `wait`.
- [X] T009 [P] [US1] Add a source-scan test (new file `gsm-sip-bridge/tests/test_initiate_bounded.rs`) that fails if the string `"--initiate"` appears in any non-test code under `src/supervise/` outside `initiate_argv` (SC-007). The failure message must say to use `StrongswanEngine::initiate`.

### Implementation

- [X] T010 [US1] In `engines.rs` add `pending_initiate: RefCell<Option<PendingInitiate>>` to `StrongswanEngine` (`PendingInitiate { handle: ChildHandle, started: Instant }`) and update every construction site: `orchestrate.rs` (~line 1653) and the test helpers in `engines.rs` (`strongswan_engine`, `engine_with`, `engine`).
- [X] T011 [US1] In `engines.rs` implement `StrongswanEngine::initiate(&self, runner)` per `data-model.md` (skip while alive; `wait` and log exit when finished; spawn with tracked `runner.spawn(ChildSpec::new(initiate_argv(..)))`; log started/skipped/exited per the contract). Make `reinitiate` call it. Delete the `--initiate` use of `swanctl_background`; keep that helper only if another caller remains, otherwise remove it and its stale review comments.
- [X] T012 [US1] In `orchestrate.rs` replace the startup `runner.spawn_detached(ChildSpec::new(["env", ... "--initiate" ...]))` (~line 1714) with `engine.initiate(runner.as_ref())`, after the engine exists and after `load_all`.
- [X] T013 [US1] Update existing tests that assert the old shape (`engines.rs` ~2245, ~2649-2710: "`--initiate` must be backgrounded", `reinitiate_also_backgrounds_initiate`) to assert the new tracked, bounded spawn instead.
- [X] T014 [US1] Run T006-T009 plus the full suite; commit "bound every swanctl --initiate with a deadline and one client per line (#139)".

**Checkpoint**: US1 delivers the fix for the reported freeze on its own.

---

## Phase 4: User Story 2 - The line recovers by itself when the ePDG comes back (P1)

**Goal**: no fixed retry count, and recovery after an outage of any length.
**Independent test**: rig with the sink replaced by a responder after several minutes; plus the unit test below.

- [X] T015 [P] [US2] In `line_supervisor.rs` tests: `tick_establishing` with `max_establish_attempts() == None` and no tunnel never returns `FatalTimedOut` or `FatalProcessDied` over 10,000 ticks and keeps issuing the bounded initiate on the cadence (using a fake engine).
- [X] T016 [US2] Extend the rig for recovery: add `rig/recover.sh` that, after the loop, swaps the silent sink for a stub that answers IKE_SA_INIT (or removes a drop rule) and shows the daemon's log reaching IKE_SA_INIT response handling without restarting charon; document in `quickstart.md`. Keep identifiers synthetic.
- [X] T017 [US2] Live check (one rig): follow `quickstart.md` "Live" steps 1-3 at 6 and 30 minutes of silence; record in `live-verification.md`.

---

## Phase 5: User Story 3 - One negotiation per line, handed to the daemon while in flight (P2)

**Goal**: the startup loop applies the same half-open rule as steady state.
**Independent test**: fake engine reporting a negotiation in flight; run repeated establish ticks.

### Tests

- [X] T018 [P] [US3] In `line_supervisor.rs` tests: establish ticks across 100 cadences with a fake engine reporting `Connecting` and fresh activity issue zero initiates.
- [X] T019 [P] [US3] Same file: `Connecting` with no activity beyond `HALF_OPEN_STUCK_AFTER` produces exactly one `terminate` + `initiate`; no IKE_SA produces one `initiate` per cadence; established-without-P-CSCF keeps today's terminate-then-initiate.

### Implementation

- [X] T020 [US3] In `line_supervisor.rs` extract the activity-tracking half of `tick_half_open` (fingerprint compare, `active_at`, idle computation) into a shared helper used by both phases, without changing steady-state behaviour; the existing specs/100 tests must pass unmodified.
- [X] T021 [US3] In `line_supervisor.rs` add `EstablishProbe { Unresponsive, NoIkeSa, Connecting { ike_id }, Other }` and a `TunnelEngine::establish_probe(&self, runner)` method (default `Other`). Implement it in `StrongswanEngine` in `engines.rs` with the existing bounded `list_sas`, `list_sas_has_ike_sa` and `ike_sa_header`.
- [X] T022 [US3] Add `EstablishTracking { half_open, vici_strikes }` and thread `&mut EstablishTracking` through `tick_establishing`; on re-initiate ticks apply the decision table in `data-model.md`. Update every caller (`orchestrate.rs` ~1737 and ~3275; find any others with grep) and existing establish-loop tests.
- [X] T023 [US3] Run T018-T019 and the suite; commit "startup loop leaves an in-flight IKE_SA to charon".

---

## Phase 6: User Story 4 - A wedged daemon is detected during startup (P2)

**Goal**: an unresponsive control interface is a defined state with a working recovery.
**Independent test**: mock runner whose `--list-sas` times out; and the rig with the old unbounded behaviour forced.

### Tests

- [X] T024 [P] [US4] In `line_supervisor.rs` tests: one timed-out probe records a strike and does nothing else; two consecutive timeouts call `restart_if_wedged` once and reset the counter; an answered probe resets the counter.
- [X] T025 [P] [US4] In `engines.rs` tests: with two engines sharing one `SharedCharon`, both observing the same wedge generation, only one respawn happens (the second sees the bumped generation and returns false). A live, answering daemon is never restarted.
- [X] T026 [P] [US4] In `engines.rs` tests: `load_all` runs under a `timeout` wrapper.

### Implementation

- [X] T027 [US4] In `engines.rs` wrap `SharedCharon::load_all` in `timeout <STEADY_LIST_SAS_TIMEOUT_SECS>`.
- [X] T028 [US4] In `engines.rs` add `SharedCharon::restart_if_wedged(observed_generation)` per research R3 (generation-guarded; reaps the live charon, respawns via `spawn_locked`), and a `generation()` accessor for callers to capture before probing.
- [X] T029 [US4] In `tick_establishing` count `Unresponsive` probes, log `swanctl --list-sas timed out (k/2)`, and on the second consecutive timeout log the wedge-recovery line and call the engine's new wedge recovery (`TunnelEngine::recover_wedged_control(&self, runner)`, default no-op, `StrongswanEngine` calls `restart_if_wedged` then `load_all` then `initiate`).
- [X] T030 [US4] Make steady-state `SteadyStateHealth::ViciBroken` use the same `recover_wedged_control` after its existing strike handling, so both phases share one recovery (keep its current tests green; add one asserting a live wedged daemon is now respawned).
- [X] T031 [US4] Run T024-T026 and the suite; commit "detect and recover a wedged charon control interface".

---

## Phase 7: User Story 5 - A silent ePDG on one line does not hurt the others (P2)

**Goal**: per-line bounds and no cross-line collateral.
**Independent test**: two engines on one shared charon, one silent.

- [X] T032 [P] [US5] In `engines.rs` or `line_supervisor.rs` tests: two engines sharing a `SharedCharon`; the silent one cycling 100 cadences issues no `terminate`, `load_all`, or restart that touches the healthy engine's connection, and the combined live clients never exceed the number of lines.
- [X] T033 [US5] Add a sizing note to `research.md` R1: with N silent lines at most N workers are held for 20s each; the 16-thread pool minus charon's own leaves headroom for N up to the supported line count. Add a test asserting the supported maximum line count (find it in config) times one stays under `16 - 4`.
- [ ] T034 [US5] Live check (two lines, one silent, 30 min): follow `quickstart.md` "Live" step 4 and record in `live-verification.md`.

---

## Phase 8: User Story 6 - Shutdown during an unreachable-ePDG attempt leaves nothing behind (P3)

**Goal**: no initiate children or waiter threads remain after shutdown or line removal.
**Independent test**: shutdown requested mid-attempt.

### Tests

- [X] T035 [P] [US6] In `engines.rs` tests: `cancel_initiate` reaps the outstanding client (mock: `reap` called once, table empty) and is a no-op when none is pending.
- [ ] T036 [P] [US6] In `orchestrate.rs` tests (or the existing shutdown tests): shutdown requested while the establish loop is waiting calls `cancel_initiate` before the loop returns.

### Implementation

- [X] T037 [US6] In `line_supervisor.rs` add `TunnelEngine::cancel_initiate(&self, runner)` (default no-op) and implement it on `StrongswanEngine` in `engines.rs` (`reap` the pending handle, clear the slot).
- [X] T038 [US6] In `orchestrate.rs` call it on every exit of the strongSwan line's loops: the establish loop's shutdown and fatal breaks (~1721-1752), and the steady loop's shutdown/removal returns (~1905-1935 onward). Prefer one RAII-free helper called at each return over scattered calls.
- [ ] T039 [US6] Live check: request shutdown mid-attempt and confirm `pgrep -f 'swanctl.*--initiate'` is empty; record in `live-verification.md`.

---

## Phase 9: Polish & Cross-Cutting

- [X] T040 [P] Update the doc comments that described the old fire-and-forget behaviour (`runner.rs` `spawn_detached` example, `engines.rs`, `docker/strongswan/charon-extra.conf` note on retries) so none claim `--initiate` is untracked.
- [X] T041 [P] Add a short troubleshooting entry (symptom: charon log stops advancing, `swanctl --list-sas` times out) to the VoWiFi docs where underlay/ePDG failures are described; find the file with `grep -rl "ePDG" docs README.md`.
- [X] T042 Full local verification: `make format && make lint && make test`.
- [X] T043 Finish `live-verification.md` with all runs (T002, T017, T034, T039) and the fixed-binary rig output; confirm no real subscriber data in any file (`git grep` for the repo's placeholder rules in CLAUDE.md).
- [X] T044 Open the PR referencing #139, including the rig numbers (before/after) in the description.

---

## Dependencies & Execution Order

- Phase 1 -> Phase 2 -> **US1 (MVP)**. US1 alone fixes the reported freeze.
- US2 depends on US1 (verification of the fixed behaviour).
- US3 depends on US1 (uses `initiate`). US4 depends on US3 (`EstablishTracking`, `establish_probe`).
- US5 depends on US3 and US4 (it asserts their combined behaviour).
- US6 depends on US1 only; it can run in parallel with US3/US4.
- Polish last.

```text
Setup -> Foundational -> US1 -> US2
                          |--> US3 -> US4 -> US5
                          '--> US6
```

## Parallel opportunities

- T006-T009 (four independent test additions) before T010.
- T018/T019, T024/T025/T026, T032, T035/T036 within their phases.
- US6 (T035-T039) alongside US3/US4 if two people work the branch.
- T040/T041 together.

## Implementation strategy

1. **MVP**: Phases 1-3 only (T001-T014). Ship it; it removes the freeze and bounds clients. Verify with the rig and `quickstart.md`.
2. Add US3 and US4 next: they stop stacked requests and give establishment a working safety net for any other wedge cause.
3. US6 and US5 close the guarantees; US2's live checks run after each stage.

## Notes

- Tasks that touch the same file (`engines.rs`, `line_supervisor.rs`, `orchestrate.rs`) are not marked [P] against each other.
- `swanctl_background` carries review history in its comments; when removing it (T011) keep the lesson that discarding a tracked handle leaks table entries, now enforced by holding and consuming the handle.
- Do not change `keyingtries = 0` or `charon.threads`.
