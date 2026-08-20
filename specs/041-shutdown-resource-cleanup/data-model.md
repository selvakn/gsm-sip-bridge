# Phase 1 Data Model: Complete release of per-line kernel resources on stop

**Feature**: 041-shutdown-resource-cleanup | **Date**: 2026-08-20

Everything here lives in `gsm-sip-bridge/src/supervise/shutdown.rs` unless stated.

## Entities

### `StartedLine` (new, replaces `StartedVolteLine`)

What one line — of **either** bearer — caused to exist, recorded so teardown can name it.
Per the 2026-08-20 clarification, both bearers are described by one teardown, so they are
described by one record rather than two that resemble each other (FR-018). It absorbs
`StartedVolteLine` and is appended under that type's existing append-on-success
discipline and the same `shutting_down` read-guard that protects handle registration.

| Field | Purpose in teardown |
|---|---|
| `index: u32` | identity in logs and messages |
| `bearer: Bearer` (`Vowifi` \| `Volte`) | selects which steps arise at all |
| `engine: Option<Engine>` (`Strongswan` \| `Swu`) | VoWiFi only: whether a terminate step is emitted (R4) |
| `conn_name: Option<String>` | VoWiFi/strongswan only: `swanctl --terminate --ike <conn>`, line-scoped, never the bare `ims` |
| `netns: String` | namespace to run in, and to delete last |
| `tun_iface: Option<String>` | VoWiFi only: the XFRM device to delete — the load-bearing step (R1) |
| `if_id: Option<u32>` | VoWiFi only: reported in messages; feeds the all-ours set for the flush guard |
| `veth_host: String` | the container-side veth end; deleting it removes the pair. **Both bearers** — VoLTE creates one too (`orchestrate_volte.rs:359`) and nothing deletes it today |
| `agent_handles: Vec<Arc<ChildHandle>>` | the line's own processes, from `StartedVolteLine` |
| `cleanup_argv: Option<Vec<String>>` | VoLTE only: the existing in-namespace `volte-cleanup` invocation, now carried as data rather than special-cased in the builder |

A VoLTE line simply has `None` where a concept does not apply to it; no step is emitted
for a `None`. This is what makes "the guarantees hold for VoLTE by construction" true
rather than aspirational — there is no second code path in which they could fail to hold.

`StartedState.started_netns` stays as it is: a namespace must be deletable even for a
line that failed before its `StartedLine` could be recorded (FR-007).

**Invariant**: a `StartedLine` is recorded at the point the namespace and interface setup
returns, *before* anything else can fail — the same position `started_netns.push` occupies
today (`orchestrate.rs:1184`). A line that fails later is still fully torn down.

### `TeardownStep` (extended)

Existing variants — `KillChild`, `WaitForExit`, `RunInNetns`, `Run`, `DeleteNetns` — are
unchanged. Three are added:

| Variant | Fields | Meaning |
|---|---|---|
| `TerminateIke` | `conn_name`, `timeout_secs` | ask charon to tear down this line's IKE_SA and its children |
| `DeleteLink` | `netns: Option<String>`, `iface`, `timeout_secs` | delete one device, inside a namespace or in the container's own |
| `FlushXfrm` | `ours: BTreeSet<u32>` | classify, then flush only if everything present is ours (FR-011) |

`timeout_secs` is carried in the step, not applied by the executor's own clock: the bound
becomes part of the pure plan and is asserted by the same tests as the ordering (R3).
`DeleteLink`'s `netns: None` is what deletes the container-side veth end.

### `TeardownBudget` (new)

The whole-teardown deadline required by FR-019. Constructed once at the start of
execution from the stop allowance, minus a reserve sized to cover the release steps for
the configured line count.

| Field | Meaning |
|---|---|
| `deadline: Instant` | when the runtime is expected to force-kill us |
| `reserve: Duration` | what the `DeleteLink`/`DeleteNetns` steps need for this line count |

Checked before each *waiting* step (`WaitForExit`, `TerminateIke`, `FlushXfrm`). When
`now + reserve >= deadline`, those steps are abandoned and execution jumps to the release
steps. The budget deliberately lives in the **executor**, not in `build_shutdown_plan`:
the plan stays a pure function of what started, and "how much time is left" is a property
of the running world. What the plan owns is the *partition* — which steps are abandonable
and which are the release steps — so the fallback is still an assertion over the plan
rather than a behaviour only observable at runtime.

### `TeardownOutcome` (new)

`execute_shutdown_plan` currently returns `()` and discards every result — the same
pattern that made a prior live failure take an hour to diagnose. It returns a summary
instead: per step, what it was and whether it succeeded; the resources that could not be
released; and which steps were abandoned to the budget (FR-019). FR-012's report is
rendered from this, and its emptiness is what "teardown completed cleanly" means.

