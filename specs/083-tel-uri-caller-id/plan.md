# Implementation Plan: Caller identity from `tel:` URIs

**Branch**: `083-tel-uri-caller-id` | **Date**: 2026-10-10 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/083-tel-uri-caller-id/spec.md`

## Summary

`header_user_part()` (`ims/session.rs:597`) finds a caller by splitting on
the literal `sip:`, and reads only the first header line. Because of this:

- `tel:` identities (issue #104, T2) are lost;
- a `tel:` PAI on Jio/Vodafone is silently bypassed in favour of `From`;
- `sips:` and upper-case schemes fail;
- a host-only `sip:` URI reports the hostname as the caller.

`header_uri()`, which addresses SMS delivery reports, has the same
first-line-only gap.

This feature:

1. adds a pure parser module, `ims/identity.rs`, implementing the RFC 3261
   list and `name-addr`/`addr-spec` grammar, RFC 3325 §9.1 multi-value PAI,
   and RFC 3966 `tel:` numbers. It takes over `split_route_list` from
   `agent/call.rs` as the shared list splitter.
2. re-bases `extract_caller`, `extract_caller_name`, `header_display_name`
   and `header_uri` on it. Their signatures and callers don't change.
3. adds the FR-015 diagnostic as a pure function, logged once per new
   inbound call and once per SMS without a decoded sender.

## Technical Context

**Language/Version**: Rust (workspace `rust-toolchain.toml`), crate `gsm-sip-bridge`
**Primary Dependencies**: none new (a percent-decoder is about 15 lines; research R6)
**Storage**: N/A
**Testing**: `cargo test` via `make test`. The tests are table-style unit tests over requests
built by the real `SipRequest::try_parse`, with no mocks. Live runs on the Vodafone
rig and the Jio Pi.
**Target Platform**: Linux host and arm64 Pi
**Project Type**: single Rust workspace
**Performance Goals**: N/A (a few header values per request)
**Constraints**:
- caller strings must never carry `"<>,;` whitespace or CR/LF (FR-008);
- the live Jio/Vodafone delivery-report address must be byte-identical (FR-014);
- names must not mix headers (FR-011).
**Scale/Scope**: one new module (about 250 lines plus tests), edits in
`session.rs`, `agent/call.rs` and `agent/mod.rs`

## Constitution Check

| Principle | Status |
|---|---|
| I. Integration-first testing | Pass. Tests run real wire text through the real SIP parser into the real functions. Nothing is mocked. The FR-015 diagnostic is a pure function, so no log-capture double is needed. |
| II. Green-on-commit | Pass. Each task ends with `make format`, `make lint` and `make test`. |
| III. Atomic commits | Pass. The work is split into separate commits: splitter move, parser, caller rebase, `header_uri` rebase, diagnostic. |
| IV. Makefile-driven | Pass. No new commands. |
| V. Simplicity | Pass, with one new module. It is justified in research R8: it gives one home to the RFC 3261 list grammar that two subsystems already need. There is no new dependency and no trait or abstraction layer. |

Re-checked after design: unchanged.

## Design decisions

- **Grammar** (research R1, R2): values come from every header line, split
  outside quotes and brackets. An unbracketed `;` is cut for `From` but
  kept for PAI, because RFC 3325 defines no PAI header params.
- **Number** (R3, R4):
  - `sip`/`sips` need an `@`, and userinfo ends at the first `@`.
  - `tel` stops at the first `;`. Local numbers are reported as dialled.
  - Visual separators are removed only from phone numbers.
- **Safety** (R6): percent-decode, then apply an allow-list
  `[A-Za-z0-9+*#\-._~]`. Anything else means no number.
- **Preference** (R5): the number comes from the PAI `tel` value first; the
  delivery report goes to the PAI `sip`/`sips` value first.
- **Diagnostic** (R7): `extract_caller` stays pure. Its INVITE callers run up
  to three times per request, so the warning is logged at one point per
  request instead.
- **Contract**: [contracts/identity-parsing.md](./contracts/identity-parsing.md)
  is the test oracle. Each row is a test.

## Project Structure

### Documentation

```text
specs/083-tel-uri-caller-id/
├── spec.md
├── plan.md
├── research.md
├── data-model.md
├── quickstart.md
├── contracts/identity-parsing.md
├── checklists/requirements.md
└── tasks.md
```

### Source Code

```text
gsm-sip-bridge/src/ims/
├── identity.rs     # NEW: split_header_values, parse_name_addr, uri_number,
│                   #      percent_decode, header_identity, header_uri_values + tests
├── mod.rs          # pub(crate) mod identity;
├── session.rs      # extract_caller / extract_caller_name / header_display_name /
│                   # header_uri become wrappers; header_user_part removed;
│                   # unresolved_caller_diagnostic added
└── agent/
    ├── call.rs     # split_route_list → identity::split_header_values (tests move)
    └── mod.rs      # two diagnostic log points; CNAP test fixed; header_uri tests added
```

**Structure Decision**: one new pure module under `src/ims/`. Everything else
is edited in place.

## Complexity Tracking

| Violation | Why Needed | Simpler Alternative Rejected Because |
|---|---|---|
| New module `ims/identity.rs` | It is a self-contained grammar with about 50 contract tests, shared by Record-Route and identity parsing. | Keeping it in `session.rs` would make `agent/call.rs` import a list splitter from a module that doesn't own it, and bury grammar tests among dialog tests. |
