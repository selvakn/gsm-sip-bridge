# Contract: Agent A/B outbound origination protocol

**Feature**: 025-outbound-calling (revision 4)
**Extends**: `vowifi::control::ControlMessage`
(`specs/011-vowifi-sip-bridge/contracts/agent-control-protocol.md`), the
existing veth-carried, newline-JSON, event-driven protocol between Agent A
(`ims::agent`, carrier-facing) and Agent B (`vowifi::mod`, phone/PBX-facing).

This is the outbound mirror of the existing inbound triad
(`IncomingCall`/`BridgeReady`/`BridgeFailed`). Same transport, same framing
(`read_msg`/`write_msg`), same tagged-JSON shape (`#[serde(tag = "event")]`).

## New variants

```rust
enum ControlMessage {
    // ...existing inbound variants unchanged...

    /// Agent B → Agent A. Sent once Agent B has accepted the
    /// outbound-triggering INVITE (spec 025 US1/US3) and determined this
    /// line's carrier path is VoWiFi/VoLTE rather than circuit-switched.
    /// `destination` is verbatim from the originating request — no
    /// transformation (FR-010), same discipline as the CS path.
    PlaceCall { call_id: String, destination: String },

    /// Agent A → Agent B. Sent immediately once Agent A decides to attempt
    /// `PlaceCall` (i.e. it was not busy) — before touching the carrier
    /// transport at all. Added after live testing (T072) found that
    /// without an immediate ack, Agent B could not tell "busy, try the next
    /// line" (fast, no carrier round trip) apart from "committed, now
    /// genuinely placing the call" (can take as long as a real carrier
    /// ring), and used one short timeout for both — abandoning real,
    /// ringing calls the carrier went on to answer.
    CallAttempting { call_id: String },

    /// Agent A → Agent B. The carrier sent `180 Ringing` for the
    /// originated INVITE. Non-terminal — sent at most once per call
    /// regardless of retransmission, zero or more of these arrive before
    /// the real `CallPlaced`/`CallFailed`. Agent B relays it as
    /// `call.answer(180)` on the phone/PBX leg (FR-012's progress table,
    /// `contracts/sip-dialout.md`). Added 2026-08-03 (review): without
    /// this, the caller heard nothing at all for up to
    /// `OUTBOUND_INVITE_TIMEOUT + OUTBOUND_RING_TIMEOUT` (75s) and then a
    /// sudden answer.
    CallRinging { call_id: String },

    /// Agent A → Agent B. The carrier leg is up (2xx received, ACK sent)
    /// and Agent A's veth-facing UAS listener (`spawn_veth_uas_listener`,
    /// already used unmodified for inbound) is up and waiting — mirrors
    /// `IncomingCall`'s role, direction reversed. No port travels on this
    /// message: exactly like inbound, Agent B places a real `Call::make`
    /// toward Agent A's veth listener and RTP addressing is negotiated
    /// through that SIP/SDP exchange, not this JSON. Agent B
    /// conference-bridges the resulting veth call to its already-accepted
    /// phone/PBX leg via `pjsua_safe::Endpoint::pair_calls` — the same
    /// primitive `bridge_call` already uses for inbound.
    CallPlaced { call_id: String },

    /// Agent A → Agent B. The carrier declined, was unreachable, or the
    /// line was otherwise unable to place the call (busy, network refused,
    /// no P-CSCF reachability, or genuinely never answered within the ring
    /// window — the last one marked with `reason::CARRIER_TIMEOUT` so
    /// Agent B can report `Unanswered` rather than a generic refusal,
    /// SC-005). Agent B answers the phone/PBX leg accordingly (the
    /// carrier's own status code when the reason carries one, else `503`
    /// — `contracts/sip-dialout.md`'s table, `vowifi::mod`'s
    /// `carrier_status_from_reason`/`outbound_outcome_for_committed_failure`)
    /// and tears down the accepted leg.
    CallFailed { call_id: String, reason: String },
}
```

`CallEnded`/`HangupAck` are reused unmodified for teardown in both
directions — already bidirectional per their existing doc comments.

## Sequence

