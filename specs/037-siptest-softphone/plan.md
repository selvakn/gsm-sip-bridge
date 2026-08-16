# Implementation Plan: siptest — SIP softphone for agent-driven end-to-end testing

**Branch**: `037-siptest-softphone` | **Date**: 2026-08-15 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/037-siptest-softphone/spec.md`

## Summary

A new workspace crate, `siptest`, providing a pure-Rust SIP softphone daemon
whose only user is a coding agent. It registers to the bridge's embedded
registrar as an ordinary handset, places outbound calls over the real carrier
path (following the registrar's `302` redirect to the telephony agent), answers
inbound carrier calls, and produces a per-direction audio verdict backed by
tone detection and packet counting kept deliberately separate. It is driven
over an HTTP control API with a cursor-addressable event log, so an agent can
operate it — including discovering inbound calls — with `curl` and `jq` alone.

The bridge's carrier leg is already well instrumented. The **local SIP leg** is
not: it can only be exercised by picking up a physical handset, which an agent
cannot do. This closes that gap, and along the way fills an explicitly flagged
hole — `MediaReport::round_trip_delay` has always been `None`, commented
*"Needs a tone detector for our own marker returning; only verifiable on a live
call"* (`gsm-sip-bridge/src/ims/call.rs:153-155`).

No pjsip. The crate reuses the pure primitives already in
`gsm_sip_bridge::ims`, which are independent of the pjsua-based local leg it
tests — so this remains a genuine interop test rather than a stack testing
itself.

## Technical Context

**Language/Version**: Rust, pinned 1.94.0 (`rust-toolchain.toml`), edition 2021.
**Primary Dependencies**: `clap` 4 (derive), `serde`/`toml`, `tokio` (axum only),
`axum` 0.8, `tracing`/`tracing-subscriber`, `thiserror`, `crossbeam-channel`,
`serde_json` — every one already in the workspace. Plus a path dependency on
`gsm-sip-bridge` for `ims::{rtp, sip_client, digest, media_stats}`. One
G.722 codec dependency is deferred to US4-b and deliberately not chosen yet
(see [research.md](./research.md) R7).
**Storage**: Per-call WAV recordings and a JSON report sidecar under a
configured directory, capped at `max_calls_retained` completed calls with
oldest-first eviction that deletes the files. No database. In-memory bounded
ring buffers for events, recent log lines, and the call registry.
**Testing**: `cargo nextest` via the existing `make test`. Pure unit tests
inline; integration tests in `siptest/tests/test_*.rs` running the bridge's
**real** registrar in-process (`Registrar::start_on{,_with_outbound}`, both
already `pub`) plus a loopback stub standing in for pjsua. No `#[ignore]`, no
env gating — the repo has neither. Per-test budget is 20s
(`.config/nextest.toml`), so call durations must be parameters.
**Target Platform**: Linux host, same LAN as the bridge. Runs on the host
directly — being pjsip-free, it needs no `pjsip-linked` build.
**Project Type**: CLI/daemon crate inside an existing Rust workspace.
**Performance Goals**: 20 ms media cadence held with jitter well under one
packet time, using absolute-deadline scheduling. Control API latency is
irrelevant — it is a debugging interface.
**Constraints**: UDP only (the registrar enforces it). One unconnected socket
for all SIP, because the registrar authorises outbound calls by exact source
`SocketAddr` and the bridge rings from a different port than it registers on.
No NAT traversal. Single concurrent call. Outbound dialling is gated by a
fail-closed destination allow-list plus a rate limit, both enforced before any
signalling leaves the host, because an agent can retry in a loop and the calls
are real (see [spec Clarifications](./spec.md#clarifications)).
**Scale/Scope**: One account, one call at a time, one operator (an agent).
Roughly 3–4k lines across eight phased slices.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

**I. Integration-First Testing (NON-NEGOTIABLE)** — **PASS.** The integration
tests run the bridge's *real* registrar in-process and siptest's *real*
registration, outbound and inbound state machines against it; every line on
that path is production code. One stand-in is required: the pjsua telephony
agent, which lives behind the `pjsip-linked` feature that neither `make test`
nor CI ever compiles. That is squarely the "component not available in CI"
carve-out, and it carries the mandated written justification at the stub site.
The state machines are pure `step(input, now) -> Vec<Output>` functions with no
I/O, so behaviour is testable without mocking any boundary.

**II. Green-on-Commit (NON-NEGOTIABLE)** — **PASS.** Each of the eight slices
is independently green. `make format && make lint && make test` gates every
commit, per `CLAUDE.md`.

**III. Frequent Atomic Commits** — **PASS.** One commit per slice, each a
single logical change with the "why" in the message.

**IV. Makefile-Driven Build** — **PASS.** Adding `siptest` to `members` makes
it reachable through the existing `--workspace` targets; no new build entry
point, the same argument `specs/035`'s check makes. Two documented convenience
targets are added, each with the required `## ` description.

**V. Simplicity & Refactorability** — **PASS with two tracked items.** See
Complexity Tracking. The design actively removes moving parts relative to the
obvious approach: no SSE, no echo buffer, no async in the SIP path, and one
implementation of each call flow (the CLI subcommands are HTTP clients against
the daemon, not a parallel path).

**Post-Phase-1 re-evaluation** — **PASS, unchanged.** The design added no
abstraction beyond the two tracked rows. The `Codec` trait exists solely to
keep an external dependency swappable and to force the two-rate distinction
into the type system; it has one implementation until US4-b.

## Project Structure

### Documentation (this feature)

```text
specs/037-siptest-softphone/
├── plan.md              # This file
├── spec.md              # Feature specification
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/
│   ├── control-api.md   # HTTP control surface
│   └── sip-flows.md     # Wire-level sequences siptest must satisfy
└── tasks.md             # Phase 2 output (/speckit.tasks — NOT created here)
```

### Source Code (repository root)

```text
Cargo.toml                        # CHANGED: add "siptest" to members
tools/count-unsafe.sh             # CHANGED: cover siptest/src; tighten the grep
Makefile                          # CHANGED: add siptest, siptest-status targets

gsm-sip-bridge/src/ims/mod.rs     # CHANGED: widen rtp/sip_client/digest to pub
gsm-sip-bridge/src/ims/sip_client.rs  # CHANGED: add pub parse_datagram();
                                  #          refactor recv_message_deadline onto it

siptest/                          # NEW
├── Cargo.toml
├── src/
│   ├── main.rs                   # thin: parse → logging → exhaustive match → ExitCode
│   ├── lib.rs
│   ├── cli.rs                    # clap 4 derive
│   ├── config.rs                 # serde + toml; reuses config::env::resolve_in_place
│   ├── error.rs                  # thiserror; SipTestResult<T>
│   ├── safety.rs                 # fail-closed allow-list + sliding-window rate limit (pure)
│   ├── daemon.rs                 # composition root: sockets, threads, shutdown
│   ├── sip/
│   │   ├── mod.rs
│   │   ├── socket.rs             # one UNCONNECTED UdpSocket; local-IP discovery
│   │   ├── message.rs            # REGISTER/INVITE/ACK/CANCEL builders (fresh)
│   │   ├── registration.rs       # RegistrationFsm  (pure)
│   │   ├── outbound.rs           # OutboundCallFsm  (pure; the 302 dance)
│   │   ├── inbound.rs            # InboundCallFsm   (pure; 200-OK retransmit)
│   │   └── engine.rs             # the one dialog thread: select! over sip/cmd/tick
│   ├── sdp.rs                    # offer/answer for PT 0 / 9 / 101 (fresh)
│   ├── media/
│   │   ├── mod.rs
│   │   ├── codec.rs              # CodecProfile + Encoder/Decoder
│   │   ├── tone.rs               # free-running generator, sample-index driven
│   │   ├── goertzel.rs           # detector + symbol decode (pure)
│   │   ├── level.rs              # RMS/peak dBFS + noise-floor percentile
│   │   ├── session.rs            # tx thread + rx thread, TxTimeline, RTT
│   │   └── report.rs             # verdict bundle + render_text()
│   ├── api/
│   │   ├── mod.rs                # axum router + AppState
│   │   ├── handlers.rs
│   │   ├── events.rs             # monotonic seq, bounded ring, long-poll
│   │   └── state.rs              # snapshot + capped call registry w/ oldest-first eviction
│   └── logbuf.rs                 # tracing Layer → bounded ring for /log/tail
└── tests/
    ├── test_against_registrar.rs      # real Registrar + loopback stub UAS
    ├── test_inbound_from_other_port.rs
    ├── test_outbound_source_port.rs
    ├── test_control_api.rs
    ├── test_goertzel.rs
    ├── test_tone_roundtrip.rs
    ├── test_sdp.rs
    └── test_cli.rs
```

**Structure Decision**: A single new workspace member, `siptest/`, following
the repo's established crate shape — thin `main.rs` with all logic in the
library so `tests/` can reach it (the rule stated at
`gsm-sip-bridge/src/commands/mod.rs:1-10`), `thiserror` and no `anyhow`,
`tracing` to stderr, and tokio constructed explicitly rather than via
`#[tokio::main]`. It is reachable through the existing `--workspace` Makefile
targets, so no new build entry point is introduced. Two small changes land in
`gsm-sip-bridge` to make already-`pub` types reachable from a sibling crate
(see research R1); they add no behaviour.

## Complexity Tracking

| Violation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|-------------------------------------|
| A path dependency on `gsm-sip-bridge` drags alsa, rusb, pcsc, serialport, rusqlite and prometheus into siptest's build graph | Reuses ~1.5k lines of already-tested primitives — RTP framing, μ-law, the WAV writer, digest maths, and loss/reordering/jitter tracking — instead of writing a second copy of each | Extracting a `sip-core` crate that both depend on is the clean layering, but it is a refactor of a 15k-line module currently load-bearing for live carrier calls. YAGNI until build times actually bite; revisit then, not now |
| A `Codec` trait with a single implementation until US4-b | Forces the audio-rate/clock-rate distinction into the type system before G.722 arrives — the bug it prevents is silent and corrupts the very measurement the tool exists to make — and keeps an external codec dependency swappable | A concrete PCMU struct would have to be retrofitted to a trait when G.722 lands, at the moment when the two-rate confusion is most likely to be introduced unnoticed |
