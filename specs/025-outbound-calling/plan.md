# Implementation Plan: Outbound Calling

**Branch**: `025-outbound-calling` | **Date**: 2026-08-03 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/025-outbound-calling/spec.md`

**Revision note (2026-08-03)**: this plan was first written assuming two
architectural blockers (no pjsua UAS support, no command channel into a CS
modem thread) that turned out, on closer reading of the actual code, to be
smaller than assumed — one of them isn't a gap at all. See research.md
R-003/R-007/R-008 for the corrected findings this revision is based on.

## Summary

Today every call this bridge carries is initiated by the mobile network: it
answers, then calls out to a PBX or a registered phone. This feature reverses
that for the first time — a call originating on the SIP side (a PBX-sent
INVITE, or an INVITE from a phone registered in the bridge's own SIP server
mode) must cause the bridge to dial the given destination number out over
whichever SIM is idle, on any of the three carrier paths, and bridge the
resulting audio back to the SIP-side caller unmodified.

**Technical approach, corrected**: three mechanisms, each smaller than first
assumed. (1) **Dial-out leg**: `AtCommander::dial` (`ATD<number>;`, done) for
circuit-switched; for VoWiFi/VoLTE, `ims::call` already contains working UAC
INVITE-origination code (`build_invite`/`build_ack`/`build_bye`) — currently
wired only to the `ims-call`/`volte-call` CLI diagnostic tool, not the live
`ims::agent` loop, so this is a *reuse* task, not new SIP/media code
(research.md R-008). (2) **Inbound-leg acceptance**: pjsua-safe has no
UAS/incoming-call support at all today (only `on_call_state` is registered);
adding `Call::from_id`, `Call::answer`, and an `on_incoming_call` callback is
a small, well-scoped addition (~3 new `unsafe` blocks, no `pjsua-sys`
regeneration needed — research.md R-007) that unlocks both the PBX-trunk
account and, later, the SIP-server-mode phone path via `Account::local`.
(3) **Line selection across processes**: turns out the daemon already has
everything needed for the CS-only case — `ControlCmd`/`ModuleCmd` plus the
`SetMode` round-trip pattern (`modules::mod`) is the exact shape a
`ControlCmd::Dial` needs, since CS modems always live in the main daemon
process regardless of which process ends up owning the SIP side. A **new**
IPC channel is still needed, but only for the genuinely cross-process case:
reaching a VoWiFi/VoLTE line's agent process from wherever the SIP side is
hosted (research.md R-003, revised).

## Technical Context

**Language/Version**: Rust 1.x, edition 2021 (see `rust-toolchain.toml`)
**Primary Dependencies**: existing only — `pjsua-safe`/`pjsua-sys` (call
placement, SDP/RTP, now also call acceptance), `tokio` (control-plane IPC,
already has the `ControlCmd`/`ModuleCmd` pattern this reuses), `serde`/`toml`
(configuration), the hand-rolled SIP primitives in `src/ims/sip_client.rs`
and `src/ims/call.rs` (UAC INVITE origination, redirect responses). No new
crate dependencies.
**Storage**: in-memory only — an outbound request is a single in-flight call
attempt, not a persisted record.
**Testing**: `cargo nextest` via `make test` (stub `pjsua-safe` build, no
`pjsip-linked`); real hardware verification against the attached EC200s
(`/dev/ttyUSB0`–`ttyUSB6`) and the `pjsip-linked` Docker image for anything
that needs the real PJSIP stack — the same two-tier verification spec 024
used, now with actual hardware available rather than only the CI stub build.
**Target Platform**: Linux (containerised; `docker/Dockerfile`).
**Project Type**: Rust workspace — a daemon plus per-line agent processes.
**Performance Goals**: call setup well inside a phone's/PBX's INVITE
retransmit timer (RFC 3261 T1 = 500ms doubling). The existing
`agent_report_interval_seconds` (default 10s) heartbeat remains too slow for
call placement — still the reason a dedicated channel is needed for the
cross-process case, even though the same-process (CS) case turned out not to
need one at all.
**Constraints**:
- `tools/count-unsafe.sh` (via `make lint`): `gsm-sip-bridge/src` must stay
  at 0 `unsafe`; `pjsua-safe/src` is currently 29 blocks / 1.68% of a 5%
  ceiling — the ~3 new blocks for UAS support leave ample headroom.
- The `pjsip-linked` feature is not compiled by `make test`/`make lint`/CI —
  only `docker/Dockerfile` builds it, and only there can the new
  `on_incoming_call` path be exercised for real. A running,
  already-`pjsip-linked`-built container is available on this host for that
  purpose.
- One call per SIM at a time (existing design) — outbound and inbound calls
  contend for the same busy/idle state per line.
**Scale/Scope**: same SIM/line counts as today; one outbound attempt at a
time per line; no queueing.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-checked after Phase 1 design.*

| Principle | Assessment | Verdict |
|---|---|---|
| **I. Integration-First Testing** (NON-NEGOTIABLE) | `ControlCmd::Dial` is exercised the same way `SetMode` already is — through the real `crossbeam_channel`/`oneshot` plumbing, no mock. `AtCommander::dial` is tested against the real response-parsing logic (`make_commander` harness, already done). The `pjsip-linked` additions are verified against real PJSIP in the container and real EC200 hardware, not stubbed — a stronger guarantee than a mock would give, and consistent with how spec 024's registrar was verified. | PASS |
| **II. Green-on-Commit** (NON-NEGOTIABLE) | Each mechanism lands behind `[outbound].enabled` before being wired live; stub-build `make test` stays green throughout since the `pjsip-linked` additions are `#[cfg(feature = "pjsip-linked")]`-gated, mirroring every existing pjsua-safe function. | PASS |
| **III. Frequent Atomic Commits** | pjsua-safe UAS support, `ControlCmd::Dial`, and the PBX-INVITE handler are separable commits, in that order (each buildable/testable alone). | PASS |
| **IV. Makefile-Driven Build** | No new build operations. | PASS |
| **V. Simplicity & Refactorability** | Two things once looked like new abstractions and turned out not to be: the CS command channel is a direct reuse of the existing `ControlCmd`/`ModuleCmd` pattern (zero new abstraction), and VoWiFi/VoLTE origination reuses existing `ims::call` UAC builders (reuse, not a new SIP stack). The one genuinely new abstraction — the cross-process line-command channel — is now correctly scoped to only the case that needs it (a different OS process), not applied blanket to same-process CS dialing as the first pass assumed. See Complexity Tracking. | PASS with justification |