```text
Agent B                                  Agent A
   │  (phone/PBX INVITE accepted,           │
   │   US1/US3 — already shipped)           │
   │                                        │
   │ ── PlaceCall{call_id, destination} ──▶ │
   │                                        │  not busy: commit
   │ ◀── CallAttempting{call_id} ─────────  │  (before touching the
   │                                        │   carrier transport at all)
   │                                        │
   │                                        │  build INVITE over the
   │                                        │  already-registered session
   │                                        │  (R-010) — never a fresh
   │                                        │  register_session call
   │                                        │
   │                                        │  ── INVITE ──▶ carrier
   │                                        │  ◀── 180 ──
   │ ◀── CallRinging{call_id} ────────────  │  (relayed as call.answer(180)
   │                                        │   on the phone/PBX leg)
   │                                        │  ◀── 200 ──
   │                                        │
   │ ◀── CallPlaced{call_id} ─────────────  │  (2xx + ACK done, veth
   │                                        │   listener up and waiting)
   │  places a veth Call::make toward       │
   │  Agent A's veth SIP listener ────────▶ │  spawn_veth_uas_listener
   │                                        │  (unmodified, already used
   │                                        │  for inbound) answers it
   │                                        │
   │  pair_calls(phone_leg, veth_leg)       │
   │  (R-011, mirrors bridge_call)          │
   │                                        │
   │ ◀────── CallEnded / HangupAck ───────▶ │  (either direction, existing)
```

On `CallFailed` instead of `CallPlaced`: Agent B answers the phone/PBX leg
per `contracts/sip-dialout.md`'s outcome table and does not attempt
`pair_calls` — no different from the CS path's `refused_network_failure`
outcome, just carried over this channel instead of `ControlCmd`. FR-009a:
this also ends the whole request — `run_outbound_listener`'s line loop
does *not* try another line after a post-`CallAttempting` `CallFailed`
(`PlaceCallOutcome::Committed`, distinct from the pre-commitment
`Unavailable` that *does* try the next line) — the carrier already
answered for this destination, so retrying elsewhere would just ring it
again for a call it just refused.

## Timeouts (two phases, not one — on both sides)

`try_place_on_line` (`vowifi/mod.rs`) waits for `CallAttempting`/`CallFailed`
first, with a short timeout (`PLACE_CALL_TIMEOUT`, 3s) — this phase never
involves the carrier, so a slow reply means "busy" or "unreachable," and
Agent B moves on to the next line. Once `CallAttempting` arrives, Agent B
switches to a much longer timeout (`CALL_ATTEMPT_TIMEOUT`, 90s, comfortably
larger than Agent A's own `OUTBOUND_INVITE_TIMEOUT + OUTBOUND_RING_TIMEOUT`)
before treating the line as failed — a real carrier call can legitimately
take that whole window to ring and answer. Using one short timeout for both
phases was the original (buggy) implementation: live testing (T072 pass 1)
had the carrier fully answer a call (100/183/180/200 OK) after Agent B had
already given up and moved to the next line, leaving the answered call
unbridged until Agent A's own veth-wait timed out and it hung up on the
carrier.

`PLACE_CALL_TIMEOUT` also has to clear how long a `PlaceCall` can sit in
Agent A's own channel before its dispatch loop even notices it —
`ims::agent::IDLE_POLL_INTERVAL` (1s; found live, T072 pass 2, back when
this was 30s and `PLACE_CALL_TIMEOUT` gave up before Agent A got around to
acking at all).

Agent A's own carrier wait (`SipTransport::recv_final_response_for_origination`)
is itself two-phase: `OUTBOUND_INVITE_TIMEOUT` (15s) for *any* response at
all (even `100 Trying`) — if nothing arrives, something transport-level is
actually wrong — then `OUTBOUND_RING_TIMEOUT` (60s) for the real final
response once the call is confirmed in flight, not reset per provisional.
Found live (T072 pass 3): a single flat 15s timeout for the whole
transaction gave up while the carrier was still setting up the call —
including an 18s gap between `100 Trying` and the next provisional
response, apparently carrier-side routing rather than the callee's own
ring time — and the real, eventual `200 OK` landed as "received response
outside an active transaction," after Agent A had already given up and
told Agent B `CallFailed`.

## Which path Agent B chooses (CS vs. VoWiFi/VoLTE)

Agent B's `run_outbound_listener` (shipped, `vowifi/mod.rs`) already picks
among idle lines when dispatching `ControlCmd::Dial { slot: None, .. }` to
the daemon. This contract does not change that selection logic
(`sip::outbound::select_idle_line`, no path preference — FR-007); it adds a
second dispatch target for when the selected idle line's path is
VoWiFi/VoLTE rather than circuit-switched: `PlaceCall` over this protocol to
*this line's own* Agent A, instead of `ControlCmd::Dial` to the daemon.
Concretely: `sip::outbound::CandidateLine.path` (already modeled,
data-model.md) decides which dispatch mechanism a given idle line uses: `CS`
→ `ControlCmd::Dial`; `VoWifi`/`Volte` → `PlaceCall` to that line's Agent A.

## Compatibility

A deployment with `[outbound].enabled = false` never sends `PlaceCall` —
Agent A gains a handler for it in the control-message dispatch loop, but
that loop already ignores message variants it has no reason to receive; no
behavior changes for the message types that already exist.
