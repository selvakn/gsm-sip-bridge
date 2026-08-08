# Implementation Plan: Interruptible wait for outbound call origination

**Branch**: `029-interruptible-origination-wait` | **Date**: 2026-08-07 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `specs/029-interruptible-origination-wait/spec.md`

## Summary

Today both halves of the bridge go deaf for up to ~80s while an outbound
carrier call is being placed. Agent B parks in a blocking read and never
notices its own caller hanging up; Agent A parks in a blocking socket read and
never notices an inbound call or anything else.

The fix, in one sentence: **stop treating origination as a blocking call and
start treating it as a state the dispatch loop advances**, on both sides.

Agent B gains a poll tick during the attempt phase so it can see its caller
hang up and say so. Agent A stops reading the carrier socket directly —
receiving origination responses through `inbound.rx`, the queue that already
exists and that a background thread is already reading (see R2) — which makes
the wait interruptible for free and lets every other dispatch-loop duty
(inbound INVITEs, SMS, keepalive) keep running throughout.

Two findings surfaced during research that were not in the spec or the triage
plan, and both are folded in: a probable **two-readers-on-one-socket race** on
the carrier connection (R2), and an **outbound lifecycle that never reaches
`Bridged`** (R5).

## Technical Context

**Language/Version**: Rust 2021, workspace at repo root
**Primary Dependencies**: std only for this change (`std::sync::mpsc`,
`std::net::TcpStream`); existing in-tree `ims::sip_client`, `ims::lifecycle`,
`vowifi::control`; `pjsua_safe` on the Agent B side
**Storage**: N/A — no persisted state; call records go through the existing
observability reporter
**Testing**: `make test` (cargo test, workspace); integration tests in
`gsm-sip-bridge/tests/`; unit tests in-module under `#[cfg(test)]`
**Target Platform**: Linux; Agent A runs in a per-line netns (Wi-Fi) or over
loopback (cellular)
**Project Type**: Single Rust workspace — library plus daemon binaries
**Performance Goals**: caller hangup → CANCEL on the wire within ~200ms
(budget in R8); SC-001 allows 10s
**Constraints**: single-threaded dispatch loop preserved; no new locks and no
second writer on the carrier transport (FR-016); no existing timeout constant
changes (FR-015)
**Scale/Scope**: one call at a time per line, unchanged; N lines per host

## Constitution Check

*GATE: evaluated before Phase 0 and re-checked after Phase 1 design.*

| Principle | Verdict | Notes |
|---|---|---|
| I. Integration-First Testing | **PASS** | Tests drive real `dispatch_loop`/`run_outbound_listener` code over real TCP sockets and real `mpsc` channels. The only stand-in is the carrier peer itself (a fake SIP endpoint on a real socket) — a real IMS carrier is not runnable locally, which is exactly the exemption Principle I allows. Every such site carries the required written justification comment. **No mocking of `RegisteredSession`, `Inbound`, or the control protocol** — all real. |
| II. Green-on-Commit | **PASS** | `make format && make lint && make test` before every commit, per `CLAUDE.md`. `make lint` covers all test targets. |
| III. Frequent Atomic Commits | **PASS** | Tasks are sized to one commit each; phase boundaries are natural commit points. |
| IV. Makefile-Driven Build | **PASS** | No new targets needed; existing `format`/`lint`/`test`/`build` suffice. |
| V. Simplicity & Refactorability | **PASS, with justification** | See Complexity Tracking — the restructure removes a blocking call, a duplicate socket reader, and a would-be second message pump. Net moving parts decrease. One new type is added. |

**Post-Phase-1 re-check**: unchanged. The design added no abstraction layer, no
trait, no new thread, and no new lock. It added one plain struct
(`PendingOrigination`) and one enum (`OriginationStep`), both concrete and
local to `ims::agent`.

## Project Structure

### Documentation (this feature)

```text
specs/029-interruptible-origination-wait/
├── plan.md              # This file
├── spec.md              # Feature specification
├── research.md          # Phase 0 output — R1..R8
├── data-model.md        # Phase 1 output — state machine and transitions
├── quickstart.md        # Phase 1 output — how to exercise it locally
├── contracts/
│   └── agent-outbound-protocol-delta.md   # Phase 1 output
├── checklists/
│   └── requirements.md
└── tasks.md             # Phase 2 output (/speckit-tasks)
```

### Source Code (repository root)

Only files this feature touches:

```text
gsm-sip-bridge/
├── src/
│   ├── ims/
│   │   ├── agent.rs        # dispatch_loop, originate_and_bridge → split into
│   │   │                   #   begin/advance/finish; PendingOrigination;
│   │   │                   #   cancel_pending_invite reason plumbing
│   │   ├── lifecycle.rs    # (test-only additions; no rule changes)
│   │   ├── session.rs      # unchanged — reader ownership documented, not moved
│   │   └── sip_client.rs   # recv_final_response_for_origination retired from
│   │                       #   the origination path (kept for CANCEL)
│   ├── vowifi/
│   │   ├── mod.rs          # await_place_call_outcome gains a poll tick;
│   │   │                   #   run_outbound_listener handles the new outcome
│   │   └── control.rs      # no wire change; reason constant reused
│   └── control/
│       └── protocol.rs     # OutboundAttemptOutcome gains CallerAbandoned
└── tests/
    ├── test_outbound_origination_race.rs   # NEW — T001, settles R2
    ├── test_outbound_abandon.rs            # NEW — US1
    └── test_vowifi_call_metrics.rs         # extended — US3
```