Best-effort execution is unchanged: a failed step never stops the rest. Per FR-020 the
outcome is *reported only* — it raises no alert and does not affect the exit code.

## Ordering invariants

These are the assertions the ordering tests encode. Each is a position comparison over
the `Vec` returned by `build_shutdown_plan` — no runner involved.

| # | Invariant | Why |
|---|---|---|
| O-1 | Every line's `TerminateIke` precedes charon's `KillChild` | a killed charon cannot terminate anything (FR-003) |
| O-2 | Every `KillChild` has a matching `WaitForExit` after it, before any resource that child uses is deleted | FR-001; today no VoWiFi child has one at all |
| O-3 | Every in-namespace child's `WaitForExit` precedes that namespace's `DeleteLink` steps | a process in the namespace holds the device (R2) |
| O-4 | `FlushXfrm` follows every `TerminateIke` and charon's exit | flushing under a live charon invites reinstallation (FR-004) |
| O-5 | A line's `DeleteLink` steps follow `FlushXfrm` | the state referencing the device must go first (R2) |
| O-6 | A line's `DeleteNetns` follows all of that line's `DeleteLink` steps | FR-006 |
| O-7 | Every recorded namespace gets a `DeleteNetns`, including lines with no `StartedLine` | FR-007 |
| O-8 | Every step that can block carries a non-zero `timeout_secs` | FR-009 |
| O-9 | VoLTE's existing order (carrier-agent kill → in-namespace `volte-cleanup` → `DeleteNetns`) is preserved, now expressed in the shared vocabulary | FR-018: the expression changes, the observable order does not |
| O-10 | Every step is classified either abandonable (waits, terminate, flush) or a release step (`DeleteLink`, `DeleteNetns`); dropping every abandonable step leaves a sequence that still releases every device and namespace | FR-019 — the budget fallback is an assertion over the plan, not a runtime-only behaviour |
| O-11 | A VoLTE line gets the same `DeleteLink` for its veth and the same bounds and reporting as a VoWiFi one | FR-018; today VoLTE's veth is never deleted |

## Resource lifecycle

```text
                 created by                          released by
  netns          ensure_epdg_interface /             DeleteNetns          (existed, kept)
                 the VoLTE line setup
  tun23-N        ensure_epdg_interface (host ns,     DeleteLink{netns}    (NEW — releases if_id)
  (VoWiFi only)  then moved into netns)
  veth pair      start_line_tail (VoWiFi),           DeleteLink{None}     (NEW, both bearers)
                 ensure_volte_line_veth (VoLTE)
  IKE/CHILD SA   swanctl --initiate                  TerminateIke         (NEW)
  (VoWiFi only)
  XFRM state,    charon, via the updown script       FlushXfrm            (NEW at stop;
  policies                                                                existed at start only)
  (VoWiFi only)
```

The left column is what a run takes; the middle is where it is taken; the right is the
step that gives it back. Before this feature the right column had one entry.

## Start-side reclamation

`reclaim_previous_run(runner, lines) -> ReclaimReport` in `epdg_iface.rs`, called once
before any line is set up.

It enumerates namespaces on the host matching the deployment's own naming patterns
(`ims<N>`, `volte<N>`) rather than walking the config's line table — VoLTE lines are
auto-discovered per modem, so a previous run may have had lines this one does not, and a
leftover from that run must still be reclaimable. For each such namespace not created by
this run, it builds **the same steps** through the same builder and executes them. One
code path for "release a line's resources" is what makes FR-008 (idempotence) structural
rather than aspirational — a step sequence that is safe to run twice is exactly a step
sequence that is safe to run against a previous run's leftovers.

States a namespace matching our patterns can be in at startup:

| Observed | Action |
|---|---|
| No namespace of that name | nothing to do (the clean-host case, FR/SC-008: no added latency) |
| Namespace exists, this run did not create it | reclaim: delete its devices, then the namespace |
| Namespace exists and this run created it | leave alone — this is the idempotent-restart path already handled by `ensure_epdg_interface` |
| XFRM state present that is not ours | leave alone and say so — existing behaviour, unchanged (FR-015) |

## What is not modelled

- No persisted manifest. Everything reclamation needs is derivable from a namespace's own
  name plus what is inside it (its devices are enumerable; interface names and `if_id`s
  are deterministic per line index), so there is no new on-disk state and nothing to keep
  in sync.
- No lock or instance registry — see R7: the deployment is single-instance by
  construction, and the guard that would express it needs `unsafe`.
