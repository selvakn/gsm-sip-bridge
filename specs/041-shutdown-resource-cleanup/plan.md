# Implementation Plan: Complete release of per-line kernel resources on stop

**Branch**: `041-shutdown-resource-cleanup` | **Date**: 2026-08-20 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/041-shutdown-resource-cleanup/spec.md`

## Summary

Every container restart costs each line ~2.5 minutes of being unreachable, because the
previous run's XFRM interface still holds that line's `if_id` from inside a namespace
nothing can address any more. A host reboot costs nothing, which is the tell: this is
resources not being given back, not a kernel we must wait out.

The teardown today is two steps — `SIGTERM` to every child (never waited on) and
`ip netns del` — and neither of them destroys a netdev. **Only destroying the netdev
releases the `if_id`** (R1), so the fix is built around one load-bearing step,
`ip link del`, and everything else exists to make that delete succeed promptly:

1. **Wait for the processes**, with escalation, so nothing is still using what we are
   about to delete (FR-001, FR-002).
2. **Bring the tunnels down and release the encryption state** while the container is
   still alive, so nothing holds a reference to the device (FR-003, FR-004).
3. **Delete the devices explicitly** — the XFRM interface and the veth pair — before
   the namespace, bounded so a stuck one is diagnosed rather than hung on (FR-005,
   FR-006, FR-009).
4. **Make the namespaces addressable from the host**, so a run that never got to do any
   of the above can still be cleaned up by the next one (FR-013, FR-014).
5. **Give the runtime a stop allowance** that fits all of it, and a budget check so that
   running out of it costs the waits rather than the deletes (FR-010, FR-019).

Structurally this stays inside the existing design: teardown remains a pure, ordered
`Vec<TeardownStep>` built from what actually started, so every ordering invariant is a
unit test — the same treatment the VoLTE steps already get. Per the 2026-08-20
clarifications, both bearers are described by that one vocabulary (FR-018): VoLTE's
existing cleanup moves into it rather than remaining a parallel implementation, which is
what makes the ordering, bounding and reporting guarantees hold for a VoLTE line by
construction.

## Technical Context

**Language/Version**: Rust (workspace edition; toolchain pinned in `rust-toolchain.toml`)
**Primary Dependencies**: existing only. **No new crates.** Runtime deps are `iproute2` and `strongswan`'s `swanctl`, both already in the image
**Storage**: N/A
**Testing**: `cargo test` via `make test`. Kernel/netns effects are not reproducible in CI, so the *plan* is unit-tested (steps, order, bounds, reporting) and the *outcomes* are measured live — see quickstart.md
**Target Platform**: Linux, privileged host-networked Alpine container; aarch64 (Raspberry Pi) in production, x86-64 in dev
**Project Type**: single Rust workspace — supervisor + CLI subcommands
**Performance Goals**: an immediate restart costs no more than 10s over the well-separated baseline (SC-000/SC-001); today it costs 150-185s. Teardown completes within the stop allowance (SC-004), and releases every identifier even when it does not (SC-010)
**Constraints**: zero `unsafe` (`make lint` counts it); `make lint` is workspace-wide with `-D warnings` over all targets; the unfiltered XFRM flush keeps its all-ours-or-nothing guard (FR-011); teardown must stay a pure function of `StartedState`
**Scale/Scope**: 1-4 lines per host, both bearers; 20 functional requirements; 3 independently shippable slices

## Constitution Check

*GATE: evaluated before Phase 0 and re-evaluated after Phase 1 design.*

| Principle | Assessment | Verdict |
|---|---|---|
| **I. Integration-First Testing** (NON-NEGOTIABLE) | The step-building is a pure function tested directly with no mock at all — the highest-value assertions here (ordering, bounds, completeness) need no I/O. Execution is tested through `MockCommandRunner`, which stands in for `ip`/`swanctl`/kernel namespace operations that cannot exist in CI; the existing mock-justification comment at that site is extended, not weakened. The behaviours a mock genuinely cannot prove — that the `if_id` is actually released — are moved to live acceptance in quickstart.md rather than faked. | **PASS** |
| **II. Green-on-Commit** (NON-NEGOTIABLE) | `make format && make lint && make test` before every commit, per the repo checklist. | **PASS** |
| **III. Frequent Atomic Commits** | Three independently deployable slices, one per spec user story (below), each leaving the tree green. | **PASS** |
| **IV. Makefile-Driven Build** | No new entry points; existing `make` targets only. | **PASS** |
| **V. Simplicity & Refactorability** | Adds no new module, no new thread, no new dependency and no new abstraction layer. Three new `TeardownStep` variants extend a vocabulary that already exists, and the two record types become one. The two additions that *do* count as machinery — unifying the bearers and the teardown budget — are justified in Complexity Tracking; two rejected alternatives are recorded there too. | **PASS with justification** |

**Reuse over new code** (Principle V in practice): `StrongswanEngine::terminate`
(`engines.rs:375`, already scoped per line and test-pinned at `:867`),
`classify_xfrm_dump` + its all-ours-or-nothing rule (`epdg_iface.rs:41`), the
`WaitForExit` step and its `KILL_CONFIRM_MAX_POLLS` bound (`shutdown.rs:102`), and the
leftover-veth cleanup already in `start_line_tail` (`orchestrate.rs:1477-1487`) are all
reused rather than reimplemented. See research.md R4, R5, R7.

## Project Structure

### Documentation (this feature)

```text
specs/041-shutdown-resource-cleanup/
├── plan.md              # This file
├── research.md          # Phase 0 — decisions, rationale, rejected alternatives
├── data-model.md        # Phase 1 — entities, the step vocabulary, ordering invariants
├── quickstart.md        # Phase 1 — live verification, including the before/after measurement
├── contracts/
│   └── observable-contracts.md   # Phase 1 — step order, log markers, compose surface
├── checklists/
│   └── requirements.md
└── tasks.md             # Phase 2 (/speckit-tasks)
```

### Source Code (repository root)

```text
gsm-sip-bridge/src/supervise/
├── shutdown.rs          # MODIFY: StartedLine (absorbs StartedVolteLine, covers both bearers);
│                        #         TerminateIke/DeleteLink/FlushXfrm steps; TeardownBudget;
│                        #         per-step outcome reporting; ordering tests O-1..O-11
├── epdg_iface.rs        # MODIFY: reuse classify_xfrm_dump at stop; reclaim_previous_run() for start
├── orchestrate.rs       # MODIFY: record each VoWiFi line's teardown facts into StartedState;
│                        #         run start-side reclamation before creating anything
├── orchestrate_volte.rs # MODIFY: record VoLTE lines as StartedLine, including their veth
└── runner.rs            # MODIFY (small): nothing structural — see R3, bounding is argv-level

