# Implementation Plan: Complete the media contract on the relay legs

**Branch**: `044-complete-media-contract` | **Date**: 2026-08-27 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `/specs/044-complete-media-contract/spec.md`

## Summary

Batch 5 (reduced scope by explicit user decision): fix RTP-03 (wrong DTMF
payload type on the pass-through relay), RTP-04 (no SSRC-change
visibility), and SDP-06's `ptime` half (offer's packetization ignored).
RTP-01 (RTCP) and SDP-06's `a=rtcp` half are deferred to their own future
feature — real RTCP needs call-wide state (send-side octet counts, an
exposed/stable SSRC, a per-call timer with live socket access, a
synchronous teardown hook) that exists nowhere in this codebase today,
across all three relay call sites and both relay implementations, a
materially larger and riskier change than the rest of this batch.

RTP-03/RTP-04 both land inside `agent::veth::forward` (the pass-through
relay) and `transcode::relay_direction` (the transcoding relay, RTP-04
only — RTP-03 doesn't apply there, since batch 1's RTP-02 already gave the
transcoding path its own correct per-leg DTMF payload-type handling).
SDP-06's `ptime` half turned out, on investigation, to need no code
change at all: `ims::sdp` gains a confirming test instead (research.md
Decision 4 — echoing the offer's `ptime` would have made the answer state
a packetization this bridge doesn't actually use).

## Technical Context

**Language/Version**: Rust (existing crate edition/toolchain, unchanged)
**Primary Dependencies**: None new — reuses `crate::ims::rtp::parse_packet` (already extracts `ssrc`/`payload_type` correctly, including past header extensions/CSRC lists)
**Storage**: N/A (per-relay-thread local state only)
**Testing**: `cargo test` / `make test` (colocated `#[cfg(test)] mod tests`, matching `veth.rs`/`transcode.rs`/`sdp.rs`'s existing patterns); real-hardware verification via `test/` docker rig + `siptest`
**Target Platform**: Linux (unchanged)
**Project Type**: Single Rust crate (existing structure, no new crates/binaries)
**Performance Goals**: N/A — the pass-through relay's per-packet cost gains one `rtp::parse_packet` call (already O(1), already used identically in the transcoding relay) plus, on a DTMF packet only, a single byte rewrite
**Constraints**: Must not change pass-through relay behavior for the ordinary case (matching DTMF PTs, no SSRC change, no `ptime` in the offer) — zero regression on existing relay/SDP test coverage; must pass `make format && make lint && make test` (whole workspace, `-D warnings`) before any commit, per `CLAUDE.md`
**Scale/Scope**: 3 findings, 3 source files (`agent/veth.rs`, `transcode.rs`, `sdp.rs`) plus their call sites (4 `spawn_relay` call sites across `agent/inbound.rs`/`agent/veth.rs`/`agent/origination.rs`), 0 new dependencies

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-checked after Phase 1 design — no changes, still passes.*

- **I. Integration-First Testing (NON-NEGOTIABLE)**: `forward`'s existing
  test (`relay_rtp_forwards_packets_in_both_directions_until_stopped`,
  `veth.rs`) already drives real loopback sockets — the DTMF-relabel and
  SSRC-log additions are exercised the same way, with real RTP packets
  built via `rtp::build_packet`, not mocked. `sdp.rs`'s ptime-honesty
  confirmation follows its existing pure-function, real-string-fixture
  test style. **PASS.**
- **II. Green-on-Commit (NON-NEGOTIABLE)**: `make format && make lint &&
  make test` runs before each commit, whole workspace. **PASS** (gate
  applied at implementation time).
- **III. Frequent Atomic Commits**: One commit per finding (RTP-03,
  RTP-04, SDP-06), matching batches 1-4's pattern. **PASS.**
- **IV. Makefile-Driven Build**: No new build steps or tooling. **PASS.**
- **V. Simplicity & Refactorability**: This is the plan's central
  constraint — see `research.md` Decision 1 (RTCP deferral) and Decision 3
  (SSRC handling is observability-only, no enforcement/dropping added, no
  new generic "stream continuity" abstraction). **PASS.**

No violations. Complexity Tracking table is empty.

## Project Structure

### Documentation (this feature)

```text
specs/044-complete-media-contract/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/
│   └── relay-behavior-contract.md
└── tasks.md              # Phase 2 output (/speckit.tasks)
```

### Source Code (repository root)

```text
gsm-sip-bridge/
├── src/ims/
│   ├── agent/veth.rs   # forward()/relay_rtp(): DTMF payload-type relabel
│   │                   # (RTP-03) + SSRC-change log (RTP-04, pass-through
│   │                   # half) — plus tests
│   ├── transcode.rs    # relay_direction(): SSRC-change log (RTP-04,
│   │                   # transcoding half) — plus tests
│   ├── sdp.rs           # No new field — a confirming test that the
│   │                    # answer's ptime stays fixed regardless of the
│   │                    # offer's own (research.md Decision 4)
│   └── agent/
│       ├── inbound.rs      # spawn_relay call site: thread DTMF PTs through
│       └── origination.rs # 3 spawn_relay call sites: same
├── docs/plans/mt-conformance-findings.md  # mark RTP-03/04/SDP-06(ptime) [x];
│                                           # RTP-01/SDP-06(a=rtcp) recorded
│                                           # as deferred, not done
└── RELEASE_NOTES.md   # one entry under ## Unreleased
```

**Structure Decision**: No new modules or files. Everything lands inside
existing files, following each file's own established pattern (small pure
helpers/fields colocated with what they extend, tests in the existing
`#[cfg(test)] mod tests`).

## Complexity Tracking

*(empty — no constitution violations)*
