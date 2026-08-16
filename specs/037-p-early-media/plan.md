# Implementation Plan: Early Media Relay for Outbound Calls

**Branch**: `037-p-early-media` | **Date**: 2026-08-15 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/037-p-early-media/spec.md`

## Summary

Outbound calls today get no audio at all before the carrier's final `200
OK` — including when the carrier is actively playing a pre-answer
announcement (observed: Jio sends ~13.7s of `P-Early-Media: sendonly` SDP
audio on outbound attempts). The caller hears silence, which is
indistinguishable from nothing happening.

Technical approach: mirror the pattern the *inbound* direction already
uses — `Endpoint::pair_calls` conference-bridges Agent B's phone/PBX leg
to its veth leg *before either is answered* (`bridge_call`,
`vowifi/mod.rs`). For outbound, do the same pairing (and the carrier-side
RTP connect + veth listener spawn) the moment the carrier's *first*
SDP-bearing provisional response arrives, instead of waiting for the real
`200 OK`. A new one-shot control message, `CallEarlyMedia`, tells Agent B
to pair and answer the local leg with `183` instead of `180`. When the
real `200 OK` follows, the already-wired relay and pairing are reused
unchanged (zero-gap handoff, SC-005) rather than rebuilt.

## Technical Context

**Language/Version**: Rust 2021 (workspace `Cargo.toml`)
**Primary Dependencies**: `pjsua-safe` (this workspace's safe wrapper over
PJSIP, `pjsua-sys` FFI bindings) for Agent B's SIP/RTP/conference-bridge
primitives; no new external dependency
**Storage**: N/A (no persisted data; in-memory call state only)
**Testing**: `cargo test` — synthetic-`SipResponse` unit tests for the
Agent A state machine (existing pattern, e.g. `sip_client.rs`'s `183`
parsing test), wire round-trip tests for the new `ControlMessage` variant
(existing pattern, `control.rs`), plus manual live-call verification
against a real carrier for the parts integration tests can't reach
(PJSIP/RTP hardware path) — consistent with this project's constitution,
which permits skipping mocks only where a real component genuinely can't
run in CI
**Target Platform**: Linux, the existing two-process bridge (Agent A /
`ims::agent`, carrier-facing; Agent B / `vowifi`, PBX-facing) linked
against real PJSIP (`pjsip-linked` feature) for the live path, stub mode
for CI
**Project Type**: Single Rust workspace (existing structure; no new crate)
**Performance Goals**: caller hears carrier pre-answer audio within 1s of
the carrier sending it (SC-001); zero perceptible gap at the moment of
answer (SC-005)
**Constraints**: outbound calls only (FR-007); MUST NOT make outbound call
setup less reliable — if early-media setup fails for a given attempt, the
call proceeds with today's plain-ringing behavior rather than failing
(FR-006)
**Scale/Scope**: one call per line, same as today; no new concurrency
model — existing multi-line infrastructure (per-line derived ports)
already covers concurrent lines and needs no change here

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

- **I. Integration-First Testing**: Satisfied. New/changed logic
  (Agent A's response state machine, the new control message's wire
  format) gets real unit tests against real parsing/serialization code,
  no mocked boundaries. The one thing that can't run in CI — the actual
  PJSIP conference-bridge/RTP behavior against a live carrier — is the
  same hardware exemption this project's existing PJSIP-linked/stub split
  already relies on, not a new mock introduced by this feature.
- **II. Green-on-Commit**: Satisfied. `make test` must pass before every
  commit per the repo's mandatory pre-commit checklist (`CLAUDE.md`); no
  deviation planned.
- **III. Frequent Atomic Commits**: Satisfied. Tasks below are scoped so
  each is committable independently (protocol message, Agent A state
  machine, Agent B pairing split, teardown wiring, tests).
- **IV. Makefile-Driven Build**: Satisfied. No new build/test entry
  points needed; `make format`/`make lint`/`make test` cover this change.
- **V. Simplicity & Refactorability**: Satisfied by construction — the
  whole design is "reuse the existing `pair_calls`-before-answer
  primitive and the existing `spawn_veth_uas_listener`/`spawn_relay`
  helpers earlier," not new infrastructure. See `research.md` for the
  alternative (a parallel early-media-specific media path) rejected for
  adding a second mechanism to maintain.

No violations. Complexity Tracking table omitted (nothing to justify).

## Project Structure

### Documentation (this feature)

```text
specs/037-p-early-media/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md         # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/           # Phase 1 output
└── tasks.md             # Phase 2 output (/speckit-tasks)
```

### Source Code (repository root)

Existing single-project Rust workspace; no new crates or top-level
directories. Files touched:

```text
gsm-sip-bridge/src/
├── vowifi/
│   ├── control.rs        # new CallEarlyMedia ControlMessage variant + wire test
│   └── mod.rs             # try_place_on_line poll arm; bridge_outbound_leg
│                           # split into pair_veth_leg + finalize; early-paired
│                           # state reachable from service_active_outbound_call
├── ims/
│   ├── agent/
│   │   ├── origination.rs # early-media detection in on_carrier_response,
│   │   │                   # early RTP connect + veth spawn, dedup at the
│   │   │                   # real 200 OK, teardown from the new pre-CallPlaced
│   │   │                   # paired state
│   │   └── veth.rs         # unchanged interface; spawn_veth_uas_listener /
│   │                        # spawn_relay reused, just invoked earlier
│   └── sip_client.rs       # unit test fixtures only (existing 183 test)
```

**Structure Decision**: No new project/crate. This is a state-machine and
protocol extension entirely inside the existing `gsm-sip-bridge` crate,
touching the same files the outbound-calling (`specs/025`) and
interruptible-wait (`specs/029`) features already own.

## Complexity Tracking

*No violations — table omitted.*
