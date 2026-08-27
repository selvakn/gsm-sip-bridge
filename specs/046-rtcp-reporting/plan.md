# Implementation Plan: RTCP reporting on the carrier media leg

**Branch**: `046-rtcp-reporting` | **Date**: 2026-08-27 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/046-rtcp-reporting/spec.md`

## Summary

Close **RTP-01** — the bridge declares `b=RS:800`/`b=RR:2400` on every SDP
answer and sends no RTCP at all — plus **SDP-06's `a=rtcp` half**, both
deferred out of batch 5. Scoped by clarification to the carrier-facing leg
of answered calls only.

The approach is one new module (`ims::rtcp`) and one thread per call that
owns an RTCP socket: it sends compound sender reports on a cadence derived
from the declared bandwidth, reads the far end's receiver reports, and
sends a BYE on its way out. Send-side accounting is added as atomics
alongside the existing `MediaMeter` counters; receive-side quality reuses
`media_stats::ReceiveTracker` unchanged. Figures reach both the existing
end-of-call log line and, via a new `ObservedEvent` variant, Prometheus.

Phase 0 turned up two findings that make this **smaller than the spec
assumed** — see Technical Context.

## Technical Context

**Language/Version**: Rust (2021 edition), workspace at `gsm-sip-bridge/`
**Primary Dependencies**: none new. `rand` (already used by
`transcode::RtpSender`) for interval randomisation; `std::net::UdpSocket`
and `std::thread` for the RTCP endpoint, matching every other media path
here.
**Storage**: N/A — all per-call state, discarded at teardown. Only derived
metrics outlive a call.
**Testing**: `cargo test` via `make test`; unit tests in-module per this
codebase's convention, plus the workspace integration tests under `tests/`.
**Target Platform**: Linux (aarch64 on the deployment Pi, x86-64 locally);
Agent A runs inside the `ims` network namespace.
**Project Type**: Single Rust workspace — a long-running daemon plus CLIs.
**Performance Goals**: One added thread and one added socket per call.
Per-packet cost on the relay hot path is three atomic stores plus one
uncontended mutex acquisition; the report path runs about once per second.
**Constraints**: `make lint` is clippy `-D warnings` across the whole
workspace *including test targets*; teardown must not gain any new work
(FR-020/SC-007); the answer SDP must stay byte-identical on the common
path (C-1.1).
**Scale/Scope**: One call at a time per line — the one-call-per-`ActiveCall`
architecture established in batch 3. Sessions are two-party by
construction, which is what justifies skipping RFC 3550's multiparty
machinery.

### Two findings that reduce scope

Recorded here because both contradict assumptions the spec inherited from
batch 5's deferral note. Full reasoning in `research.md`.

1. **No synchronous teardown hook is needed** (Decision 2). The spec's US5
   says the RTCP BYE "needs teardown to become synchronous, which is a
   change to how every call ends." It does not. All three teardown paths do
   one atomic store and return; the RTCP thread owns its own socket and can
   send the BYE when it observes `stop`, after the caller has already
   returned. FR-018/019/020 are satisfied without touching a single
   teardown call site. **US5 is therefore no longer the risky item the spec
   treats it as** — it is among the cheapest, and can land with the thread
   US1 needs anyway.
2. **No per-call timer is needed** (Decision 3). The RTCP thread's socket
   read timeout is the clock. Checking an interval deadline on a wakeup
   that already happens costs one comparison.

Together with the clarification's scope cut (carrier leg only), three of
batch 5's four stated blockers dissolve and the fourth — send-side octet
counts — is a direct extension of an existing pattern.

### Two decisions Phase 0 resolved

- **RTCP port** (Decision 1): three tiers — RTP+1 by convention (answer
  unchanged, works with every peer), else any ephemeral port declared via
  `a=rtcp` (RFC 3605), else no RTCP with a warning and a metric. Tiering
  takes the safer half of each mechanism: the convention needs no RFC 3605
  support from the carrier, and the declaration is better than nothing when
  the convention is unavailable.
- **Source validation** (Decision 7): `recv_from` plus an IP check, *not*
  `connect()`. Connecting would filter on address **and port**, silently
  dropping a peer that sends RTCP from an asymmetric source port — exactly
  the "silence we cannot explain" failure this feature exists to end.

No `NEEDS CLARIFICATION` remains.

## Constitution Check

*GATE: passed before Phase 0; re-checked after Phase 1 design — see below.*

| Principle | Assessment |
| --- | --- |
| **I. Integration-First Testing** (NON-NEGOTIABLE) | **Pass.** No mocks introduced. Packet builders/parsers, the schedule, and source validation are pure and tested directly. Relay-path accounting is tested through the real relay functions, extending batch 5's existing tests. The two things that need a live socket (tier selection, the full thread loop) are honestly recorded in `quickstart.md` as hardware-only — the same disclosure batches 3, 5 and 6 made rather than mocking a socket to manufacture coverage. |
| **II. Green-on-Commit** (NON-NEGOTIABLE) | **Pass.** `make format && make lint && make test` before every commit, per CLAUDE.md's mandatory checklist. |
| **III. Frequent Atomic Commits** | **Pass.** The phase breakdown below is written as commit boundaries — each is one logical change that compiles and tests green on its own. |
| **IV. Makefile-Driven Build** | **Pass.** No new tooling, no new targets. |
| **V. Simplicity & Refactorability** | **Pass, with one item to watch** — see below. |

**Principle V in detail.** The plan actively declines complexity at four
points, each recorded with reasoning: no RFC 3550 §6.3 member counting or
reconsideration (FR-004b — two-party by construction); no async runtime for
one socket; no reimplementation of loss/jitter tracking (reuses the tested
`ReceiveTracker`); no port-range configuration (a bounded retry needs
none). The one genuinely new abstraction is the `ims::rtcp` module itself,
which is not premature — it is the feature.

**The item to watch** is parameter growth (Decision 10). `transcode::
relay_direction` and `veth::forward` both already carry
`#[allow(clippy::too_many_arguments)]` at ten and seven parameters. The
plan bundles the new handles into one struct rather than deepening those
suppressions — which is the Principle V-aligned choice, but it means this
feature adds a small type whose only job is to carry three fields. That is
the right trade against a `-D warnings` lint gate that CLAUDE.md records as
having broken commits here before.

