# Implementation Plan: The long tail — smaller conformance gaps across SIP, SDP, and SMS

**Branch**: `045-long-tail-conformance` | **Date**: 2026-08-27 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `/specs/045-long-tail-conformance/spec.md`

## Summary

Batch 6, reduced scope: 9 findings fixed (MT-11, MT-12, MT-13, SDP-05,
SMS-02, SMS-03, SMS-04, CS-03, CS-04; MT-04 resolved by a confirming test
only). Four findings deferred to their own future features (MT-06,
SDP-04, SMS-05 — each needing a new subsystem; SMS-07 — needing verified
TS 23.038 Annex A table data this session can't responsibly transcribe
from memory) — see `research.md` Decision 1 and Decision 8. Every finding
here lands in exactly one or two files, following each file's own
established pattern.

## Technical Context

**Language/Version**: Rust (existing crate edition/toolchain, unchanged)
**Primary Dependencies**: None new
**Storage**: N/A
**Testing**: `cargo test` / `make test`, colocated `#[cfg(test)] mod tests`; real-hardware verification via `test/` docker rig + `siptest`
**Target Platform**: Linux (unchanged)
**Project Type**: Single Rust crate
**Performance Goals**: N/A — correctness fixes on already-hot paths, no new allocations beyond small per-call `Vec`s where noted
**Constraints**: Zero regression on every existing test for the ordinary case per finding; must pass `make format && make lint && make test` (whole workspace, `-D warnings`) before any commit, per `CLAUDE.md`
**Scale/Scope**: 9 findings fixed across `src/ims/session.rs`, `src/ims/sip_client.rs`, `src/ims/sms_pdu.rs`, `src/ims/agent/mod.rs`, `src/ims/agent/inbound.rs`, `src/sip/server/mod.rs`, `src/volte/sms.rs`, `src/modules/worker.rs`; 1 finding (MT-04) confirmed via test only; 0 new dependencies

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-checked after Phase 1 design — no changes, still passes.*

- **I. Integration-First Testing (NON-NEGOTIABLE)**: Every fix is a pure
  function or a small, directly-testable change (header builders, a DCS
  table lookup, a UDH IE reader, a quote-aware line splitter) exercised
  with real fixture strings/bytes, matching each file's existing test
  style. **PASS.**
- **II. Green-on-Commit (NON-NEGOTIABLE)**: `make format && make lint &&
  make test` before each commit. **PASS** (applied at implementation time).
- **III. Frequent Atomic Commits**: One commit per finding (or small
  cluster where they share one function), matching batches 1-5's pattern.
  **PASS.**
- **IV. Makefile-Driven Build**: No new tooling. **PASS.**
- **V. Simplicity & Refactorability**: Central constraint — see
  `research.md` Decision 1 (three findings deferred rather than
  force-fitting new subsystems into this batch), Decision 5 (MT-13 fixed
  at 2 transport-boundary call sites rather than ~39 builder call sites),
  and Decision 8 (SMS-07 deferred mid-implementation rather than shipping
  unverified table data). **PASS.**

No violations. Complexity Tracking table is empty.

## Project Structure

### Documentation (this feature)

```text
specs/045-long-tail-conformance/
├── plan.md
├── research.md
├── data-model.md
├── quickstart.md
├── contracts/
│   └── response-and-decode-contract.md
└── tasks.md
```

### Source Code (repository root)

```text
gsm-sip-bridge/
├── src/ims/
│   ├── session.rs      # extract_caller (MT-12), SubscribeParts/build_subscribe (MT-11)
│   ├── sip_client.rs   # annotate_via_received_rport + SipSink::peer_addr (MT-13)
│   ├── sms_pdu.rs      # TpduMessageType, DecodedRp::UnsupportedTpdu/Undecodable
│   │                   # (SMS-02/03), build_rp_error (SMS-03), Alphabet::from_dcs
│   └── agent/
│       ├── mod.rs      # handle_message's new match arms (SMS-02/03),
│       │               # subscribe_reg_event call sites (MT-11)
│       └── inbound.rs  # UAS_EXTRA_HEADERS per-line P-Access-Network-Info
│                       # (MT-11), Content-Type gate before parse_offer (SDP-05),
│                       # a confirming test for MT-04
├── src/sip/server/mod.rs   # serve()'s send_to call site (MT-13)
├── src/volte/sms.rs        # sweep_modem_storage's AT+CNMI (CS-03)
├── src/modules/worker.rs   # parse_sms_response's quote-aware split (CS-04)
├── docs/plans/mt-conformance-findings.md  # mark 9 [x], MT-04 [x] (test-only),
│                                           # MT-06/SDP-04/SMS-05/SMS-07 recorded deferred
└── RELEASE_NOTES.md   # one entry under ## Unreleased
```

**Structure Decision**: No new modules. Everything lands inside existing
files, following each file's own established pattern.

## Complexity Tracking

*(empty — no constitution violations)*
