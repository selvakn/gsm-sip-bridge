# Implementation Plan: Outbound Calling

**Branch**: `025-outbound-calling` | **Date**: 2026-08-03 | **Spec**: [spec.md](./spec.md)

**Revision note (2026-08-03, pass 4)**: US1 (PBX) and US3 (SIP server mode)
are implemented and live-verified against this host's real deployment — a
real `302` redirect, real PJSUA `on_incoming_call`, real `503` refusal for
"no idle line," all confirmed over the air with a test SIP UAC dialing a
real number. That live test also confirmed this host's one physical modem
is fully committed to the VoWiFi tunnel, so **US4 (VoWiFi/VoLTE/PC-SC
origination) is what this specific deployment actually needs to complete a
real call** — it was scoped in the original plan as "no pjsua dependency,
reuse `ims::call`'s UAC builders," which undersold the remaining work. This
revision replaces that undersell with the real shape of it, found by reading
`ims::agent`/`vowifi::mod`'s actual structure.

## Summary

US1/US3 answer "how does an outbound-triggering INVITE reach the bridge and
get accepted." US4 answers the part that was still hand-waved: **how does
the bridge then actually place a call over VoWiFi/VoLTE**, given the
carrier-facing leg (Agent A, `ims::agent`, in the IMS network namespace) and
the phone/PBX-facing leg (Agent B, `vowifi::mod`, default namespace) are
**separate OS processes** connected only by a veth link and a small
event-driven control protocol (`vowifi::control::ControlMessage`) — today
entirely shaped for the inbound direction (`IncomingCall`/`BridgeReady`/
`CallAnswered`/...). Agent B already accepts the outbound-triggering INVITE
(US1/US3, shipped); what's missing is telling Agent A to originate a call
over the *already-registered* IMS session (a second registration would tear
the live one down — confirmed elsewhere in this codebase), and bridging the
two legs the way `bridge_call` already does for inbound, reversed.

**Technical approach**: extend `ControlMessage` with an outbound triad
(`PlaceCall`/`CallPlaced`/`CallFailed`), mirroring the existing inbound triad
(`IncomingCall`/`BridgeReady`/`BridgeFailed`) in shape and direction-reversed
in meaning. Add a UAC origination path to `ims::agent` that builds an INVITE
against the live `RegisteredSession` (reusing `ims::call`'s `InviteParts`/
`build_invite`/`AckParts`/`build_ack`/BYE-building, generalized from
`ims::call`-private to `pub(crate)` — they already take everything they need
as parameters, not global state, so this is a visibility change plus a new
UAC-role `DialogInfo` constructor next to the existing `from_invite` one, not
new SIP-message-building logic). Bridge the resulting carrier `Call`-analog
to the phone leg the same way `bridge_call` already conference-bridges two
legs for inbound. VoLTE gets this for free (`volte::carrier_agent` already
calls the same `ims::agent::serve_inbound`); PC/SC-sourced lines get it for
free (SIM source never touched call-origination code, confirmed in the
original US4 research).

## Technical Context

