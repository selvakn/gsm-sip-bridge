# Implementation Plan: Inbound VoWiFi-to-SIP Call Bridge

**Branch**: `011-vowifi-sip-bridge` | **Date**: 2026-07-12 | **Spec**: [./spec.md](./spec.md)
**Input**: Feature specification from `specs/011-vowifi-sip-bridge/spec.md`

## Summary

Add a second, independent inbound call path alongside the existing circuit-switched GSM-to-SIP
bridge: calls that arrive over the carrier's VoWiFi/IMS service (via the already-proven ePDG
tunnel in `docker/epdg/`) are answered automatically and two-way bridged to the same SIP/PBX
destination the CS bridge already uses. Because the IMS leg must run inside the tunnel's network
namespace while the SIP/PBX leg needs ordinary LAN reachability, the feature is delivered as two
supervised, communicating processes — an IMS agent and a SIP agent — joined by a dedicated `veth`
link, both launched and supervised by `docker/epdg/entrypoint.sh`. The SIP agent bridges audio by
placing a second PJSIP call across the veth and conference-connecting it to the PBX call, reusing
the same `pjsua_conf_connect` mechanism the CS bridge already relies on.

## Technical Context

**Language/Version**: Rust stable (toolchain pinned by `rust-toolchain.toml`), same workspace as
the rest of the project.

**Primary Dependencies**:
- `pjsua-safe` / `pjsua-sys` — PJSIP wrapper, extended to support two concurrent calls
  conference-connected to each other instead of one call connected to a sound device.
- `amr-safe` / `amr-sys` — AMR-WB codec, already present behind the `amr-linked` feature; reused
  as-is for the (deferred-priority) AMR-WB fallback path.
- `tokio` — already a workspace dependency; used for the control-socket-style pattern if the new
  agents adopt it, though the existing `ims` module's blocking/`std::net` style is the closer
  precedent and is preferred for consistency (see research.md item 8 testing notes).
- No new external crates anticipated. The Agent A↔B control channel reuses the existing
  newline-JSON convention (`serde_json`, already a dependency) rather than pulling in a new
  protocol library.

**Storage**: No new persistent storage (see `data-model.md` — no new SQLite tables). VoWiFi Line
Registration and Bridged Call state are in-process; recent-call history for status reporting is an
in-memory bounded ring buffer plus structured `tracing` events, consistent with how the existing
project treats short-lived operational state.

**Testing**: `cargo test --workspace`, run via `nextest` (`.config/nextest.toml`). SIP/SDP
UAS-parsing additions and registration-renewal scheduling are unit-tested with no hardware
required, mirroring the existing `#[cfg(test)]` patterns in `ims/sip_client.rs` and `ims/sdp.rs`.
The Agent A↔B protocol gets in-process socket-pair round-trip tests like `control/protocol.rs`'s
existing tests. `pjsua-safe` two-call bridging follows the existing `pjsip-linked`-feature-gated
pattern (no-op/stub without the feature, so `cargo test --workspace` stays green without a real
PJSIP build). Live end-to-end verification (real tunnel, real inbound call) is manual — see
`quickstart.md` — and is explicitly out of the automated suite, same as the existing `ims-call`
CLI tool's real-network testing was.

**Target Platform**: Linux (Debian bookworm inside the `epdg-tunnel` container), same
Unix-domain-socket/network-namespace assumptions the rest of the project already makes.

**Project Type**: Extension of the existing Cargo workspace binary (`gsm-sip-bridge`), adding new
CLI subcommands (`vowifi-ims-agent`, `vowifi-sip-agent`, plus a status-query subcommand) alongside
the existing `ims-register`/`ims-call`/`card` subcommands — not a new crate.

**Performance Goals** (from spec Success Criteria):
- Inbound call answered and bridged within 5 s (SC-001) — matches the CS bridge's existing
  responsiveness expectation.
