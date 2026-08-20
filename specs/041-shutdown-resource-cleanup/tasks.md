---

description: "Task list for 041-shutdown-resource-cleanup"
---

# Tasks: Complete release of per-line kernel resources on stop

**Input**: Design documents from `/specs/041-shutdown-resource-cleanup/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: Included and non-optional — Constitution I makes integration testing
NON-NEGOTIABLE. The split is deliberate: the *plan* (steps, order, bounds, partition) is
unit-tested because it is a pure function; the *outcomes* (an `if_id` actually released)
are live-measured, because no mock can prove them (plan.md, Constitution Check I).

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story the task serves

## Path Conventions

Single Rust workspace. Supervisor source under `gsm-sip-bridge/src/supervise/`; teardown
tests live in-module (`#[cfg(test)] mod tests` in `shutdown.rs`), not under `tests/`.

## Commit discipline (Constitution II & III)

Every task group ends with `make format && make lint && make test` green, then one
focused commit. No commit may leave the tree red.

---

## Phase 1: Setup & gating experiments

**Purpose**: establish the baseline and settle the two things that cannot be settled by
reading code (research.md R2, R6). T003 can invalidate the whole design — it runs before
any implementation, deliberately.

- [x] T001 Confirm the worktree builds clean before any change: `make format && make lint && make test`. Record the baseline so later failures are attributable.
- [ ] T002 **[GATE]** Run quickstart A1 on a live host: capture SC-000 (restart after a 3-minute stop) and the immediate-restart numbers, per line — seconds to registered, `restarting in 5s` count, IKE_SA setups. Record both in `specs/041-shutdown-resource-cleanup/research.md` under a new "R9. Measured baselines" section. If the gap between them no longer reproduces, STOP and re-scope.
- [ ] T003 **[GATE]** Run quickstart A2, the discriminating experiment: four stops, each with one manual intervention, recording when the `if_id` frees. Append the results table to research.md R2, replacing the "residual uncertainty" paragraph with what was measured. **If run 4 still takes ~150s the mechanism is wrong — STOP and bring the premise back for review before writing any code.**
- [ ] T004 [P] Run quickstart A3: confirm the image's busybox `timeout` accepts `timeout SECS PROG`. Record the confirmed form in research.md R3; if it wants `-t`, note it there so T009 emits the right argv.
- [ ] T005 [P] Run quickstart A4: prove `/var/run/netns` bind-mount propagation host↔container. Record in research.md R6. A failure here drops Phase 5 (US2) only — say so explicitly rather than shipping a bind mount that does nothing.

**Checkpoint**: the design's premise is measured, not assumed. Phases 2-4 proceed on T003
passing; Phase 5 additionally requires T005.

> **Implementation note (2026-08-20):** T002-T005 need a live deployment host with
> privileged Docker/root access to real VoWiFi namespaces, which was not available in the
> environment that wrote Phases 2-5's code — and running them would mean restarting the
> actual production line on the host that happened to be reachable, which was not this
> session's call to make unprompted. The code was written on the strength of research.md
> R1's mechanism (destroying the netdev is the only thing that releases the `if_id` —
> this is standard Linux XFRM-interface/netns behaviour, not a guess specific to this
> deployment) — but **T002-T005 are not yet run, and this is not equivalent to having run
> them.** Before this branch is trusted or deployed:
> - Run T002-T005 for real.
> - If T003 confirms the mechanism (run 4 frees the id in seconds), the Phase 3-5 code
>   below should need no changes — re-run `make test` and proceed to the Phase 3/4/5 live
>   checkpoints.
> - If T003 contradicts it, treat every "Checkpoint: live-verify ..." line below as
>   unmet and bring the premise back for review before relying on any of this code.
> T004's argv form (`timeout SECS PROG`) and T005's mount propagation are assumed in the
> code as written (see T009, T028) and called out at each site; both are one-line checks.

---

## Phase 2: Foundational (blocking prerequisites)

**Purpose**: the record and the step vocabulary every story needs. No behavioural change
lands in this phase — the plan builds exactly the steps it builds today.

