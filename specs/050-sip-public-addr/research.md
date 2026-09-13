# Research: Advertise a configured public address in SIP/SDP

No `[NEEDS CLARIFICATION]` markers remain in `spec.md` — the one open
question (hostname resolution timing) was resolved in the `/speckit-clarify`
session and is recorded in `spec.md`'s `## Clarifications`. This document
records the *implementation-level* decisions needed to execute FR-001
through FR-008, each grounded in the current tree.

## Decision 1: Where the resolved address is stored and threaded through

**Decision**: Add `public_addr: Option<std::net::IpAddr>` to
`pjsua-safe::EndpointConfig`. `Endpoint::create` applies it to the SIP
transport's `tp_cfg.public_addr` (`pjsua-safe/src/endpoint.rs:223-225`,
where `tp_cfg` is currently zeroed/defaulted with only `port` set) and
retains a copy for `Account::register`/`Account::local` to read via the
`&Endpoint` reference they already accept (currently unused, hence the
`_endpoint` parameter name — `pjsua-safe/src/account.rs:30,108`). Each
`Account` caches its own copy of the resolved address at construction time.
`AccountConfig` (`pjsua-safe/src/account.rs:5-10`) is **not** changed — the
address is a property of the transport/endpoint, not of an individual
account's credentials.

**Rationale**: A single `[sip].public_addr` setting applies bridge-wide
(spec Assumptions), matching how `[sip].local_port`/`[sip].transport`
already apply to the one `Endpoint` each process creates. Reading it off the
`Endpoint` reference `register`/`local` already receive needs no new
plumbing mechanism — just de-underscoring an existing, currently-unused
parameter. `Account::set_identity` (`pjsua-safe/src/account.rs:189`) has no
`Endpoint` reference at all (only `&self`), so the address has to be cached
on `Account` itself at construction regardless of where it originates — this
is what makes FR-004 (survives the `set_identity` rebuild) possible.

**Alternatives considered**:
- *Add `public_addr` to `AccountConfig` instead*: rejected — `AccountConfig`
  is per-account (username/password/server), and `Account::local` doesn't
  even take one (it synthesizes a minimal stand-in,
  `pjsua-safe/src/account.rs:112-118`); threading a bridge-wide value
  through a per-account struct that one of the two constructors doesn't use
  is more plumbing for no behavioral difference, since every account in this
  process shares the same `Endpoint`/transport.
