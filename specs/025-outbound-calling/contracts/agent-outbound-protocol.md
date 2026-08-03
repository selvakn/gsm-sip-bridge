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
    /// no P-CSCF reachability). Agent B answers the phone/PBX leg
    /// accordingly (`486`/`503`, matching `contracts/sip-dialout.md`'s
    /// table) and tears down the accepted leg.
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
   │                                        │  build INVITE over the
   │                                        │  already-registered session
   │                                        │  (R-010) — never a fresh
   │                                        │  register_session call
   │                                        │
   │                                        │  ── INVITE ──▶ carrier
   │                                        │  ◀── 180/200 ──
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
outcome, just carried over this channel instead of `ControlCmd`.

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