- [x] T006 **[DEVIATED from data-model.md]** Implemented as two structs, not one merged `StartedLine`: `StartedVowifiLine { index, strongswan: Option<StrongswanTeardownInfo{conn_name, tun_iface, if_id}>, netns, veth_host }` (new) and `StartedVolteLine` (existing, gains `veth_host: Option<String>`). Reason: VoWiFi's and VoLTE's fields diverge enough (if_id/tun_iface/conn_name have no VoLTE analogue) that a single struct would be mostly `None`s for one bearer or the other, for no benefit over two small structs sharing the same `TeardownStep` vocabulary and the same builder logic in `build_shutdown_plan` — which is what FR-018 actually requires (the guarantees hold by construction), not struct identity. `StartedState` holds both `vowifi_lines: Vec<StartedVowifiLine>` and `volte_lines: Vec<StartedVolteLine>`; `started_netns` unchanged (FR-007). data-model.md updated to match.
- [x] T007 Update `gsm-sip-bridge/src/supervise/orchestrate_volte.rs` to record each VoLTE line as a `StartedLine` — including its veth ends from `ensure_volte_line_veth` (`:359`) and its existing `volte-cleanup` argv as `cleanup_argv`.
- [x] T008 Update `gsm-sip-bridge/src/supervise/orchestrate.rs` to record each VoWiFi line as a `StartedLine` at the point `started_netns.push` occupies today (`:1184` strongswan, `:1798` swu), under the existing `shutting_down` read-guard. Carry `veth_sip`/`veth_ims` down from `start_line_tail` so the record is complete before anything can fail.
- [x] T009 Add the `TerminateIke`, `DeleteLink` and `FlushXfrm` variants to `TeardownStep` in `shutdown.rs`, each carrying its own `timeout_secs` where it can block, plus their `execute` arms. Bounds are argv-level (`timeout N ...`, research.md R3) — no `CommandRunner` trait change.
- [x] T010 Port the existing VoLTE ordering tests in `shutdown.rs` to the new `StartedLine` representation, asserting the **same relative order** they assert today. This is the review gate named in contracts C5: a rewrite that relaxes an assertion is indistinguishable from a passing test.

**Checkpoint**: `make test` green with no observable change to what any stop does.

---

## Phase 3: User Story 1 — A restart costs seconds, not minutes (P1) 🎯 MVP

**Goal**: the stop gives back everything the run took, in dependency order, so the next
start finds nothing in its way.

**Independent Test**: restart with all lines registered; every line reaches
call-answering within 10s of the SC-000 baseline, with no "already claimed" report.

- [x] T011 [US1] Emit `TerminateIke` for every strongswan-engine line before charon's `KillChild`, scoped to that line's `conn_name` (never the bare `ims`), in `build_shutdown_plan` — invariant O-1. Reuse `StrongswanEngine::terminate`'s argv shape (`engines.rs:375`); emit nothing for a swu line (`engines.rs:593`).
- [x] T012 [US1] Emit `WaitForExit` for every VoWiFi child after its `KillChild`, with escalation to `Signal::Kill` for anything still alive at the bound — invariants O-2, O-3, FR-002. Today no VoWiFi child has a wait at all (`shutdown.rs:125-130`).
- [x] T013 [US1] Emit `FlushXfrm` carrying this run's `if_id` set, after every `TerminateIke` and charon's exit — invariant O-4. Reuse `classify_xfrm_dump` and its all-ours-or-nothing rule unchanged (`epdg_iface.rs:41`), including the half-failed-inventory veto (FR-011).
- [x] T014 [US1] Emit `DeleteLink` for each line's tun interface (in its netns) and each line's host-side veth end (`netns: None`), after `FlushXfrm` and before that line's `DeleteNetns` — invariants O-5, O-6, O-11. Both bearers get the veth delete; only VoWiFi has a tun (FR-005, FR-018).
- [x] T015 [US1] Express VoLTE's in-namespace `volte-cleanup` through `cleanup_argv` in the shared builder rather than a VoLTE-specific branch (FR-018), preserving its observable position between the line's `WaitForExit` and its `DeleteNetns` — invariant O-9.
- [x] T016 [US1] Add ordering tests in `shutdown.rs` for O-1 through O-7, O-9 and O-11, as position assertions over `build_shutdown_plan`'s output. One test per invariant, named for the invariant.
- [x] T017 [US1] Add tests: every blocking step carries a non-zero `timeout_secs` (O-8, FR-009); a namespace with no `StartedLine` still gets a `DeleteNetns` (O-7, FR-007); building and executing the same plan twice yields no error and no extra steps (FR-008).
- [x] T018 [US1] Add a `STOP_ALLOWANCE` constant to `shutdown.rs`, documented as "must match `stop_grace_period` in docker/docker-compose.yml", sized per research.md R8 (60s for 4 lines).
- [x] T019 [P] [US1] Set `stop_grace_period: 60s` on the bridge service in `docker/docker-compose.yml`, and in `docker/docker-compose.cellular-internet.yml` if it starts that service.
- [x] T020 [US1] Add a contract test asserting the compose file declares a `stop_grace_period` at least equal to `STOP_ALLOWANCE`, so the two cannot drift — same pattern as `tests/test_config_docs.rs`.

**Checkpoint**: live-verify SC-001 through SC-006 and SC-008 per quickstart Phase C.
This slice alone resolves the original complaint.

---

## Phase 4: User Story 3 — A teardown that cannot finish says so (P3)

**Goal**: the teardown reports what it did, and running out of allowance costs the waits
rather than the deletes.

**Independent Test**: park a process in a line's namespace, stop the container; both
identifiers are still released before the allowance expires, and the report names what
was skipped and what could not be released.

> Ordered before US2 deliberately: US2's reclamation reuses `TeardownOutcome` for its own
> failure reporting (FR-012), and the budget protects the slice that already shipped.