- *Read from a global/env var directly inside `pjsua-safe`* (the community
  patch's approach): rejected per spec User Story 2 — bypasses the project's
  config system entirely and is untestable via the existing config-parsing
  suite.

## Decision 2: Hostname resolution mechanism

**Decision**: Use `std::net::ToSocketAddrs` (`(host, 0).to_socket_addrs()`)
at config-build time (`gsm-sip-bridge/src/config/build.rs`'s `build_sip`),
taking the first resolved address. If the raw string already parses as
`std::net::IpAddr`, skip resolution entirely (no syscall for the common
case — an operator pointing this at a Tailscale IP literal, which is the
issue's actual reported scenario).

**Rationale**: Stdlib-only, zero new dependencies (Simplicity/YAGNI
principle). `to_socket_addrs()` performs a synchronous OS-level resolution
(the same mechanism `TcpStream::connect` uses internally for a hostname),
called exactly once, inside `build_sip` — which today is a fully synchronous
function with no I/O of its own, invoked once per process startup from
`load_config` (`gsm-sip-bridge/src/config/mod.rs:1143-1181`). Doing the one
resolution here — not in `pjsua-safe`, and not lazily on first use —
guarantees FR-008's "at most once, at startup" and rules out any path by
which a later account rebuild could trigger DNS I/O.

**Alternatives considered**:
- *Resolve inside `pjsua-safe::Endpoint::create`*: rejected — `Endpoint` is
  constructed once per process already (fine for "once"), but this would
  put a stdlib-resolution dependency inside a crate whose whole purpose is
  a thin, side-effect-minimal PJSIP wrapper, and would duplicate validation
  logic that the config layer already owns for every other `[sip]` field.
  Keeping resolution in `gsm-sip-bridge`'s config layer means `pjsua-safe`
  only ever sees an already-resolved `IpAddr` — simpler contract, and
  consistent with `SipConfig` already being the single place raw strings
  become validated, typed values (`transport`, `tls_verify` follow the same
  pattern in `build_sip`).
- *A dedicated async resolver crate (`trust-dns-resolver` etc.)*: rejected —
  this bridge has no async runtime dependency for config loading, and a
  one-time, startup-only, blocking resolution has no async benefit here.

## Decision 3: Validation & error reporting

**Decision**: In `build_sip`, after the existing `transport`/`tls_verify`
validation blocks, resolve `raw.public_addr` (new field on `RawSip`,
`Option<String>`, default `None`) exactly as Decision 2 describes. A
syntactically invalid value that also fails DNS resolution, or a value with
no resolvable address, returns `BridgeError::Config(...)` — the same error
type and message style every other `[sip]` field already uses (e.g. the
`transport`/`tls_verify` "must be one of..." messages,
`gsm-sip-bridge/src/config/build.rs:127-146`), naming the field
(`sip.public_addr`) and the offending value.

**Rationale**: Matches FR-006 exactly and keeps this field indistinguishable
from every other `[sip]` field in how it fails — an operator who has
already seen a `sip.transport must be one of...` error from a typo
recognizes the same shape here.

**Alternatives considered**:
- *Warn and fall back to unset on a bad value*: explicitly rejected by
  FR-006/SC-005 — the entire point is failing fast instead of shipping a
  deployment with silent broken audio and no diagnostic.

## Decision 4: What "hostname" validation actually rejects vs. accepts

**Decision**: No new syntax restriction beyond what `to_socket_addrs()`
itself accepts (any string that's a valid hostname or IP literal per the
platform resolver) or rejects (empty string, malformed literal, NXDOMAIN,
resolver timeout — all surfaced as the resolution failing). No custom
regex/parser is introduced.

**Rationale**: `[sip].server` already accepts the same class of string with
no additional bridge-side syntax validation (`build_sip` only checks
non-empty, `gsm-sip-bridge/src/config/build.rs:122`) — consistency, and
avoids reinventing hostname-syntax validation the OS resolver already does
correctly.

**Alternatives considered**:
- *Restrict to IPv4/IPv6 literal syntax only, reject hostnames*: this was
  the rejected option (Option A) in the `/speckit-clarify` session — the
  chosen answer (Option B) explicitly keeps hostname support, just resolved
  once and cached.

## Decision 5: Test strategy

**Decision**:
- **Config layer** (`gsm-sip-bridge/src/config/mod.rs` test module): add
  cases to the existing `try_parse`-based suite — unset (default `None`,
  unchanged behavior), IP literal accepted, a syntactically-malformed value
  rejected with a `sip.public_addr` error, and (guarded appropriately, see
  below) a hostname that fails to resolve rejected the same way.
- **`pjsua-safe`**: extend the existing `tests/smoke.rs`-style integration
  tests, which already run in both stub mode and `--features pjsip-linked`
  against real, compiled PJSIP. Every existing `EndpointConfig` literal in
  that suite needs the new field added (mechanical, compile-enforced). Add
  a `pjsip-linked` case that constructs an `Endpoint` with `public_addr`
  set, creates an `Account` via each of `register`/`local`, calls
  `set_identity`, and confirms (via PJSUA's own transport/account info
  query, or by inspecting a generated SDP/Contact in a real call as
  `sdp_offer.rs`/`two_call_bridge.rs` already do) that the configured
  address is what's advertised before *and* after `set_identity` runs.
- A DNS-resolution-failure test needs a hostname guaranteed not to resolve;
  using a real (if reserved/invalid) name is consistent with the
  Integration-First principle's "mocks only for external services
  impractical to run locally" — DNS resolution is not impractical to run
  locally, so no mock or justification comment is needed. If CI/sandbox
  networking makes even NXDOMAIN resolution unreliable, fall back to
  asserting on a syntactically-invalid value instead and leave the
  resolution-failure path to manual/deployment verification (documented in
  `quickstart.md`).

**Rationale**: Matches the project's Integration-First Testing principle
(no new mocks) and reuses the exact dual-mode (stub / `pjsip-linked`) test
pattern the crate already established for every other PJSIP-facing feature.
