# Implementation Plan: AMR passthrough for AMR-capable clients

**Branch**: `120-amr-passthrough-forwarder` | **Date**: 2026-10-22 | **Spec**: [spec.md](spec.md)
**Input**: `specs/120-amr-passthrough-forwarder/spec.md` (issue #111, option 2a of #109); evidence in `specs/111-amr-passthrough-spike/findings-phase{1,2}.md`

## Summary

On an inbound VoWiFi/VoLTE call whose carrier leg and client leg both settled on AMR-WB (or both AMR-NB) with compatible parameters, the speech frames stop being decoded and re-encoded. Agent A relays AMR packets between the carrier and a plain UDP socket on the veth; Agent B runs a socket forwarder between that socket and the client. PJSIP in Agent B keeps SIP/SDP only: its client-leg media transport is a pjmedia loop transport that advertises the forwarder's address, and the stream is paused. Every other call is transcoded exactly as today.

Off by default behind one `[vowifi].amr_passthrough` boolean. Inbound only.

The approach differs from the spec's first sketch in one important way (details and evidence in [research.md](research.md) R1): Agent B places both its legs *before* the client answers, so the client's AMR parameters are not known when the offers go out. The plan therefore never changes a call that does not qualify. Every call starts on today's path. After the client answers, and before Agent B tells Agent A to answer the carrier (so no media has flowed), Agent B judges compatibility. Only a qualifying call is **upgraded**: Agent A and Agent B agree a forwarding socket pair over the existing control channel, and Agent B re-INVITEs the client leg onto the forwarder's address. A non-qualifying call has no extra step at all, which is what makes US2 and SC-002 hold by construction.

Two parts of this are unproven on stock pjproject 2.16 and are gated by a spike task before any product code (research R2, R3): re-creating the client leg's media transport on a re-INVITE (`PJSUA_CALL_REINIT_MEDIA`), and rejecting a client's incompatible re-INVITE (`on_call_rx_reinvite`).

## Technical Context

**Language/Version**: Rust (workspace crates `gsm-sip-bridge`, `pjsua-safe`, `pjsua-sys`), pjproject 2.16 stock build in Docker
**Primary Dependencies**: none new. Public pjsua/pjmedia API only: `pjmedia_transport_loop_create2`, `pjmedia_transport_close`, `pjmedia_stream_pause`, `pjsua_call_reinvite2`, callbacks `on_create_media_transport`, `on_stream_created2`, `on_call_rx_reinvite`
**Storage**: N/A. One new optional field on the in-memory `CallRecord` (control protocol), `serde(default)`
**Testing**: `cargo test` via `make test`; real loopback UDP sockets for the relay (no mocks); pure functions for compatibility and repacking; pjsip-linked hook checks in the privileged container (`test/`); live Vodafone + Jio calls with an AMR-WB softphone
**Target Platform**: Linux container, amd64 and arm64 (Pi, Jio line)
**Project Type**: single Cargo workspace, two supervised agent processes per line (A in the carrier netns, B in the default netns)
**Performance Goals**: SC-004 (≥70% less bridge CPU on the Pi), SC-005 (≥20 ms less added delay)
**Constraints**: stock pjproject, no patches, one image (spec Assumptions); carrier-facing SDP byte-identical (FR-006); GSM path untouched (FR-018); forward-or-transcode never fails a call (FR-005); control-protocol changes must stay readable by an older peer (`serde(default)`)
**Scale/Scope**: about 10 source files plus one new module in each of `gsm-sip-bridge` and `pjsua-safe`, docs and release notes

## Constitution Check

| Principle | Status |
|---|---|
| I. Integration-first testing | Pass. The relay is tested over real loopback UDP sockets; compatibility and repacking are pure functions tested directly; the PJSIP hook is tested against real PJSIP in the privileged container. No mocks. The unavoidable external piece (a real carrier) is covered by the live-verification tasks, not a fake. |
| II. Green-on-commit | Pass. Each task phase ends with `make format && make lint && make test`. |
| III. Atomic commits | Pass. One commit per task group (config, pure logic, relay, control protocol, PJSIP hook, wiring, docs). |
| IV. Makefile-driven | Pass. No new commands. |
| V. Simplicity | Pass with one justified addition. The upgrade-on-answer design is chosen *because* it adds no step to calls that don't qualify and needs no fallback transport. Rejected heavier alternatives are in research R1. RTCP towards the client is "none" rather than a synthesised stream (R7). No SSRC/seq rewriting (spec Assumptions). |

Re-checked after design: no violations; Complexity Tracking not needed.

## Project Structure

### Documentation

```text
specs/120-amr-passthrough-forwarder/
├── spec.md
├── plan.md              # this file
├── research.md          # R1–R12
├── data-model.md        # AmrLegParams, MediaPath, ForwardSession
├── quickstart.md        # lab + live verification
├── contracts/
│   ├── config.md        # [vowifi].amr_passthrough
│   ├── control-protocol.md   # IncomingCall extension, ForwardMedia/ForwardAck/ForwardCancel
│   └── logs.md          # the "media path" log line and CallRecord.media_path
├── checklists/requirements.md
└── tasks.md             # /speckit-tasks
```

### Source touched

```text
gsm-sip-bridge/src/amr_forward/mod.rs        # NEW: re-exports
gsm-sip-bridge/src/amr_forward/params.rs     # NEW: AmrLegParams, parse from SDP fmtp/rtpmap, compatibility rule, MediaPath + reasons
gsm-sip-bridge/src/amr_forward/repack.rs     # NEW: octet-aligned <-> bandwidth-efficient without decoding (all frame types, CMR kept)
gsm-sip-bridge/src/amr_forward/relay.rs      # NEW: socket relay (PT remap, direction gate, counters, SSRC log), used by A and B
gsm-sip-bridge/src/ims/amr_rtp.rs            # reuse BitReader/BitWriter/speech_bits; expose the pieces repack.rs needs
gsm-sip-bridge/src/ims/sdp.rs                # ChosenCodec gains AmrLegParams for the AMR case (mode-set, ptime/maxptime, features)
gsm-sip-bridge/src/ims/agent/{inbound,veth}.rs  # A: send carrier params in IncomingCall; handle ForwardMedia; spawn the forwarding relay instead of transcoding
gsm-sip-bridge/src/volte/carrier_agent.rs    # pass the setting through (it builds its own agent params)
gsm-sip-bridge/src/vowifi/control.rs         # IncomingCall.carrier_amr, ForwardMedia/ForwardAck/ForwardCancel, CallRecord.media_path
gsm-sip-bridge/src/vowifi/mod.rs             # B: decide after the client answers, upgrade or leave, forwarder lifecycle, call record
gsm-sip-bridge/src/config/{mod,raw,build}.rs # [vowifi].amr_passthrough, default false
gsm-sip-bridge/src/commands/config.rs        # AMR_PASSTHROUGH in the shell export
gsm-sip-bridge/tests/test_shell_env_contracts.rs
pjsua-safe/src/passthrough.rs                # NEW: the unsafe FFI surface, nothing else touches loop transports
pjsua-safe/src/endpoint.rs                   # register the three callbacks; per-call forward registry; unpair on upgrade
pjsua-safe/src/call.rs                       # reinvite onto the forwarder; stream info incl. fmtp and direction
docs/{configuration,vowifi-bridge,operations}.md, config.toml.example, CHANGELOG.md, RELEASE_NOTES.md
```

**Structure Decision**: the protocol-independent parts (compatibility, repacking, the relay) live in one new `amr_forward` module of the main crate so Agent A and Agent B share them, and are testable without PJSIP. All new `unsafe` is confined to one new file in `pjsua-safe`, behind a small safe API (`register_forward(call_id, ForwarderAddr)`, `forward_params(call_id)`); `make lint`'s unsafe audit sees one place.
