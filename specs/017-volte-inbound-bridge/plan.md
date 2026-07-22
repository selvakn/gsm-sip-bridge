# Implementation Plan: Inbound Call Bridging over the Host-Side LTE Registration

**Branch**: `017-volte-inbound-bridge` | **Date**: 2026-07-22 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/017-volte-inbound-bridge/spec.md`

## Summary

Answer calls arriving from the carrier over the bridge's own cellular
registration and connect them through to the operator's telephone system, as a
long-lived service that also carries the subscriber's text messages.

Phase 0 was unusually productive: **the feature's stated possibly-fatal risk is
already resolved, positively**, and **its stated central problem is already
solved in the tree**.

- The carrier *does* deliver incoming calls to our registration — four of them,
  with caller identity, each acknowledging our response (research R1).
- "One registration serving both liveness and calls" is already handled by the
  Wi-Fi agent, which defers renewal while a call is active, with the reasoning
  recorded at the site (research R2).

What remains is mostly assembly: collapse the Wi-Fi path's two processes into
one, point it at the LTE transport, and cover the two text-message delivery
routes. Three things are *not* assembly and carry the risk:

1. **A third telephone-side endpoint in one container.** The codebase already
   carries a scar from two racing for a port; this adds a third (research R3).
2. **Attachment loss interacting with calls.** The carrier tears the LTE
   attachment down roughly every two hours and the registration loop
   re-attaches automatically — that must not fire mid-call (research R2).
3. **Answering with the right audio format.** On the outbound path, getting the
   equivalent decision wrong cost a 45-fold increase in packet loss (research R7).

## Technical Context

**Language/Version**: Rust, toolchain pinned in `rust-toolchain.toml`; workspace-wide zero-`unsafe` policy enforced by `make lint`
**Primary Dependencies**: in-tree `ims/` (agent, sip_client, sdp, rtp, media_stats), `volte/` (transport, registration, guard, pcscf), `sms/`, `pjsua-safe` for the telephone-system leg, `modules/` for discovery and modem access
**Storage**: existing SQLite store for call and message history — no new schema
**Testing**: `cargo test --workspace`; integration-first per Constitution I. Message handling, codec selection and lifecycle state are pure and testable; a bridged call needs a carrier and is validated live per `quickstart.md`
**Target Platform**: Linux, inside the existing `privileged: true` + `network_mode: host` container
**Project Type**: Single Rust workspace — long-running service plus CLI
**Performance Goals**: Real-time audio, continuous for a call's length; answer fast enough that a caller does not give up
**Constraints**: one call at a time; the telephone-side library is a per-process singleton needing its own port; a card belongs to exactly one subsystem; the LTE attachment disappears periodically
**Scale/Scope**: One card per service instance, one call at a time

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Assessment | Status |
|---|---|---|
| **I. Integration-First Testing** | Message handling, codec selection and the call/renewal lifecycle are pure and tested directly, nothing mocked. Modem exchanges reuse the established wire-level simulation. A bridged call needs a carrier and a telephone system, so it is validated live rather than faked. | ✅ PASS |
| **II. Green-on-Commit** | `make format && make lint && make test` before every commit; no test may require a modem or carrier to pass. | ✅ PASS |
| **III. Frequent Atomic Commits** | Phases map to independently committable units, each leaving the suite green. | ✅ PASS |
| **IV. Makefile-Driven Build** | No new entry points; the service is a subcommand reached through existing `make` targets and supervised by the existing entrypoint. | ✅ PASS |
| **V. Simplicity & Refactorability** | **This feature removes structure rather than adding it**: one process instead of two, no private link, no inter-agent protocol. No new abstraction — the transport seam from feature 015 already carries it. | ✅ PASS |

**Post-Phase-1 re-check**: ✅ Still passing. The design adds two service modules
and extends configuration, discovery and metrics; it introduces no traits, no
indirection, and fewer processes than the path it mirrors.

## Gates

### Gate B1 — Does a bridged call connect end to end? *(blocks US1)*

Incoming calls reach us (R1) and the telephone-side library is already used
elsewhere, but the two have never been joined. The first bridged call is where
answering, the second leg and audio relay meet.

**Exit criteria**: a call dialled to the SIM rings the telephone system, is
answered, and carries audio both ways for at least 60 seconds.

### Gate B2 — Is an incoming call given conversational-voice treatment? *(measures; does not block)*

Verified for **outgoing** calls in feature 016 — a dedicated context at the
voice quality class, 136 kbps guaranteed, present only for the call. Unverified
in this direction.

**Exit criteria**: sample the modem's per-context quality class before, during
and after an inbound call and record whether the voice-class context appears.

**Either answer is a result.** Its absence would mean inbound audio is carried
as ordinary data, which the outbound experiment showed costs roughly a 45-fold
increase in packet loss.

### Gate B3 — Do the existing dashboards still work? *(blocks US3's claim)*

Adding a value to the existing `transport` label is additive for queries
(research R5), but a panel that *groups by* transport will split into two
series.

**Exit criteria**: with the service running, existing call dashboards show these
calls without modification, and any panel that changes shape is identified.

### Gate B4 — Are text messages delivered, by either route? *(blocks US5)*

Which route the carrier uses is its decision and is unmeasured (research R4).

**Exit criteria**: a text sent to the SIM while the service runs is recorded and
forwarded exactly once, and the route it arrived by is known.

## Project Structure

### Documentation (this feature)

```text
specs/017-volte-inbound-bridge/
├── plan.md              # This file
├── spec.md              # Feature specification (clarified)
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/
│   ├── volte-bridge-service-contract.md   # What the service must do
│   └── volte-status-contract.md           # The live status query
├── checklists/
│   └── requirements.md
└── tasks.md             # Phase 2 output
```

### Source Code (repository root)

```text
gsm-sip-bridge/src/
├── volte/
│   ├── bridge.rs          # NEW: the service — one process, both legs
│   ├── sms.rs             # NEW: both message routes, converging on the
│   │                      #   existing record-and-forward
│   ├── mod.rs             # MODIFY: service wiring
│   ├── registration.rs    # MODIFY: defer renewal AND re-attach during a call
│   ├── guard.rs           # REUSED as-is
│   └── pcscf.rs           # REUSED as-is
│
├── ims/
│   ├── agent.rs           # EXTRACT FROM: inbound handling, renewal deferral,
│   │                      #   message acknowledgement — shared, not copied
│   ├── sdp.rs             # MODIFY: deliberate answer-side format preference
│   ├── media_stats.rs     # REUSED: the one-way verdict (FR-017)
│   └── call.rs            # UNCHANGED
│
├── sms/                   # REUSED: reader + record_and_forward, both routes
├── modules/discovery.rs   # MODIFY: a third exclusive subsystem
├── metrics/mod.rs         # MODIFY: calls under the existing `transport`
│                          #   label; registration stays separate
├── config/mod.rs          # MODIFY: per-card selection
├── cli.rs / main.rs       # MODIFY: the service subcommand + status
└── docker/entrypoint.sh   # MODIFY: supervise it