**No Complexity Tracking entries** — no violation requires justification.

### Post-Phase 1 re-check

Re-evaluated after `data-model.md` and the contract. Still passing, and
two design outcomes strengthen it:

- FR-008b (bounded metric cardinality) needs **no new mechanism** —
  `ObservedEvent`'s closed-enum rule already enforces it architecturally
  (Decision 8). Nothing was added to satisfy it.
- `ReceiveQuality` turned out to need **no new type at all**;
  `ReceiveTracker` is reused as-is behind the same `Arc<Mutex<...>>`
  wrapper `ims/call.rs` already uses. One fewer entity than the spec's Key
  Entities implied.

Net new types: `SendAccounting`, `FarEndQuality`, `RtcpEndpoint`,
`ReportSchedule`, one `SdpOffer` field, one `ObservedEvent` variant, one
parameter-bundle struct. Every one traces to a requirement.

## Project Structure

### Documentation (this feature)

```text
specs/046-rtcp-reporting/
├── plan.md              # This file
├── spec.md              # Feature spec (5 clarifications integrated)
├── research.md          # Phase 0 — 10 decisions
├── data-model.md        # Phase 1 — entities, validation, relationships
├── quickstart.md        # Phase 1 — verification plan
├── contracts/
│   └── rtcp-wire-contract.md    # Phase 1 — the external wire contract
├── checklists/
│   └── requirements.md  # Spec quality checklist (16/16)
└── tasks.md             # Phase 2 — NOT created by /speckit-plan
```

### Source Code (repository root)

