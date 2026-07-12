# Control Protocol: Agent A (IMS/VoWiFi) ↔ Agent B (SIP/PBX)

**Feature**: 011-vowifi-sip-bridge | **Date**: 2026-07-12

Mirrors the shape of the existing CLI↔daemon control protocol
(`specs/009-gsm-resiliency-cli/contracts/control-protocol.md`,
`gsm-sip-bridge/src/control/protocol.rs`) so the same newline-framed-JSON pattern, and largely the
same `read_cmd`/`write_resp`-style helpers, can be reused rather than inventing a new wire format.

## Transport

- **Mechanism**: TCP socket over the dedicated `veth` pair (Agent A's veth address : a fixed port).
  TCP (not a Unix domain socket) because the two agents are different processes in different
  network namespaces — a Unix socket path can't cross that boundary, but the veth link can.
- **Framing**: Newline-terminated JSON, one message per line — identical framing convention to
  `control/protocol.rs::read_cmd`/`write_resp`.
- **Direction**: Bidirectional. Agent A is the one that learns about inbound calls first (it owns
  the IMS leg), so it initiates `IncomingCall`. Agent B reports the outcome of its side back.
- **Cardinality**: One `Bridged Call` in flight at a time (per the spec's single-line assumption);
  the protocol does not need call-ID multiplexing beyond echoing `call_id` for log correlation.

## Messages: Agent A → Agent B

### `incoming_call`

```json
{"event": "incoming_call", "call_id": "a1b2c3", "caller": "+919789063708"}
```

Sent the moment Agent A receives an inbound `INVITE` over the Gm-protected IMS transport and has
parsed the offer. Agent B must respond with `bridge_ready` or `bridge_failed` (see below) before
Agent A sends its own SIP response (`180 Ringing` / `200 OK` / `486 Busy Here`) to the carrier.

### `call_ended`

```json
{"event": "call_ended", "call_id": "a1b2c3", "reason": "caller_hangup"}
```

Sent when the IMS leg receives a `BYE` from the carrier side, so Agent B can tear down its own two
legs (FR-005). `reason` is one of `caller_hangup`, `transport_error`.

## Messages: Agent B → Agent A

### `bridge_ready`

```json
{"event": "bridge_ready", "call_id": "a1b2c3", "veth_rtp_port": 40100}
```

Sent once Agent B has placed its own outbound call to the PBX **and** to Agent A across the veth
(the second leg, answered by Agent A's own lightweight UAS — see `research.md` item 2), and has
conference-bridged the two PJSIP call slots together. `veth_rtp_port` tells Agent A which local
port its own RTP relay should be sending to/receiving from for this call (Agent A's UAS answer
already carries this in ordinary SDP, so this field is mostly for log correlation / a sanity
cross-check rather than the primary signaling path).

### `bridge_failed`

```json
{"event": "bridge_failed", "call_id": "a1b2c3", "reason": "pbx_unreachable"}
```

Sent when Agent B could not establish or bridge the PBX-side leg. `reason` values: `pbx_unreachable`,
`pbx_rejected`, `veth_leg_failed`. On receipt, Agent A declines the inbound `INVITE` with
`486 Busy Here` per FR-010 and the spec's Clarifications answer (fast, explicit rejection).

### `hangup_ack`

```json
{"event": "hangup_ack", "call_id": "a1b2c3"}
```

Confirms Agent B has torn down both of its legs in response to a `call_ended` from Agent A.

## Failure handling

- If Agent B doesn't respond to `incoming_call` within a bounded timeout (short — this sits in the
  critical path of SC-001's 5-second answer target, so the timeout must leave headroom for the
  PBX-leg call setup itself, not consume most of the budget), Agent A treats it as `bridge_failed`
  with reason `veth_leg_failed` and declines the call — never silently drops it (FR-009/FR-010,
  "MUST decline ... rather than ... silently drop").
- If the veth link itself is down (Agent B process not running), Agent A must still decline inbound
  calls cleanly rather than fail to respond to the carrier at all — this is the same "decline with
  a fast, explicit signal" behavior as any other bridge-side failure.

## Why not reuse `control/protocol.rs::ControlCmd` directly

`ControlCmd`/`ControlResp` (existing) model synchronous request→single-response CLI operations
(`card_restart`, `set_mode`, ...). This protocol is asymmetric and event-driven in both directions
(Agent A pushes `incoming_call`/`call_ended` unprompted; Agent B pushes `bridge_ready`/
`bridge_failed`/`hangup_ack` unprompted), so it gets its own small enum pair following the same
serde/newline-JSON conventions rather than overloading the existing request/response shape.