**Post-Phase-1 re-check**: no new violations found during design. The pjsua
UAS addition follows the existing per-function `#[cfg(feature =
"pjsip-linked")]` stub-split convention exactly (`Call::answer`/`hangup`,
`Call::from_id` needs no FFI at all), so it introduces no new safety pattern
to audit beyond what `count-unsafe.sh` already gates.

## Project Structure

### Documentation (this feature)

```text
specs/025-outbound-calling/
├── plan.md              # This file (revised 2026-08-03)
├── spec.md              # Feature specification
├── research.md          # Phase 0 — revised: R-003 corrected, R-007/R-008 added
├── data-model.md        # Phase 1 — entities, state, validation rules (unchanged)
├── quickstart.md        # Phase 1 — operator walkthrough (unchanged)
├── contracts/
│   ├── config-schema.md     # The [outbound] TOML contract (unchanged)
│   ├── line-command.md      # Cross-process call-placement protocol — rescoped
│   ├── control-cmd-dial.md  # NEW: same-process ControlCmd::Dial contract (CS)
│   └── sip-dialout.md       # The on-the-wire SIP contract toward PBX/phones
└── checklists/
    └── requirements.md      # Spec quality checklist (already passing)
```

### Source Code (repository root)

```text
gsm-sip-bridge/
├── pjsua-safe/src/
│   ├── call.rs                  # NEW: Call::from_id (safe), Call::answer (pjsip-linked + stub)
│   └── endpoint.rs               # NEW: on_incoming_call registration + callback
├── src/
│   ├── modules/
│   │   ├── at_commander.rs      # DONE: dial()
│   │   └── mod.rs               # NEW: ModuleCmd::Dial, ControlCmd::Dial handling
│   ├── control/
│   │   ├── protocol.rs          # MODIFIED: ControlCmd::Dial (same-process, CS);
│   │   │                        #   PlaceCall/PlaceCallOutcome kept for cross-process only
│   │   ├── line_server.rs       # Cross-process only (VoWiFi/VoLTE agents) — deferred to Step 4
│   │   └── line_client.rs       # Cross-process only — deferred to Step 4
│   ├── sip/
│   │   ├── mod.rs               # NEW: PBX-trunk UAS INVITE handler, progress relay, teardown
│   │   └── outbound.rs          # DONE: entities/validation/selection; NEW: dial dispatch via ControlCmd::Dial
│   ├── ims/
│   │   ├── call.rs               # EXISTING: build_invite/build_ack/build_bye — to be
│   │   │                         #   generalized for reuse (deferred to Step 4 / US4)
│   │   └── agent.rs              # Deferred to Step 4 / US4: origination trigger
│   └── metrics/
│       └── mod.rs               # DONE: gsm_sip_bridge_outbound_attempts_total
├── tests/
│   ├── test_outbound_control_cmd_dial.rs   # NEW: real ControlCmd::Dial round-trip, no mocks
│   └── test_outbound_pbx_call.rs           # NEW: PBX INVITE -> CS dial, stub-buildable parts only
```

**Structure Decision**: unchanged from the original — this feature is
additive glue between subsystems that already exist. The revision doesn't
add new top-level modules beyond what was already planned; it removes one
(no new same-process IPC for CS) and shrinks another (pjsua UAS support is
now a bounded, itemized addition rather than an open scope).

## Complexity Tracking

> **Fill ONLY if Constitution Check has violations that must be justified**

| Violation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|---------------------------------------|
| Cross-process line-command channel (`control::line_server`/`line_client`) — **rescoped**, deferred to Step 4 | Only needed when the process that owns the SIP side is not the process hosting the selected line — i.e. reaching a VoWiFi/VoLTE agent's line from the daemon (or another agent). The CS-only MVP (Step 3) needs no such channel: `ControlCmd`/`ModuleCmd` already lets the daemon command a CS modem in-process. | Piggybacking on `AgentReport` (10s heartbeat) is still too slow once this channel is actually built (Step 4) — same reasoning as before, just no longer applied to a case (CS) that doesn't need it. |
| pjsua-safe UAS support (`Call::from_id`, `Call::answer`, `on_incoming_call`) | Both the PBX-trunk account and (later) `Account::local` must accept an inbound INVITE; pjsua-safe has no such capability today. | Hand-rolling SIP/media for the PBX/phone-facing leg (the way `ims::agent` does for the carrier-facing leg) was rejected: that leg already has a real, working PJSIP stack via pjsua for everything except accepting the call — duplicating it would be the actual complexity increase. |