```text
gsm-sip-bridge/
├── src/
│   ├── ims/
│   │   ├── rtcp.rs              # NEW — packets, schedule, the per-call thread
│   │   ├── mod.rs               # + pub mod rtcp
│   │   ├── sdp.rs               # + SdpOffer.rtcp; a=rtcp in the answer
│   │   ├── media_stats.rs       # + SendAccounting
│   │   ├── rtp.rs               # (unchanged — SsrcTracker reused as-is)
│   │   ├── transcode.rs         # RtpSender publishes SSRC; send accounting
│   │   └── agent/
│   │       ├── inbound.rs       # the one call site: bind, spawn, wire up
│   │       ├── veth.rs          # pass-through: publish SSRC + accounting
│   │       └── call.rs          # ActiveCall holds handles; end-of-call report
│   ├── control/protocol.rs      # + ObservedEvent::MediaQuality
│   └── metrics/
│       ├── mod.rs               # + metric definitions
│       └── ingest.rs            # + apply_event arm
└── tests/
    └── test_metric_renames.rs   # + coverage for the new names

docs/plans/mt-conformance-findings.md   # RTP-01 / SDP-06 status at the end
```

**Structure Decision**: Existing layout, one new module. `ims::rtcp` sits
beside `ims::rtp` and `ims::media_stats` because it is peer to both — it
consumes what they produce. The relays are modified in place rather than
wrapped; the single answer-path call site (`agent/inbound.rs`) is where
everything is assembled, exactly as `MediaMeter` is today.

## Implementation Phases

Ordered as commit boundaries. Each compiles and tests green alone.

**Phase A — the wire format, standalone.** `ims::rtcp` with SR/RR/SDES/BYE
building and parsing, the RTT derivation, and `ReportSchedule`. Pure, fully
unit-testable, touches nothing else. The largest single body of new code
and the one with zero integration risk.

**Phase B — send accounting.** `SendAccounting` in `media_stats`, published
from `transcode::RtpSender` and `veth::forward`. Extends batch 5's relay
tests. Nothing consumes it yet, so the relays' behaviour is provably
unchanged.

**Phase C — the endpoint.** `SdpOffer.rtcp` parsing, the three-tier bind,
and the conditional `a=rtcp` in the answer. Includes the byte-identical
regression assertion (C-1.1) and the FR-017 case (C-1.2/C-1.3) — the
contradiction the clarification session caught.

**Phase D — the thread.** Wire A+B+C together at `agent/inbound.rs`: spawn
the RTCP thread, feed it the tracker and accounting, send reports, read
reports, send the BYE on stop. This is where US1, US2, US4 and US5 all
become real.

**Phase E — reporting.** End-of-call log fields, the `ObservedEvent`
variant, the ingest arm, the metric definitions, and the rename-test
coverage. One commit, because it spans a serialized wire protocol.

**Phase F — verification.** Hardware round per `quickstart.md`, then update
`docs/plans/mt-conformance-findings.md` — including FR-023a's residue, so
the doc does not read as if RTP-01 were closed everywhere.

**Sequencing note.** US5 (the RTCP BYE) is P3 by value but lands in Phase D
alongside P1 work, because Decision 2 made it a few lines in a thread that
has to exist anyway. Deferring it would cost more than including it.

## Risks

| Risk | Mitigation |
| --- | --- |
| The answer SDP changes and a carrier rejects the call — the exact failure mode `sdp.rs` already documents (omitted `telephone-event` → `503` 2ms after ACK) | Tier 1 keeps the answer byte-identical; a test pins that. `a=rtcp` only appears on the tier-2 path, which should be rare. |
| The carrier never sends RR, so US2 delivers nothing | Unknowable in advance; both carriers declare RTCP bandwidth in their own SDP, so they should. `quickstart.md` step 4 makes "no RRs arrived" an explicit recorded finding rather than a silent disappointment. |
| A new thread and socket per call destabilises the answer path | Phase D is the integration point and the hardware round's first check is an ordinary successful call. Tier 3 exists so a socket failure degrades instead of failing the call. |
| The pass-through relay's SSRC observation stays untested on real hardware | Likely — real calls here negotiate AMR-WB and take the transcoding path. Recorded in `quickstart.md` rather than papered over; consistent with batches 5 and 6. |

## Complexity Tracking

No Constitution Check violations. Table intentionally empty.
