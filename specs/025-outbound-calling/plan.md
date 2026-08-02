# Implementation Plan: Outbound Calling

**Branch**: `025-outbound-calling` | **Date**: 2026-08-02 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/025-outbound-calling/spec.md`

## Summary

Today every call this bridge carries is initiated by the mobile network: it
answers, then calls out to a PBX or a registered phone. This feature reverses
that for the first time — a call originating on the SIP side (a PBX-sent
INVITE, or an INVITE from a phone registered in the bridge's own SIP server
mode) must cause the bridge to dial the given destination number out over
whichever SIM is idle, on any of the three carrier paths, and bridge the
resulting audio back to the SIP-side caller unmodified.

**Technical approach**: three genuinely new mechanisms, reusing everything
else. (1) A **dial-out leg** per carrier path — a `ATD<number>;` command for
circuit-switched (new; `answer_call`'s `ATA` is the closest existing analog),
and an originating IMS INVITE for VoWiFi/VoLTE (new; today's `ims::agent`
only ever answers). (2) An **inbound leg acceptance** on the SIP side — the
PBX's INVITE lands on the trunk account pjsua already registers (no new
listener), while a phone's INVITE lands on the lightweight hand-rolled
registrar, which cannot itself carry media and must redirect it to the
pjsua-hosted local account (`Account::local`, spec 024) the same way an
inbound call already rings that phone, just reversed. (3) A **cross-process
line-selection command channel** — the process that owns the SIP side today
only ever *receives* state from other line agents (`AgentReport` over the
control socket, one-directional, agent→daemon, on a 10 s heartbet cadence).
Placing a call needs the opposite direction, synchronously, well inside SIP
retransmit timers — a new, small per-agent command listener, documented and
justified under Complexity Tracking below.

## Technical Context

**Language/Version**: Rust 1.x, edition 2021 (see `rust-toolchain.toml`)
**Primary Dependencies**: existing only — `pjsua-safe`/`pjsua-sys` (call
placement, SDP/RTP), `tokio` (control-plane IPC), `serde`/`toml`
(configuration), the hand-rolled SIP primitives in `src/ims/sip_client.rs`
(redirect responses). No new crate dependencies anticipated.
**Storage**: in-memory only — an outbound request is a single in-flight call
attempt, not a persisted record.
**Testing**: `cargo nextest` via `make test`; integration tests in
`gsm-sip-bridge/tests/`, unit tests inline, real loopback sockets for the new
IPC surface (constitution Principle I — no mocks).
**Target Platform**: Linux (containerised; `docker/Dockerfile`), the same
single-board deployments this bridge already targets.
**Project Type**: Rust workspace — a daemon plus per-line agent processes.
**Performance Goals**: call setup must complete well inside a phone's/PBX's
INVITE retransmit timer (RFC 3261 T1 = 500 ms doubling); the existing
10 s `agent_report_interval_seconds` heartbeat is **too slow** to carry a
call-placement command, which is why (3) above is a distinct, synchronous
channel rather than piggybacking on it.
**Constraints**:
- `tools/count-unsafe.sh` (via `make lint`) fails the build on any `unsafe`
  in `gsm-sip-bridge/src`. The new dial-out and IPC code must be safe Rust,
  as the existing registrar is.
- The `pjsip-linked` feature is not compiled by `make test`/`make lint`/CI —
  only `docker/Dockerfile` builds it. The hand-rolled registrar's redirect
  response must therefore be provable without it, exactly as spec 024's
  REGISTER/OPTIONS handling is today.
- One call per SIM at a time (existing design, reaffirmed by this spec's
  Assumptions) — outbound and inbound calls contend for the same busy/idle
  state per line.
**Scale/Scope**: same SIM/line counts as today (1–8 CS modems, 1–8 VoWiFi/
VoLTE lines); one outbound attempt at a time per line; no queueing.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-checked after Phase 1 design.*

| Principle | Assessment | Verdict |
|---|---|---|
| **I. Integration-First Testing** (NON-NEGOTIABLE) | The new command channel is exercised over a real Unix/TCP loopback socket between two real processes, matching the existing control-socket tests. The registrar's redirect is exercised the same way spec 024 tested REGISTER — real `UdpSocket` client against the real handler. The `ATD` path is exercised against the existing `AtCommander` test harness (`make_commander`), which already fakes only the serial line, not the parsing/dispatch logic. | PASS |
| **II. Green-on-Commit** (NON-NEGOTIABLE) | Commit sequence (see tasks.md) adds each new capability behind the existing `owns_sip_side`/feature-flag gating before wiring it live, mirroring spec 024's ordering. | PASS |
| **III. Frequent Atomic Commits** | Each of the three new mechanisms (dial-out leg, inbound-leg acceptance, cross-process command channel) is its own commit sequence; config/validation is separated from behavior, as in spec 024. | PASS |
| **IV. Makefile-Driven Build** | No new build operations. | PASS |
| **V. Simplicity & Refactorability** | One new abstraction not present in the codebase today: a per-agent, synchronous, daemon→agent command listener (item 3 above). See Complexity Tracking — the existing one-directional `AgentReport` channel cannot meet the latency requirement, and piggybacking a call command on a 10 s heartbeat is not simplicity, it is a 10 s-average call-setup delay hidden in a data structure that was never meant to carry it. | PASS with justification |

**Post-Phase-1 re-check**: pending Phase 1 completion (data-model.md,
contracts/). No additional violations anticipated: the redirect strategy
reuses `Account::local` and `ims::sip_client` response-building already
proven in spec 024; the `ATD` addition is one new `AtCommander` method
alongside `answer_call`.

## Project Structure

### Documentation (this feature)

```text
specs/025-outbound-calling/
├── plan.md              # This file
├── spec.md              # Feature specification
├── research.md          # Phase 0 — design decisions and rejected alternatives
├── data-model.md        # Phase 1 — entities, state, validation rules
├── quickstart.md        # Phase 1 — operator walkthrough
├── contracts/
│   ├── config-schema.md     # The [outbound] TOML contract
│   ├── line-command.md      # The new daemon<->agent call-placement protocol
│   └── sip-dialout.md       # The on-the-wire SIP contract toward PBX/phones
└── checklists/
    └── requirements.md      # Spec quality checklist (already passing)
