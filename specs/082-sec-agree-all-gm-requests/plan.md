# Implementation Plan: Security-agreement headers on every Gm request

**Branch**: `082-sec-agree-all-gm-requests` | **Date**: 2026-10-07 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/082-sec-agree-all-gm-requests/spec.md`

## Summary

PR #99 put `Require`/`Proxy-Require: sec-agree` and `Security-Verify` on the
outbound INVITE and the SMS delivery-report MESSAGE, with the block
copy-pasted into both builders. This feature:

1. adds one helper, `sec_agree_headers(Option<&str>) -> String`, and uses it
   in every request builder (INVITE and MESSAGE switch to it; BYE, UPDATE,
   PRACK, ACK, CANCEL, SUBSCRIBE, OPTIONS and un-REGISTER gain it);
2. folds the separately stored `security_verify` into the Gm security state,
   so it exists exactly when the SA does;
3. tests the path from the registration state to each request, with a real
   loopback transport for the two requests that are sent from the session.

No behaviour changes when no Gm SA was negotiated: the helper returns an
empty string.

## Technical Context

**Language/Version**: Rust (workspace `rust-toolchain.toml`), crate `gsm-sip-bridge`
**Primary Dependencies**: none new
**Storage**: N/A
**Testing**: `cargo test` via `make test`; in-module unit tests plus a real
loopback TCP listener (the repo already does this in `agent/origination.rs`
tests); live runs on the Vodafone rig and the Jio Pi
**Target Platform**: Linux host and arm64 Pi
**Project Type**: single Rust workspace
**Performance Goals**: N/A (about 100 extra bytes per request)
**Constraints**: byte-identical requests when no Gm SA is negotiated; the echo
must remain the full `Security-Server` list (Jio 494s a partial one)
**Scale/Scope**: about 10 builders and their call sites, in `src/ims/`

## Constitution Check

| Principle | Status |
|---|---|
| I. Integration-first testing | Pass. Session-sent requests (OPTIONS, un-REGISTER) are tested over a real loopback socket, not a mock. Builders are pure functions tested directly. |
| II. Green-on-commit | Pass. Each task ends with `make format`, `make lint`, `make test`. |
| III. Atomic commits | Pass. Helper, state fold, and per-request wiring are separate commits. |
| IV. Makefile-driven | Pass. No new commands. |
| V. Simplicity | Pass. One helper and one small struct replacing a tuple. No new abstraction layer. |

Re-checked after design: unchanged.

## Design decisions

- **Helper location**: `ims/sip_client.rs` next to `build_message`, `pub(crate)`,
  imported by `call.rs` and `session.rs`.
- **State**: replace the `gm_state` tuple `(GmEndpoints, SaProposal,
  SecurityServerParams)` with a named struct `GmSa { endpoints, proposal,
  theirs, security_verify }`. `RegisteredSession::security_verify()` reads from
  it, and the separate field goes away. See [data-model.md](./data-model.md).
- **In-dialog requests** snapshot the echo into `DialogInfo` at call setup,
  exactly as they already snapshot `local_addr` and `use_tcp`. Renewal is
  deferred during a call, so this equals "read the current registration".
  See [research.md](./research.md) R2.
- **CANCEL / ACK**: all requests carry the full block (R3), which satisfies
  FR-002 and FR-003 because the INVITE they mirror carries it.
- **un-REGISTER** passes the block through its existing `extra_headers`.

## Project Structure

### Documentation

```text
specs/082-sec-agree-all-gm-requests/
├── spec.md
├── plan.md
├── research.md
├── data-model.md
├── quickstart.md
└── tasks.md
```

No `contracts/`: the feature adds no external interface; the wire format is
the contract and is covered by tests.

### Source Code

```text
gsm-sip-bridge/src/ims/
├── sip_client.rs       # helper; ByeRequest, UpdateRequest, OptionsRequest, MessageRequest
├── call.rs             # InviteParts, AckParts (ACK/BYE/PRACK), CancelParts
├── session.rs          # SubscribeParts, build_subscribe and its caller
├── mod.rs              # GmSa struct, RegisteredSession, unregister, send_gm_ping
└── agent/              # call sites: call.rs (DialogInfo), origination.rs, inbound.rs, mod.rs
```

**Structure Decision**: edit in place; no new modules.