- VoWiFi reachability automatically restored within 90 s of the underlying network path returning
  (SC-003) — the existing CS-side network-loss-detection window (60 s, feature 009) plus headroom
  for the IMS-AKA/Gm-IPsec re-handshake.
- Operator can read current line health and last call outcome in under 30 s via status tooling
  (SC-004).

**Constraints**:
- **Zero new `unsafe` blocks in `gsm-sip-bridge/src`** — enforced by `make lint` →
  `tools/count-unsafe.sh`, which fails the build on any `unsafe` there. The chosen architecture
  (second PJSIP call + `pjsua_conf_connect`, all via existing `pjsua-safe` FFI) requires no new
  `unsafe` in `gsm-sip-bridge`; any FFI surface growth stays inside `pjsua-safe` and must keep its
  `unsafe` ratio under the existing 5% threshold.
- Full pre-commit gate stays exactly as documented in `CLAUDE.md` and unchanged by this feature:
  `cargo fmt --all`, `make lint` (rustfmt check + `clippy -D warnings` + unsafe audit + `cargo
  deny check`), `cargo test --workspace`. No relaxation of any of these for this feature's code.
- Two-process, two-namespace split is structurally required (research.md item 1) — not a
  complexity choice to be minimized away.
- The existing CS-GSM bridge must remain byte-for-byte unmodified in behavior (FR-006); this
  feature only adds new modules/subcommands and extends `pjsua-safe` to support a second
  simultaneous call, without touching the existing slot-0-to-call-slot bridging code path.

**Scale/Scope**: Single SIM/single VoWiFi line, one call at a time (spec Assumptions) — no
multi-line or multi-call concurrency in scope.

## Constitution Check

*Gate: must pass before Phase 0. Re-checked after Phase 1.*

### I. Integration-First Testing — PASS
- SIP/SDP UAS parsing and building tested against real wire-format byte fixtures (not mocks),
  mirroring the existing `sip_client.rs`/`sdp.rs` test style.
- Agent A↔B control protocol tested with real in-process socket pairs, same pattern as
  `control/protocol.rs`'s existing tests — no mocking of the transport.
- `pjsua-safe`'s two-call conference-connect logic is exercised for real when built with
  `--features pjsip-linked` against the system PJSIP; stubbed (documented, not silently mocked)
  otherwise, matching the existing feature-gate convention already used for the single-call path.
- End-to-end call flow (real IMS registration, real inbound call, real audio) is validated
  manually against the real carrier network and real hardware (quickstart.md) — this is the
  correct integration boundary given hardware/network dependencies, exactly as the existing
  `ims-register`/`ims-call` tools already required manual validation against real carriers.

### II. Green-on-Commit — PASS (process gate)
- Every task in the eventual `tasks.md` ends with `cargo test --workspace` passing before commit,
  per the CLAUDE.md pre-commit checklist, unchanged by this feature.

### III. Frequent Atomic Commits — PASS
- The phased breakdown below (UAS parsing → persistent registration → RTP relay → PJSIP two-call
  bridging → deployment glue) is sized so each phase is independently committable and testable,
  matching how feature 009 was structured.

### IV. Makefile-Driven Build — PASS
- No new Makefile targets required. `make build`, `make test`, `make lint`, `make format` continue
  to cover this feature's code exactly as they do today; the new subcommands are CLI surface, not
  new build outputs.

### V. Simplicity & Refactorability — PASS
- The two-process/veth split is the simplest architecture that satisfies the hard
  namespace-isolation constraint (research.md item 1) — not an added layer for its own sake.
- Reusing PJSIP's conference bridge for the second call (research.md item 2) avoids hand-rolling a
  new audio-relay abstraction; the alternative (completing the unused `AudioMediaPort`/
  `AudioPipeline` scaffolding) is explicitly deferred rather than built speculatively, per YAGNI.