```

### Source Code (repository root)

```text
gsm-sip-bridge/
├── src/
│   ├── modules/
│   │   └── at_commander.rs      # MODIFIED: new dial()/ATD method alongside answer_call/ATA
│   ├── sip/
│   │   ├── mod.rs               # MODIFIED: accept a PBX-originated INVITE (UAS on the
│   │   │                        #   trunk account), start an outbound attempt
│   │   ├── server/
│   │   │   └── mod.rs           # MODIFIED: "INVITE" branch redirects (302) to the
│   │   │                        #   pjsua-hosted local account instead of refusing (403)
│   │   └── outbound.rs          # NEW: OutboundCallRequest, line selection, outcome
│   │       #                      mapping shared by both entry points above
│   ├── ims/
│   │   └── agent.rs             # MODIFIED: originate an INVITE toward the P-CSCF
│   │       #                      (new) alongside the existing inbound-only handling
│   ├── control/
│   │   ├── protocol.rs          # MODIFIED: new PlaceCall command/response variants
│   │   ├── line_server.rs       # NEW: the per-agent listener each line process runs
│   │       #                      so the SIP-owning process can command it
│   │   └── line_client.rs       # NEW: synchronous client used by sip::outbound
│   └── metrics/
│       └── mod.rs               # MODIFIED: gsm_sip_bridge_outbound_* counters
├── tests/
│   ├── test_outbound_line_command.rs   # NEW: real loopback socket, both processes real
│   ├── test_sip_server_redirect.rs     # NEW: registrar 302 response, wire-level
│   └── test_at_dial.rs                 # NEW: ATD dispatch against AtCommander harness
```

**Structure Decision**: extends the existing per-concern module layout
(`sip/`, `ims/`, `modules/`, `control/`, `metrics/`) rather than introducing
a new top-level crate or directory — this feature is additive glue between
subsystems that already exist, not a new subsystem of its own, matching
Principle V.

## Complexity Tracking

> **Fill ONLY if Constitution Check has violations that must be justified**

| Violation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|---------------------------------------|
| New daemon↔agent synchronous command channel (`control::line_server`/`line_client`) | An outbound call must be placed on a specific idle line, hosted in a different OS process (VoWiFi/VoLTE line agents run in their own network namespace), within a few hundred milliseconds of the INVITE that requested it. | Piggybacking on the existing `AgentReport` heartbeat (`agent_report_interval_seconds`, default 10 s) was rejected: it is one-directional (agent→daemon) and would make call setup wait for the next scheduled report, i.e. up to 10 s of silence on a channel a caller is actively listening to ring. A dedicated, low-latency, request/response channel is the smaller change once that latency requirement is taken as given. |
