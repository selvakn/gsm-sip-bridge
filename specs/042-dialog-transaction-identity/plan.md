# Implementation Plan: Match in-dialog SIP requests to the call they name

**Branch**: `042-dialog-transaction-identity` | **Date**: 2026-08-26 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `/specs/042-dialog-transaction-identity/spec.md`

## Summary

Batch 3 of the terminating-side protocol conformance review
(`docs/plans/mt-conformance-findings.md`): fix MT-01 (no server transaction
layer), MT-02 (re-INVITE refused busy), and MT-08 (BYE not matched to a
dialog). All three share one mechanism: check an inbound request's Call-ID
(and, for INVITE, CSeq) against the single `ActiveCall` a line holds before
acting on it, rather than acting on any request just because a call happens
to be active. No generic transaction table — the codebase's one-call-per-line
architecture makes that unjustified complexity (Constitution Principle V).

## Technical Context

**Language/Version**: Rust (existing crate edition/toolchain, unchanged)
**Primary Dependencies**: None new — uses only `crate::ims::sip_client::SipRequest`/response builders already in the tree
**Storage**: N/A (in-memory call state only, unchanged shape)
**Testing**: `cargo test` / `make test` (colocated `#[cfg(test)] mod tests`, matching every other module in `src/ims/agent/`); real-hardware verification via `test/` docker rig + `siptest`, per this project's Integration-First Testing principle
**Target Platform**: Linux (unchanged — same binary, same deployment)
**Project Type**: Single Rust crate (existing structure, no new crates/binaries)
**Performance Goals**: N/A — correctness fix, no new load or hot path changes (all checks are O(1) string comparisons on the existing dispatch path)
**Constraints**: Must not change the one-call-per-line admission behavior for genuinely separate calls (`Admission::for_current`/`486` stays as-is); must pass `make format && make lint && make test` (whole workspace, `-D warnings`) before any commit, per `CLAUDE.md`
**Scale/Scope**: 3 findings, 1 shared mechanism, 4 source files, 0 new dependencies, 0 new crates

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-checked after Phase 1 design — no changes, still passes.*

- **I. Integration-First Testing (NON-NEGOTIABLE)**: Every new pure function
  (`classify_in_dialog_invite`, `names_active_call`, `cancel_response`,
  `bye_response_if_unmatched`) is exercised by real `SipRequest` values built
  the same way existing tests in these files already do (no mocked SIP
  parsing). The two branches that need a live socket/session harness
  (retransmit-while-ringing, retransmit/re-INVITE-while-answered) are
  explicitly called out as hardware-verification-only in `quickstart.md`,
  exactly mirroring the existing, already-accepted pattern for
  `await_pbx_answer`'s CANCEL-during-ring branch. **PASS.**
- **II. Green-on-Commit (NON-NEGOTIABLE)**: `make format && make lint && make
  test` runs before each commit, whole workspace, per `CLAUDE.md`'s
  pre-commit checklist (which is this project's concrete expression of this
  principle). **PASS** (gate applied at implementation time).
- **III. Frequent Atomic Commits**: Task breakdown groups by finding
  (MT-08/BYE, MT-01/retransmission+CANCEL+ACK, MT-02/re-INVITE) so each can
  land as its own focused commit, matching how batches 1 and 2 were
  committed. **PASS** (structure supports it; actual commit boundaries are
  the user's call per this project's own commit-only-when-asked convention).
- **IV. Makefile-Driven Build**: No new build steps, targets, or tooling —
  uses the existing `make format`/`make lint`/`make test`. **PASS.**
- **V. Simplicity & Refactorability**: This is the plan's central design
  constraint — see `research.md` Decision 1. Explicitly rejected a generic
  transaction-table engine in favor of the minimum mechanism the confirmed
  architecture needs. **PASS.**

No violations. Complexity Tracking table is empty.

## Project Structure

### Documentation (this feature)

```text
specs/042-dialog-transaction-identity/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md         # Phase 1 output
├── quickstart.md         # Phase 1 output
├── contracts/
│   └── sip-response-contract.md
└── tasks.md              # Phase 2 output (/speckit.tasks — not yet created)
```

### Source Code (repository root)

Single project (existing crate) — no new directories. Real paths, all
verified against current source in Phase 0:

```text
gsm-sip-bridge/
├── src/ims/agent/
│   ├── mod.rs         # dispatch_loop arms (INVITE/BYE/ACK/new CANCEL arm),
│   │                  # names_active_call, cancel_response,
│   │                  # bye_response_if_unmatched, handle_inbound_invite's
│   │                  # new pre-check — plus their #[cfg(test)] mod tests
│   ├── call.rs        # ActiveCall.answered_invite field, CachedInviteAnswer,
│   │                  # InDialogInvite, classify_in_dialog_invite — plus tests
│   ├── inbound.rs     # await_pbx_answer's retransmit-while-ringing branch,
│   │                  # the ActiveCall { .. } construction site (line ~387)
│   └── origination.rs # the second ActiveCall { .. } construction site (~1536)
├── docs/plans/mt-conformance-findings.md  # mark MT-01/02/08 [x], "Landed" writeup
└── RELEASE_NOTES.md   # one entry under ## Unreleased
```

**Structure Decision**: No new modules or files. Every change lands inside
the existing `src/ims/agent/` module, following that module's own
established pattern (small pure functions/enums colocated with the struct
they classify, tests in a `#[cfg(test)] mod tests` at the bottom of the same
file) — the same pattern batches 1 and 2 already used for this codebase.

## Complexity Tracking

*(empty — no constitution violations)*
