# Phase 0 Research: Inbound VoWiFi-to-SIP Call Bridge

**Feature**: 011-vowifi-sip-bridge | **Date**: 2026-07-12

All items below were resolved during architecture exploration (two parallel codebase surveys) and
a design pass prior to this planning session; there are no outstanding `NEEDS CLARIFICATION`
markers carried into Phase 1.

## 1. Where does the inbound IMS leg have to run?

**Decision**: The IMS/VoWiFi leg runs inside the `epdg-tunnel` container's `ims` network
namespace, as a long-running Rust process ("Agent A"). The SIP/PBX leg runs as a second
long-running process ("Agent B", PJSIP-backed) in the container's default namespace, joined to
Agent A by a dedicated `veth` pair.

**Rationale**: The SWu tunnel's routes and the Gm IPsec (kernel XFRM) policy installed during IMS
registration only match traffic that actually originates from inside netns `ims`
(`docker/epdg/entrypoint.sh`, `gsm-sip-bridge/src/ims/gm_ipsec.rs`). The PJSIP leg needs ordinary
LAN reachability to the PBX, which is only available on the container's default namespace
(`epdg-net` bridge network, per `docker/epdg/docker-compose.epdg.yml`). One process cannot satisfy
both constraints at once, so the two legs are necessarily two processes.

**Alternatives considered**:
- *Single process with the socket bound before entering the namespace*: rejected — PJSIP's
  internal transport/media threads are not compatible with binding a socket in one namespace and
  operating it after a `setns()` from safe Rust, and the existing Gm IPsec policy is
  namespace-scoped by construction (`ip netns exec ims ip xfrm ...`).
- *Move the whole container into netns `ims` and add a route out to the PBX from there*: rejected
  — would require punching PBX-bound routes and DNS through the same namespace as the carrier
  tunnel, entangling tunnel routing (owned by the SWu dialer) with LAN routing; the veth pair keeps
  the two routing domains cleanly separated.

## 2. How do the two legs move audio and signaling between each other?

**Decision**: Agent B places a second PJSIP call *across the veth* to Agent A, which answers it
with a lightweight SIP UAS (the same request-parsing/response-building code it uses for the
carrier-facing IMS leg). Agent B then uses PJSUA's conference bridge to connect the PBX call slot
and the veth call slot bidirectionally — the same `pjsua_conf_connect` mechanism the existing
CS-GSM bridge already uses today between the modem's sound-device slot and the PBX call slot
(`pjsua-safe/src/endpoint.rs:363-373`). Agent A relays RTP between the IMS side and the veth side
directly (packet-level, reusing existing codec/packetization helpers).

**Rationale**: This reuses PJSIP's proven jitter buffer, clock-drift handling, and (if ever needed)
transcoding on both legs, and needs no new `unsafe` code in `gsm-sip-bridge` — the constitution
and `tools/count-unsafe.sh` (invoked by `make lint`) enforce zero `unsafe` blocks in
`gsm-sip-bridge/src`, only allowing FFI `unsafe` inside `pjsua-safe`. A second PJSIP call is one
more `pjsua_call_make_call` + `pjsua_conf_connect` pair, not new FFI surface.

**Alternatives considered**:
- *Raw RTP/PCM relay over the veth feeding a custom `AudioMediaPort` directly into the PBX call
  slot*: lighter on the wire, but `pjsua-safe::AudioMediaPort`/`MediaPortHandle` and
  `modules/audio_pipeline.rs::AudioPipeline` are unused scaffolding today (confirmed: no
  production code path constructs or feeds them) — completing them means hand-rolling jitter
  buffering, timing, and any transcoding that PJSIP would otherwise provide for free. Deferred as a
  possible later optimization, not the first cut.

## 3. Codec choice for the first cut

**Decision**: Prefer PCMU (G.711 μ-law, 8 kHz) end-to-end when the inbound offer includes it, which
is the common case observed against the one carrier this has been tested with (Airtel — see
`gsm-sip-bridge/src/ims/call.rs` header notes: the far end chose PCMU when both PCMU and AMR-WB
were offered). AMR-WB (16 kHz) support already exists in `amr-safe`/`amr-sys` behind the
`amr-linked` feature and stays available as a fallback path, but a 16 kHz↔8 kHz transcode step
(needed to bridge AMR-WB to the PBX leg, which — like the rest of the SIP side — runs PJSIP's fixed
8 kHz narrowband media config, `pjsua-safe/src/endpoint.rs:99-105`) is deferred past the first cut.

**Rationale**: Matches the codec PJSIP's existing conference bridge already runs natively, avoiding
a transcode step in the critical path for the initial working version. AMR-WB-only far ends (a
real risk on VoLTE-only networks, per the existing `ims::call` module docs) are called out as a
known limitation, not silently unhandled.

## 4. Registration lifecycle: one-shot vs. persistent

