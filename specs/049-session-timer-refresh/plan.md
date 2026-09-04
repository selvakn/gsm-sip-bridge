# Implementation Plan: Honour RFC 4028 session-timer refresh on outbound calls

**Branch**: `049-session-timer-refresh` | **Date**: 2026-09-04 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `/specs/049-session-timer-refresh/spec.md`

## Summary

`origination.rs`'s outbound-call response handling never inspects
`Session-Expires`/`Require` on a carrier's `200 OK`, so a carrier that ever
requires session-timer refresh on a call that connects would silently drop
it at the interval — an unguarded gap `docs/todo.md` has tracked since
2026-08-24. This plan implements the real mechanics RFC 4028 defines (not
a defensive stub): read `Session-Expires` on the outbound call's `200 OK`,
resolve the refresher role per §7.2 (with one defensive default for a
non-compliant response, research.md Decision 2), and either send this
bridge's own periodic `UPDATE` refresh (half the interval, §7.4) or accept
the carrier's own in-dialog `UPDATE` refresh — both via `UPDATE` only, no
re-INVITE-based refresh (Decision 1). A failed refresh, in either
direction, ends the call cleanly with a distinct, diagnosable reason
(`EndedBy::SessionTimerExpired`) rather than a silent drop. Outbound
INVITEs remain unchanged — `[vowifi] originating_headers`'s `supported`
token stays off by default; this feature is purely reactive to what a
carrier's `200 OK` says.

## Technical Context

**Language/Version**: Rust (existing crate edition/toolchain, unchanged)
**Primary Dependencies**: None new — pure additions to `crate::ims::agent`
and `crate::ims::sip_client`
**Storage**: N/A (refresh state lives on `ActiveCall`, per-call, in memory)
**Testing**: `cargo test` / `make test` (colocated `#[cfg(test)] mod tests`
in `agent/session_refresh.rs`, `sip_client.rs`, `agent/origination.rs`,
`agent/mod.rs`, `ims/lifecycle.rs`, matching each file's existing fixture
style); real-hardware verification is regression-only (quickstart.md) —
no carrier reachable here has ever sent `Session-Expires` on a `200 OK`,
per Integration-First Testing's own allowance for a genuinely
unreachable-live path (same posture `specs/048` already established for
this identical feature area)
**Target Platform**: Linux (unchanged)
**Project Type**: Single Rust crate (existing structure, no new
crates/binaries)
**Performance Goals**: N/A — one new `Option` check per `dispatch_loop`
tick on an already-O(1)-per-tick path
**Constraints**: No re-INVITE-based refresh (research.md Decision 1); no
per-response-code retry ladder (Decision 3); must pass
`make format && make lint && make test` (whole workspace, `-D warnings`)
before any commit, per `CLAUDE.md`
**Scale/Scope**: 1 new module (`agent/session_refresh.rs`, the pure state
machine); 6 existing files touched (`agent/origination.rs`,
`agent/call.rs`, `agent/mod.rs`, `ims/sip_client.rs`, `ims/lifecycle.rs`,
`vowifi/control.rs`); 0 new dependencies

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-checked after Phase 1 design
— no changes, still passes.*

- **I. Integration-First Testing (NON-NEGOTIABLE)**: Every new function
  (`Session-Expires` parsing, the refresh state machine, the new `UPDATE`
  builder, the new dispatch-loop hooks) is exercised by real
  `SipResponse`/`SipRequest` fixture values built the same way
  `origination.rs`/`ping.rs`/`sip_client.rs`'s existing tests already do —
  no mocked parsing, no mocked state machine. The one genuinely
  unreachable-live path (no carrier here has ever sent `Session-Expires`
  on a connecting call's `200 OK`) is the documented, justified exception
  the constitution itself allows, identical to `specs/048`'s own posture
  for this same feature area. **PASS.**
- **II. Green-on-Commit (NON-NEGOTIABLE)**: `make format && make lint &&
  make test` runs before each commit, whole workspace. **PASS** (gate
  applied at implementation time).
- **III. Frequent Atomic Commits**: Task breakdown groups by concern (the
  new state-machine module, the `UPDATE` builder, threading the field
  through `origination.rs`/`ActiveCall`, the dispatch-loop hooks, the
  observability variant), matching prior batches' per-concern commit
  pattern. **PASS.**
- **IV. Makefile-Driven Build**: No new build steps or tooling. **PASS.**
- **V. Simplicity & Refactorability**: Central design constraint —
  research.md Decisions 1 and 3 explicitly reject building a re-INVITE
  fallback and a per-response-code retry ladder, the same scope discipline
  `specs/048`'s plan already applied to this identical feature area.
  **PASS.**

No violations. Complexity Tracking table is empty.

## Project Structure

### Documentation (this feature)

```text
specs/049-session-timer-refresh/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output
├── quickstart.md         # Phase 1 output
└── tasks.md              # Phase 2 output (/speckit.tasks)
```

No `contracts/` directory: this feature has no external interface of its
own to document — it is reactive behavior on an existing SIP dialog this
bridge already owns, not a new endpoint, command, or wire format this
bridge exposes to a caller. `data-model.md`'s "Control flow" section
already carries the one shape (`SessionRefreshState`/`RefreshPhase`
transitions) `specs/048`'s `contracts/precondition-answer-contract.md`
existed to pin for its wire-level answer-line table — there is no
equivalent answer table here (an `UPDATE` refresh carries no body either
way, per RFC 4028 §7.4's own recommendation).

### Source Code (repository root)

Single project (existing crate) — no new directories. Real paths, verified
against current source in Phase 0:

```text
gsm-sip-bridge/
├── src/ims/
│   ├── agent/
│   │   ├── session_refresh.rs   # NEW — Refresher, SessionRefreshState,
│   │   │                        # RefreshPhase, RefreshVerdict; pure,
│   │   │                        # mirrors ping.rs's PingState shape
│   │   ├── call.rs              # ActiveCall.session_refresh field;
│   │   │                        # DialogInfo::build_update_for;
│   │   │                        # end_call_attachment_lost generalized to
│   │   │                        # end_call_best_effort(session, call, reason)
│   │   ├── origination.rs       # 200 OK handling reads Session-Expires;
│   │   │                        # PendingOrigination carries it through to
│   │   │                        # finish_origination -> ActiveCall
│   │   └── mod.rs               # handle_session_refresh (new per-tick
│   │                            # hook); handle_carrier_response gains a
│   │                            # branch for our own UPDATE's response;
│   │                            # new UPDATE dispatch-loop match arm +
│   │                            # handle_carrier_update
│   ├── sip_client.rs            # UpdateRequest/build_update (mirrors
│   │                            # ByeRequest/build_bye)
│   └── lifecycle.rs             # EndedBy::SessionTimerExpired
├── src/vowifi/control.rs        # reason::SESSION_TIMER_EXPIRED
├── docs/todo.md                 # mark the RFC 4028 item done
└── RELEASE_NOTES.md             # one entry under ## Unreleased
```

**Structure Decision**: No new modules besides `agent/session_refresh.rs`,
which follows `agent/ping.rs`'s own precedent exactly (a small,
self-contained, pure state machine split out of `agent::mod` because it is
independently unit-testable without a socket). Every other change is the
smallest possible addition to a file whose existing shape already fits
(`ActiveCall`/`DialogInfo` already hold per-call/per-dialog state;
`dispatch_loop`'s existing per-tick hooks already established the pattern
a new one now joins; `EndedBy`/`reason` already exist as the generic
"why did this call end" vocabulary). No file is restructured beyond what
threading one new field/branch through it requires.

## Complexity Tracking

*(empty — no constitution violations)*
