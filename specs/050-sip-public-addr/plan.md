# Implementation Plan: Advertise a configured public address in SIP/SDP

**Branch**: `050-sip-public-addr` | **Date**: 2026-09-13 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/050-sip-public-addr/spec.md`

## Summary

Neither `pjsua-safe`'s SIP transport config nor any account's RTP media
config ever sets PJSIP's `public_addr`, so PJSIP falls back to
`pj_gethostip()`'s default-route candidate — a private, host-internal
address — for both the Contact/Via headers and the SDP `c=` line. A remote
SIP client reachable only over a routed network (Tailscale, per GitHub
issue #77) gets told to send RTP to an address it cannot route to: complete
two-way silence, while local calls work fine.

The fix adds a new `[sip].public_addr` config field (IP literal or
hostname, resolved at most once — at config-load time — to an `IpAddr`,
never in the call path) that flows into `EndpointConfig`, applied to the SIP
transport's `tp_cfg.public_addr` in `Endpoint::create`. `Account::register`
and `Account::local` already receive a `&Endpoint` reference (currently
unused, hence `_endpoint`); they read the resolved address straight off it
and apply it to `acc_cfg.rtp_cfg.public_addr`, and each `Account` caches its
own copy at construction. That cached copy is what lets
`Account::set_identity` — which has no `Endpoint` reference, only `&self` —
re-apply the address on every inbound call in SIP-server mode, which is the
sharp edge: `set_identity` already rebuilds the account config from PJSIP
defaults today, silently dropping any media config not re-applied there.
Scope is limited to
`pjsua-safe`'s PBX/softphone-facing transport (used by `src/vowifi/mod.rs`
"Agent B" and the legacy `src/sip/mod.rs`); the carrier-facing IMS/Gm leg
(`src/ims/agent`) and the internal veth link are untouched.

## Technical Context

**Language/Version**: Rust 1.94.0 (workspace `rust-toolchain.toml`, edition 2021)
**Primary Dependencies**: `pjsua-sys` (raw PJSIP FFI bindings), `toml`/`serde` (config), `tracing` (logging) — no new external crate; hostname resolution uses `std::net::ToSocketAddrs` (stdlib only, matching YAGNI/Simplicity)
**Storage**: N/A — one resolved `Option<std::net::IpAddr>` held in process memory (`EndpointConfig`/`AccountConfig`/`Account`), sourced from the TOML config file
**Testing**: `cargo test` across the workspace; `pjsua-safe`'s existing conformance tests run twice — once in stub mode (default) and once with `--features pjsip-linked` against the real, compiled PJSIP — per the project's Integration-First Testing principle; config-layer tests live in `gsm-sip-bridge/src/config/mod.rs`'s existing `try_parse`-based test module
**Target Platform**: Linux (Docker, `network_mode: host`), the bridge's only deployment target
**Project Type**: Single Cargo workspace, multiple member crates (`pjsua-safe` is a thin safe wrapper around PJSIP; `gsm-sip-bridge` is the application binary that owns config)
**Performance Goals**: Zero added cost in the call-answering path — hostname resolution (if any) happens exactly once, at process startup, never per call (FR-008, SC-006)
**Constraints**: Must not reintroduce the blocking-DNS-in-call-path bug class `2a04eae` already fixed; must not touch the carrier-facing IMS/Gm leg or the internal veth RTP-destination logic (FR-007)
**Scale/Scope**: Three call sites in `pjsua-safe/src/account.rs` (`register`, `local`, `set_identity`) plus one in `pjsua-safe/src/endpoint.rs` (`Endpoint::create`'s transport setup); one new `[sip]` config field end to end (`raw.rs` → `mod.rs` → `build.rs`)

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

- **Integration-First Testing**: Satisfied by design — `pjsua-safe`'s
  existing tests already exercise both the stub and `pjsip-linked` (real
  PJSIP) builds; this feature adds cases to that same suite rather than
  introducing new mocks. The one piece of test-only fakery this feature
  needs — a fake/unreachable hostname to exercise the "fails to resolve"
  path (FR-006) — is a real DNS query against a real (if deliberately
  invalid) name, not a mocked resolver, so no mock-justification comment is
  needed.
- **Green-on-Commit**: No special risk — this is additive config plus three
  well-isolated PJSIP call sites; existing tests continue to pass unchanged
  except where fixture structs (e.g. `EndpointConfig` literals in
  `pjsua-safe/tests/*.rs`) need the new field added, which is a mechanical,
  compile-enforced change.
- **Frequent Atomic Commits**: Natural break points exist: (1) `[sip]`
  config plumbing + validation, (2) `pjsua-safe` `EndpointConfig`/
  `AccountConfig`/`Account` changes, (3) wiring the resolved config value
  from `gsm-sip-bridge` into `Endpoint::create`/`Account::register`/
  `Account::local` call sites in `src/vowifi/mod.rs` and `src/sip/mod.rs`.
- **Makefile-Driven Build**: No new build steps; `make format`/`make lint`/
  `make test` cover everything this feature touches.
- **Simplicity & Refactorability**: Stdlib-only hostname resolution, one new
  config field, no new abstraction layer — the existing `_endpoint: &Endpoint`
  parameter `Account::register`/`Account::local` already receive (currently
  unused) is reused to hand the resolved address to `Account`, avoiding a
  new plumbing mechanism.

**Result**: PASS, no violations to justify.

## Project Structure

### Documentation (this feature)

```text
specs/050-sip-public-addr/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md         # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/           # Phase 1 output
│   └── sip-public-addr-contract.md
└── tasks.md             # Phase 2 output (/speckit.tasks — not this command)
```

### Source Code (repository root)

```text
pjsua-safe/
├── src/
│   ├── endpoint.rs       # EndpointConfig gains `public_addr`; Endpoint::create
│   │                     # applies it to the SIP transport's tp_cfg.public_addr,
│   │                     # and Endpoint exposes it for Account to read
│   └── account.rs        # Account gains a cached `public_addr` field, read from
│                          # the `&Endpoint` register/local already receive;
│                          # register/local/set_identity all apply it to
│                          # acc_cfg.rtp_cfg.public_addr (AccountConfig itself
│                          # is unchanged — the address comes from Endpoint,
│                          # not from the caller's per-account config)
└── tests/
    ├── smoke.rs           # Existing EndpointConfig/AccountConfig literals need
    │                      # the new field; add coverage for public_addr applied
    └── (new or extended)  # Coverage for set_identity retaining public_addr

gsm-sip-bridge/
└── src/
    ├── config/
    │   ├── raw.rs         # RawSip gains `public_addr: Option<String>`
    │   ├── mod.rs         # SipConfig gains `public_addr: Option<std::net::IpAddr>`
    │   └── build.rs       # build_sip: parse/resolve/validate the new field
    ├── vowifi/mod.rs      # Agent B: passes config.sip.public_addr into
    │                      # EndpointConfig and AccountConfig at construction
    └── sip/mod.rs         # Legacy circuit-switched bridge: same wiring
```

**Structure Decision**: No new crates, modules, or directories — this is a
narrow, additive change to two existing crates (`pjsua-safe`'s PJSIP wrapper,
`gsm-sip-bridge`'s config layer and its two `Endpoint`/`Account`
construction sites) plus their existing test suites. Matches the workspace's
established single-project-per-crate layout; no alternative structure was
considered.

## Complexity Tracking

*No Constitution Check violations — this section is not applicable.*