**Decision**: Refactor the existing one-shot `register_session` (`gsm-sip-bridge/src/ims/mod.rs`)
into a persistent registration loop that re-REGISTERs before the `Expires` interval lapses (default
3600 s, `DEFAULT_EXPIRES`), running each renewal through a fresh AKA challenge/response over
`AT+CSIM`, and keeping the Gm-protected transport and installed XFRM SAs alive between renewals
instead of tearing them down (`RegisteredSession::cleanup` today runs unconditionally at the end of
every CLI invocation — that has to become conditional/deferred for a long-running agent).

**Rationale**: FR-001 and FR-007 (continuous reachability, automatic recovery) both require a
registration that outlives a single request/response transaction. The one-shot CLI tool proved the
protocol exchange works; a daemon needs the same exchange run on a timer with retry/backoff.

**Alternatives considered**: None seriously — the AKA/Gm-IPsec mechanics are already correct and
tested; the only change in kind is *when* they run (once vs. on a schedule) and what happens to
their state afterward (torn down vs. kept alive).

## 5. Inbound call detection: what's missing today

**Decision**: Add request-side (UAS) parsing to the hand-rolled SIP transport
(`gsm-sip-bridge/src/ims/sip_client.rs`), which today only parses *responses*
(`SipResponse::try_parse`). A new `SipRequest` parser mirrors the same partial-read/
`Content-Length` framing. Response builders for `100 Trying` / `180 Ringing` / `200 OK` (with an
SDP answer) / `200 OK` to `BYE` are new. `ims/sdp.rs` gets the offer-parsing/answer-building
counterparts to its existing `build_offer`/`parse_answer`.

**Rationale**: Every existing IMS code path is outbound-only (place a REGISTER, place an INVITE).
Receiving a call requires acting as a UAS for the first time in this codebase. This is pure
protocol logic, fully unit-testable with canned message fixtures — no hardware or live tunnel
needed to validate it, consistent with Constitution Principle I (Integration-First Testing; SIP
message parsing is a real component, not a mock, and integration here means testing the actual
parser against real wire-format bytes).

## 6. Declining a call (busy / SIP-side unreachable)

**Decision**: Per the spec clarification, a declined call gets an immediate, explicit SIP-level
rejection (e.g., `486 Busy Here` sent as the final response to the inbound INVITE) rather than
being left ringing or answered into silence.

**Rationale**: Directly resolves the spec's Clarifications session answer; a fast, standard SIP
rejection code is both the correct telephony convention and trivially deterministic to test (assert
on the response code, not on caller-perceived timing).

## 7. Deployment/process supervision

**Decision**: Both agents are launched from `docker/epdg/entrypoint.sh` after the tunnel is up and
`/tmp/pcscf` is written — Agent A inside `ip netns exec ims`, Agent B in the container's default
namespace — replacing today's manual `docker exec ... ims-call` invocation. `entrypoint.sh` also
creates the `veth` pair (one end moved into netns `ims`) before starting either agent. Both
processes are supervised (restarted on unexpected exit) by the entrypoint script's process
management, matching the existing script's pattern of supervising the SWu dialer itself.

**Rationale**: FR-001/FR-007 require unattended, continuous operation — a manually-invoked CLI tool
does not satisfy that. This stays inside the existing, deliberately-isolated `docker/epdg/`
deployment (kept separate from the production `docker-compose.yml` per its own README) rather than
merging into the main daemon's process, since the namespace split (research item 1) rules out a
single-process design regardless.

## 8. Testing strategy given the constitution's Integration-First mandate

**Decision**: Follow the existing project pattern for each layer:
- SIP/SDP message parsing and building (UAS additions): pure unit tests against canned
  request/response fixtures (mirrors existing `#[cfg(test)]` blocks in `sip_client.rs`, `sdp.rs`).
- Registration renewal scheduling and retry/backoff logic: testable in-process without hardware by
  injecting a fake clock/trigger, mirroring how `modules/mod.rs` already tests backoff logic for
  the CS-GSM resiliency feature (009).
- Control-channel protocol between Agent A and Agent B: in-process socket-pair round-trip tests,
  the same pattern `control/protocol.rs`'s existing tests already use.
- Two-call PJSIP bridging (`pjsua-safe` changes): follows the existing `pjsip-linked`-feature-gated
  pattern — stubbed/no-op without the feature so `cargo test --workspace` runs everywhere, real
  PJSIP behavior verified when built with `--features gsm-sip-bridge/pjsip-linked` against the
  system PJSIP.
- End-to-end (real tunnel, real inbound call, real audio): manual/hardware-gated verification per
  the Verification section of this feature's plan — cannot be part of `cargo test --workspace`
  since it requires a live SIM, live carrier network, and a reachable PBX.

**Rationale**: `make test` (`cargo test --workspace`) must stay green on every commit
(Constitution Principle II) without requiring hardware, matching how the existing `ims` and
`modules` code is already tested.