**Structure Decision**: No new modules or crates. The change is concentrated in
the two existing dispatch loops — `ims::agent::dispatch_loop` (carrier side) and
`vowifi::run_outbound_listener` (telephone side) — because that is where the
blocking already lives. Splitting origination into its own module was considered
and rejected: it would separate the state machine from the loop that drives it,
which is the coupling that makes the design work.

## Design in brief

Full detail is in [data-model.md](./data-model.md); the shape is:

**Agent A** — `dispatch_loop` gains `pending: Option<PendingOrigination>`
alongside `active_call: Option<ActiveCall>`.

- `PlaceCall` arrives, line idle → `begin_origination` builds the SDP offer,
  sends the INVITE, spawns the control reader **now** (not after success), and
  returns a `PendingOrigination` carrying the dialog identifiers, the deadline,
  and a `BridgedCall` at stage `Answering`.
- The loop's existing `inbound.rx` response arm gains one check: does this
  response's `Call-ID` match `pending`? If so, advance the state machine
  (`180` → `PbxRinging` + relay `CallRinging`; `2xx` → finish; non-2xx → abort).
  If not, fall through to the keepalive/other handling exactly as today.
- The loop's existing inbound-INVITE arm consults `Admission` over *either*
  `active_call` or `pending` — so an inbound call during an attempt gets
  `486 Busy Here` through the path it already uses (FR-011/012/013).
- Every tick, if `pending` is `Some`: check `ctrl_rx` for
  `CallEnded{call_id, ...}`, check the deadline, and (once the carrier has
  answered) check the veth channel. Abandonment and timeout share one exit:
  `cancel_pending_invite`, differing only in the reason recorded.

**Agent B** — `await_place_call_outcome` drops its 90s blocking read to a
~100ms poll with a carried `pending_line` buffer (R7), and on each tick checks
`call.poll_state()`. On `Disconnected` it writes
`CallEnded{call_id, reason: CALLER_HANGUP}` to Agent A and returns a new
`PlaceCallOutcome::Abandoned`, which stops the line-by-line retry loop (FR-004).

**What is deliberately not changed**: every timeout constant (FR-015);
`Admission`'s rule; the control-protocol wire format; the `RECV_TIMEOUT` on the
carrier socket; registration and renewal.

## Delivery phases

Ordered so that the riskiest unknown is settled first and each user story lands
as a working increment.

| Phase | Delivers | Gate |
|---|---|---|
| **0. Settle R2** | A test that proves or disproves the two-reader race | Its result decides whether Phase 2 is a bug fix or a refactor — but the plan is the same either way |
| **1. Agent B detects (US1a)** | Caller hangup observed and reported during an attempt | Independently testable; no Agent A change needed to verify the message is sent |
| **2. Agent A acts (US1b)** | Origination as loop state; CANCEL on abandonment; veth wait interruptible | **US1 complete** — SC-001, SC-002, SC-003 |
| **3. Inbound not starved (US2)** | `486` during an attempt, recorded like any busy refusal | **US2 complete** — SC-004 |
| **4. Outcomes (US3)** | `CallerAbandoned` outcome; the R5 lifecycle fix | **US3 complete** — SC-005 |
| **5. Docs** | The three known-limitation sites updated (FR-020) | SC-006, SC-007 regression pass |

Phase 1 is genuinely independent and could ship alone (it makes the hangup
*visible in logs* even before Agent A acts on it). Phases 2–4 build on Phase 2's
restructure and are not independently orderable.

## Risks

| Risk | Mitigation |
|---|---|
| The Phase 2 restructure regresses successful outbound calls — the path with real hardware verification behind it (`specs/025` T072, five passes) | SC-006 is an explicit regression criterion, including the observed 18s inter-provisional gap. Tests assert it before the restructure lands, so they fail if it breaks. |
| R2 turns out not to be a race, making Phase 2's premise partly wrong | The design does not depend on it. Routing through `inbound.rx` is correct regardless; if there is no race, Phase 2 is a pure refactor that enables FR-011 and nothing is lost. |
| Splitting `originate_and_bridge` loses one of its many careful teardown paths (leaked carrier leg, unpaired call, orphaned relay thread) | Those paths are the accumulated result of five hardware passes. Task-level rule: each teardown branch moves as a unit and keeps its comment. A dedicated task audits the before/after set. |
| Agent B's shorter read timeout causes message fragmentation | Reuse the existing, already-debugged `pending_line` pattern (R7); a test feeds a message in two chunks across a timeout boundary. |
| `CallEnded` arriving for a *stale* attempt cancels a live one | FR-010: match on `call_id`; test covers the mismatched case. |

## Complexity Tracking

> Constitution Check passes; this records the one addition and why the larger
> restructure is a simplification rather than a violation.

| Addition | Why needed | Simpler alternative rejected because |
|---|---|---|
| `PendingOrigination` struct + `OriginationStep` enum in `ims::agent` | An in-flight attempt has real state (dialog identifiers, deadline, RTP socket, control reader, lifecycle) that must survive across loop ticks once the wait is no longer a single stack frame | Keeping the blocking call and adding an `on_poll` callback — rejected in R3: it cannot satisfy FR-011 without duplicating the dispatch loop's message handling, and would silently drop inbound SMS |
| Restructuring `originate_and_bridge` (~350 lines) into begin/advance/finish | Required by the above | *Net effect is fewer moving parts, not more*: it deletes a blocking read on the carrier socket, removes the second concurrent reader of that socket (R2), and avoids introducing a second message pump. No new thread, lock, trait, or module. |