- [ ] T021 [US3] Add `TeardownOutcome` to `shutdown.rs` — per-step result, resources not released, steps abandoned — and return it from `execute_shutdown_plan` instead of discarding every result. Best-effort execution is unchanged: a failed step never stops the rest.
- [ ] T022 [US3] Render the report at the call site in `orchestrate.rs:892`, using the exact markers in contracts C2. Per FR-020 this is the whole escalation: no critical alert, no change to the exit code.
- [ ] T023 [US3] Add `TeardownBudget` (deadline + reserve) and the abandonable/release partition per data-model.md. The budget lives in the executor; the partition is a property of the plan, so O-10 is assertable without a clock.
- [ ] T024 [US3] Implement the fallback: before each abandonable step, if `now + reserve >= deadline`, skip the remaining abandonable steps, go straight to the release steps, and record why (FR-019).
- [ ] T025 [US3] Add tests: O-10 (dropping every abandonable step still releases every device and namespace); the fallback fires on an exhausted budget and skips waits rather than deletes; the outcome reports abandoned steps distinctly from failed ones.
- [ ] T026 [P] [US3] Update the `could not create <tun> (xfrm if_id <id>)` message in `epdg_iface.rs:250-269`: keep the wording, drop the "it clears itself, waiting is the whole remedy" advice, which stops being true (contracts C2).
- [ ] T027 [US3] Rewrite the "a line re-establishes its tunnel every ~30 seconds" section of `docs/operations.md:296-395` (FR-017): what stop now does, what an operator should see, what to check when a resource is reported unreleasable, and the corrected note that `if_id` refusal is still normal while the deployment is *running*.

**Checkpoint**: live-verify SC-009 and SC-010 per quickstart Phase C, including the fault
injection.

---

## Phase 5: User Story 2 — A crashed container can still be cleaned up (P2)

**Goal**: a run that never got to tear down is cleaned up by the next one, and its
leftovers are visible to an operator on the host.

**Independent Test**: `docker kill` with no grace period, start again; lines come up
within 30s of the SC-000 baseline and nothing from the killed run remains.

**Blocked on T005** — if propagation cannot be made to work, this phase is dropped and
only SC-007 is lost (plan.md, Delivery slices).

- [ ] T028 [US2] Add the `- /var/run/netns:/var/run/netns:rshared` bind mount to the bridge service in `docker/docker-compose.yml` (and the cellular-internet compose if applicable), with a comment recording what it buys and the single-instance constraint (research.md R7).
- [ ] T029 [US2] Add `reclaim_previous_run` to `gsm-sip-bridge/src/supervise/epdg_iface.rs`: enumerate host namespaces matching `ims<N>` / `volte<N>`, skip any this run created, and release each through the **same** step builder used at stop (FR-008, FR-014).
- [ ] T030 [US2] Call it once from `orchestrate.rs` before any line setup, alongside the existing `reclaim_stale_xfrm`, and report its outcome with the C2 markers.
- [ ] T031 [US2] Add tests: a clean host emits no steps and adds no latency (FR/SC-008); a namespace this run created is left alone (FR-016); foreign XFRM state is still left untouched with the existing message (FR-015); a leftover namespace yields exactly the stop-path steps for that line.

**Checkpoint**: live-verify SC-007 per quickstart Phase C.

---

## Phase 6: Polish & cross-cutting

- [ ] T032 [P] Add a `RELEASE_NOTES.md` entry at the house length (see the 039 entry, trimmed in commit b90c647 for exactly this reason).
- [ ] T033 [P] Add a `CHANGELOG.md` entry.
- [ ] T034 Re-run the full quickstart Phase C table end to end on the live host and record the results against SC-000…SC-010, including the before/after comparison from T002. This is the feature's acceptance evidence, not the test suite.
- [ ] T035 Update `specs/041-shutdown-resource-cleanup/research.md` R2 with the final measured conclusion, so the next person reading `docs/operations.md` finds the evidence rather than the superseded claim.

---

## Dependencies

```text
T001 ──► T002 ──► T003 (GATE: design may be invalidated here)
                   │
                   ├──► Phase 2 (T006…T010)  ──► Phase 3 (US1, T011…T020)
                   │                                   │
                   │                                   ▼
                   │                            Phase 4 (US3, T021…T027)
                   │                                   │
   T005 (GATE) ────┴───────────────────────────────────┴──► Phase 5 (US2, T028…T031)
                                                              │
                                                              ▼
                                                        Phase 6 (T032…T035)
T004 ──► T009 (decides the `timeout` argv form)
```

- **US1 depends on** Phase 2 only. It is a complete increment on its own.
- **US3 depends on** US1 (there must be a teardown to bound and report).
- **US2 depends on** Phase 2 and T005; it uses US3's `TeardownOutcome` for reporting, so
  scheduling it after US3 avoids reworking its reporting.

## Parallel opportunities

- T004 and T005 are independent live checks — run them in the same session as T002/T003.
- T019 (compose) is independent of the Rust work in Phase 3.
- T026 (message wording) is independent of the rest of Phase 4.
- T032 and T033 are independent files.

## Implementation strategy

**MVP is Phase 3 (US1).** It resolves the complaint that started this feature — an
immediate restart stops costing 2.5 minutes per line — and is independently deployable
and revertible. US3 makes the failure modes legible; US2 covers the ungraceful exit.

Stop after any checkpoint and the tree is green, the deployment is shippable, and the
remaining phases are still available.