gsm-sip-bridge/tests/
├── test_volte_bridge.rs   # NEW: lifecycle — renewal deferral, attachment loss
└── test_volte_sms.rs      # NEW: both routes, exactly-once, duplicates
```

**Structure Decision**: `volte::bridge` is new because the service is new;
`volte::sms` is separate because it has two inputs converging on one output and
deserves isolated tests. Everything else is modification. **`ims::agent` must be
*extracted from*, not copied** — FR-019 and SC-008 both require one
implementation serving both paths, and a copy would satisfy neither while
looking like it did.

## Complexity Tracking

> No Constitution violations to justify.

This feature **removes** structure relative to the path it mirrors: one process
instead of two, no veth pair, no inter-agent protocol. Three options were
considered and rejected:

| Rejected | Why |
|---|---|
| Reuse the two-process split | A private link, a control protocol and a second process, for an isolation boundary that does not exist on this path (research R3) |
| Run inside the circuit-switched daemon | That daemon owns cards this service must not touch, and merging lifecycles makes one subsystem's crash the other's outage |
| Copy `ims::agent` and adapt | Fastest to write, most expensive to own. Two copies of registration, renewal and inbound handling would drift — FR-019 and SC-008 exist precisely to prevent that |

## Implementation Phasing

| Phase | Delivers | Stories | Gate |
|---|---|---|---|
| 1 | Extract the shared inbound/renewal machinery from `ims::agent`, Wi-Fi path proven unchanged | — (FR-019) | — |
| 2 | `volte::sms` — both routes converging, exactly-once, unit-tested | US5 | — |
| 3 | `volte::bridge` — answer, place the second leg, relay audio, end both sides | US1 | **B1** |
| 4 | Lifecycle: renewal deferral, attachment loss mid-call, recovery | US2 | — |
| 5 | Live status over the control channel; calls under the existing metric label | US3 | **B3** |
| 6 | Exclusive card assignment, per-card selection, entrypoint supervision | US4 | — |
| 7 | **Live validation** — first bridged call, message delivery, soak | US1, US2, US5 | **B1, B2, B4** |

Phase 1 goes first and alone: it touches the production Wi-Fi path, and nothing
else should be in flight when that lands. Phase 2 is pure and needs no hardware.

## Notes carried forward

- **This service needs its own telephone-side port.** Two endpoints already
  raced for one; a third must not join them (research R3).
- **Extract, do not copy.** A copy of `ims::agent` would violate FR-019/SC-008
  and would drift.
- **Answer-side format choice is load-bearing**, not cosmetic — the outbound
  path measured a 45-fold loss difference from the equivalent decision.
- **Exclusive assignment removes the fallback.** A card here takes no calls when
  the path is down, which makes FR-035's availability reporting load-bearing
  rather than decorative.
- **A message must be recorded exactly once** whichever route delivered it,
  including when the network retransmits.