**Language/Version**: Rust 1.x, edition 2021
**Primary Dependencies**: existing only — `ims::call`'s UAC builders
(generalized, not rewritten), `ims::sdp` (offer/answer, already bidirectional
— `build_offer`/`parse_answer` exist alongside the inbound `parse_offer`/
`build_answer`), `vowifi::control` (protocol extension), raw `UdpSocket` RTP
relay (already used by both the inbound path and `ims::call::run_call`).
**Storage**: none — an outbound dialog is in-memory, matching `ActiveCall`.
**Testing**: `cargo nextest` via `make test`. The control-protocol extension
is testable exactly like the existing inbound triad — real `TcpStream`/veth-
style round trips, no mocks (constitution Principle I). The UAC INVITE
building is unit-testable off the existing `InviteParts`-equivalent struct
the same way spec 024's registrar tests build requests by hand. Actually
placing a call over a live carrier network can only be verified against real
VoWiFi/VoLTE hardware — this project's precedent (`ims::call::run_call`,
already tested live against Airtel) is the model to follow, not a new one.
**Target Platform**: Linux, containerised (`docker/Dockerfile`), inside the
IMS/default network namespaces `supervise::orchestrate` already creates.
**Project Type**: Rust workspace — extends two existing per-process binaries
(`vowifi-ims-agent`/Agent A, `vowifi-sip-agent`/Agent B), no new process.
**Performance Goals**: not throughput-bound — one call at a time per line,
matching every other call path in this codebase.
**Constraints**:
- A second IMS-AKA registration for the same IMSI while one is already live
  tears the existing one down (documented elsewhere in this codebase, e.g.
  `docs/greptile-review-learnings.md`'s discussion of re-registration). The
  UAC origination **must** run over the session `ims::agent` already
  maintains — it cannot call `register_session` again, unlike
  `ims::call::run_call`, which is a standalone diagnostic that always starts
  fresh.
- `tools/count-unsafe.sh`: this phase touches no pjsua-safe/FFI code at all
  (confirmed in the prior pass — the carrier-facing leg is hand-rolled SIP,
  independent of pjsua) — `gsm-sip-bridge/src` stays at the required 0.
- One call per line at a time (existing design) — an outbound attempt makes
  that line's `ActiveCall`/`RegisteredSession` busy exactly like an inbound
  one does.
**Scale/Scope**: 1–8 VoWiFi/VoLTE lines, one call at a time per line.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-checked after Phase 1 design.*

| Principle | Assessment | Verdict |
|---|---|---|
| **I. Integration-First Testing** (NON-NEGOTIABLE) | The new `ControlMessage` variants are exercised the same way the existing inbound triad already is — real sockets, no mocks. UAC INVITE construction is unit-tested the same way `ims::call`'s existing (CLI-only) UAC builders would be if they had tests — pure functions over explicit parameters. Live-network verification follows `ims::call::run_call`'s existing precedent (tested live against Airtel per its own doc comment) rather than inventing a new verification story. | PASS |
| **II. Green-on-Commit** (NON-NEGOTIABLE) | Each commit lands the extension behind existing gates (`[outbound].enabled`, already shipped) before wiring it live, same discipline as passes 1–3. | PASS |
| **III. Frequent Atomic Commits** | Separable units: (1) `ControlMessage` extension + Agent B send-side, (2) `ims::call` builder generalization (visibility only, no behavior change — verifiable by the existing `run_call` continuing to work unmodified), (3) Agent A's UAC origination + dialog handling, (4) bridging the two legs, (5) live verification. | PASS |
| **IV. Makefile-Driven Build** | No new build operations. | PASS |
| **V. Simplicity & Refactorability** | The `ControlMessage` triad is a direct structural mirror of the existing inbound one, not a new abstraction — same reviewer who understands `IncomingCall`/`BridgeReady`/`BridgeFailed` already understands `PlaceCall`/`CallPlaced`/`CallFailed`. The UAC `DialogInfo` constructor sits next to the existing UAS one (`from_invite`), same struct, same file, not a parallel type. See Complexity Tracking for the one thing that isn't free. | PASS with justification |

**Post-Phase-1 re-check**: pending Phase 1 (data-model.md, contracts/). No
FFI/unsafe surface is touched, so `count-unsafe.sh`'s gate is unaffected by
construction, not by audit.

## Project Structure

### Documentation (this feature)

```text
specs/025-outbound-calling/
├── plan.md                          # This file (revision 4)
├── research.md                      # +R-009..R-011 for this phase
├── data-model.md                    # +OutboundDialog, PlaceCall triad
├── quickstart.md                    # unchanged
├── contracts/
│   ├── config-schema.md             # unchanged
│   ├── control-cmd-dial.md          # unchanged (CS path, done)
│   ├── line-command.md              # unchanged (superseded/likely unneeded, done)
│   ├── sip-dialout.md               # unchanged (US1/US3, done)
│   └── agent-outbound-protocol.md   # NEW: the PlaceCall/CallPlaced/CallFailed triad
└── checklists/
    └── requirements.md
```

### Source Code (repository root)

```text
gsm-sip-bridge/src/
├── vowifi/
│   ├── control.rs        # MODIFIED: PlaceCall/CallPlaced/CallFailed variants
│   └── mod.rs             # MODIFIED: run_outbound_listener (already shipped) sends
│                           #   PlaceCall to Agent A instead of/alongside ControlCmd::Dial
│                           #   when this line's carrier path is VoWiFi/VoLTE, not CS
├── ims/
│   ├── call.rs             # MODIFIED: InviteParts/build_invite/AckParts/build_ack/
│   │                       #   BYE-building made pub(crate); no behavioral change
│   └── agent.rs            # MODIFIED: UAC DialogInfo constructor (next to from_invite),
│                            #   PlaceCall handler in the control-message dispatch loop,
│                            #   originate-and-bridge path mirroring bridge_call's shape
├── volte/
│   └── carrier_agent.rs    # UNCHANGED — already calls ims::agent::serve_inbound,
│                            #   inherits the origination path for free
└── tests/
    ├── test_agent_outbound_protocol.rs   # NEW: real-socket ControlMessage round trip
    └── test_ims_uac_dialog.rs            # NEW: UAC DialogInfo/INVITE construction, pure
```

**Structure Decision**: extends the two existing agent processes and the
existing control protocol between them — no new process, no new top-level
module. Matches how the inbound direction is already built, deliberately:
this is the same shape, reversed, not a new architecture.

## Complexity Tracking

> **Fill ONLY if Constitution Check has violations that must be justified**

| Violation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|---------------------------------------|
| A second UAC-role `DialogInfo` constructor in `ims/agent.rs`, alongside the existing UAS-role `from_invite` | Answering an INVITE (UAS) and sending one (UAC) need different dialog state — the tag we generate vs. receive, whose CSeq space is whose, what a BYE's From/To look like — RFC 3261 makes these genuinely asymmetric, not two names for the same data. | A single `DialogInfo` covering both roles with optional/nullable fields was rejected: it would let a UAS-only field be read from a UAC dialog (or vice versa) with no compiler check, exactly the class of bug two focused constructors prevent. |