- The Agent A↔B protocol is newline-JSON, matching the existing control-protocol convention exactly
  rather than introducing a new serialization scheme (research.md item 8 / contracts).

No constitution violations requiring justification — Complexity Tracking table is empty.

## Project Structure

### Documentation (this feature)

```text
specs/011-vowifi-sip-bridge/
├── plan.md                          ← this file
├── research.md                      ← Phase 0 output
├── data-model.md                    ← Phase 1 output
├── contracts/
│   └── agent-control-protocol.md    ← Phase 1 output
├── quickstart.md                    ← Phase 1 output
└── tasks.md                         ← Phase 2 output (/speckit-tasks, not created by /speckit-plan)
```

### Source Code Changes (all in `gsm-sip-bridge/src/` and `pjsua-safe/src/`, plus `docker/epdg/`)

```text
gsm-sip-bridge/src/
├── cli.rs                       MODIFY — add vowifi-ims-agent / vowifi-sip-agent / vowifi-status
│                                          subcommands alongside existing ims-register/ims-call
├── config/
│   └── mod.rs                   MODIFY — add VowifiConfig (enable, mcc/mnc, modem port, tcp/
│                                          sec-agree, pcscf source, veth addressing); reuse
│                                          existing SipConfig/bridge-destination config as-is
├── ims/
│   ├── sip_client.rs            MODIFY — add SipRequest parsing (UAS side), response builders
│   │                                     (100/180/200/486), dialog-state helpers
│   ├── sdp.rs                   MODIFY — add parse_offer / build_answer (inverse of existing
│   │                                     build_offer/parse_answer)
│   ├── mod.rs                   MODIFY — persistent registration loop (renew-before-expiry,
│   │                                     keep Gm IPsec SAs alive) replacing one-shot cleanup
│   ├── agent.rs                 NEW — Agent A: registration lifecycle + inbound INVITE/BYE
│   │                                  dispatch + IMS↔veth RTP relay
│   └── rtp.rs                   MODIFY (minor) — any relay-loop helpers beyond existing
│                                     build_packet/parse_packet/ulaw conversions
├── vowifi/                      NEW
│   ├── mod.rs                   Agent B orchestrator: on incoming_call, places PBX leg + veth
│   │                                  leg via SipBridge/pjsua-safe, conference-bridges them
│   └── control.rs               Agent A↔B newline-JSON protocol (contracts/agent-control-
│                                     protocol.md) — request/response types + read/write helpers,
│                                     following control/protocol.rs's exact pattern
└── main.rs                      MODIFY — dispatch new subcommands before daemon startup, same
                                       pattern as existing Card/ImsRegister/ImsCall handling

pjsua-safe/src/
├── endpoint.rs                  MODIFY — generalize on_call_media_state_cb beyond the hardcoded
│                                          slot-0 sound-device connect so a call can be told which
│                                          peer call slot to bridge to; add pjsua_set_null_snd_dev
│                                          support (no physical sound device in this container)
└── call.rs                      MODIFY (if needed) — support holding/tracking two concurrent
                                       Call handles from one Endpoint

docker/epdg/
├── entrypoint.sh                 MODIFY — create veth pair, launch + supervise both agents after
│                                           tunnel readiness, replacing the manual `docker exec
│                                           ... ims-call` flow described in its README
└── docker-compose.epdg.yml       MODIFY (if needed) — any additional capabilities/env for the
                                        veth pair or agent configuration
```

**Structure Decision**: Everything stays inside the existing `gsm-sip-bridge` binary as additional
modules and CLI subcommands (matching how `ims-register`/`ims-call` were added), plus the matching
extension to `pjsua-safe` for two-call bridging. No new crate, no new repository top-level
directory beyond the existing `docker/epdg/` deployment surface — consistent with Constitution
Principle V (fewer moving parts) and with how the project's prior IMS work was integrated.

## Complexity Tracking

*No entries — Constitution Check passed without needing to justify any deviation.*