docker/
├── docker-compose.yml               # MODIFY: stop_grace_period; host-visible netns dir
└── docker-compose.cellular-internet.yml  # MODIFY: same, if it starts the bridge service

docs/operations.md       # MODIFY: replace the "wait it out, nothing shortens this" section
```

**Structure Decision**: existing layout, unchanged. No new file in `src/`. The change is
concentrated in `shutdown.rs` (the plan vocabulary and its ordering tests) with a small
amount of bookkeeping in `orchestrate.rs` to record what each line created.

## Delivery slices

One per spec user story, ordered so the first delivers the complaint's resolution on its
own. Each is independently deployable and independently revertible.

| Slice | Story | Requirements | Delivers |
|---|---|---|---|
| **1. Ordered, confirmed teardown** | US1 (P1) | FR-001…FR-011, FR-018, and the compose stop allowance | SC-001…SC-006, SC-008 |
| **2. Recovery from an ungraceful exit** | US2 (P2) | FR-013…FR-016 | SC-007 |
| **3. Budget, reporting, runbook** | US3 (P3) | FR-012, FR-017, FR-019, FR-020 | SC-009, SC-010 |

Slice 1 carries per-step bounds (FR-009) because a teardown that can hang is worse than
no teardown; the *whole-teardown* budget and its fallback (FR-019) are slice 3, where they
refine an already-working sequence rather than gate it. Slice 2 is the one that can be
abandoned outright if A4 shows mount propagation cannot be made to work (R6), which costs
only SC-007.

**Implementation order is 1 → 3 → 2**, not slice order: slice 2's reclamation reports its
failures through the outcome type slice 3 introduces (FR-012), so building 2 first would
mean writing that reporting twice. tasks.md is sequenced accordingly.

## Phase 1 Design Summary

Full detail in `data-model.md` and `contracts/observable-contracts.md`. The load-bearing
decisions:

- **`StartedState` learns what a line *is*, not just what it spawned.** Today it records
  child handles plus a bare list of namespace names, which is why the plan cannot name a
  device to delete. One `StartedLine` — bearer, index, netns, veth, and the VoWiFi-only
  connection name / tun interface / if_id — absorbs `StartedVolteLine` and is appended on
  its existing append-on-success discipline. A VoLTE line carries `None` where a concept
  does not apply; no step is emitted for a `None`.
- **Teardown stays a pure function.** `build_shutdown_plan` gains steps but no I/O, so
  "terminate before kill", "kill before delete", "delete devices before the namespace"
  and "flush only after every tunnel is down" are all assertions over a returned `Vec`.
- **The XFRM flush moves to where it can work.** It exists today only at startup
  (`reclaim_stale_xfrm`), which is the one moment it cannot help — by then the device is
  in an unreachable namespace. Stop is when we still have both the knowledge and the
  access. The all-ours-or-nothing guard is carried over verbatim (FR-011).
- **Start-side reclamation reuses the same step builder** over namespaces found on the
  host rather than a second cleanup implementation, which is what makes FR-008
  (idempotence) structural instead of aspirational.
- **Bounds are argv-level** (`timeout N ip link del ...`), so no runner-trait change and
  no new thread; the bound is visible in the step and therefore assertable (R3).
- **The budget lives in the executor; the partition lives in the plan.** FR-019 needs a
  clock, which a pure function cannot have — but what it needs to *decide* is which steps
  are abandonable and which actually release resources, and that is a property of the
  plan. Splitting it this way keeps the fallback assertable without a clock in the tests
  (O-10).

## Complexity Tracking

> Constitution V requires written justification for added machinery. Nothing here needed
> new machinery; recorded instead are the two places where machinery was *rejected*.

| Addition | Why needed | Simpler alternative rejected because |
|---|---|---|
| One `StartedLine` and one step vocabulary across both bearers (absorbing `StartedVolteLine`) | FR-018: every ordering, bounding, reporting and reclamation guarantee must hold for a VoLTE line *by construction*. A second implementation is a second place for them to silently stop holding | Leaving VoLTE separate keeps its regression tests untouched — the cheaper option, chosen against deliberately (2026-08-20 clarification) because it leaves two teardowns to be kept in step by hand. The cost is real: VoLTE's ordering tests are rewritten against the new representation, asserting the same relative order |
| A whole-teardown budget (`TeardownBudget`: a deadline plus an abandonable/release partition) | FR-019: the step order is a dependency order, so the deletes come last — yet they are the only steps that release an `if_id`. Without a budget check the worst case spends the whole allowance waiting and is force-killed having released nothing | Per-step bounds plus a generous `stop_grace_period` alone. Rejected because it makes the guarantee depend on every worst case having been estimated correctly, and the failure mode is silent |

| Considered | Why rejected | What is done instead |
|---|---|---|
| A cross-container instance lock (flock on a host-visible file) to satisfy FR-016 | Needs `unsafe` (`libc::flock`) which `make lint` forbids, and it guards a case that is already impossible: two instances would collide on fixed namespace names, fixed veth addresses, and charon's wildcard bind of UDP 500/4500 — the deployment is single-instance by construction today (R7) | Document the single-instance constraint, and gate reclamation on the namespace not having been created by *this* run |
| A `run_bounded` method on `CommandRunner` | Adds a trait method, a real implementation with a wait-with-timeout, and a mock implementation, to express something the `timeout` applet already expresses in argv — and an argv-level bound is directly assertable in the plan tests (R3) | `timeout <secs>` prefix inside the step's argv |

## Post-Design Constitution Re-Check

Re-evaluated after Phase 1, and again after the 2026-08-20 clarifications: **PASS with
justification**. No new module, dependency or thread; two justified additions above.

The blast radius grew with the bearer unification: VoLTE's teardown is now in scope, and
its existing ordering tests are rewritten rather than left untouched. That is a real risk
and it is taken deliberately — the alternative was two teardowns kept in step by hand.
Mitigation is that the rewritten tests must assert the *same relative order* as today's,
so any behavioural drift shows up as a failing assertion rather than a passing rewrite;
this is called out explicitly in contracts C5 and is a review gate, not a test detail.

The riskiest element is still not code but a deployment change — exposing the namespace
directory at host scope (R6) — which is why it is a slice of its own, verified live, and
independently revertible. The three slices remain independently shippable, and slice 1
(graceful teardown) delivers SC-001 on its own.
