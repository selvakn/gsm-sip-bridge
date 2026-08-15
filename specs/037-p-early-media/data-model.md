# Phase 1 Data Model: Early Media Relay for Outbound Calls

No persisted storage is involved (Constitution/Technical Context: N/A).
This documents the in-memory state shape for the one entity the spec
defines, and how it changes on each agent.

## Entity: Outbound call attempt

Per spec.md's Key Entities. State progression, updated for this feature
(new state in **bold**):

```text
attempting → **pre-answer audio** (optional) → answered → ended
attempting → ended                                         (no answer/early media)
attempting → **pre-answer audio** → ended                   (abandoned or carrier
                                                              failed during early media)
```

`pre-answer audio` is optional and skippable — a carrier that never sends
early media goes straight from `attempting` to `answered`, exactly as
today (FR-002, R5).

## Agent A: `PendingOrigination` (origination.rs)

Existing fields relevant here: `provisional_answer: Option<String>`,
`ringing_relayed: bool`, `step: OriginationStep` (unchanged variant set,
per research R4).

New fields (name illustrative, exact naming is an implementation
decision for tasks, not the spec):

| Field | Type | Meaning |
|---|---|---|
| `early_media_rtp_connected` | `bool` | Whether `self.rtp_socket` has already been `connect()`-ed to a carrier-provided address from a provisional response — guards against redoing it at the real `200 OK` (R5). |
| `early_veth_rx` | `Option<mpsc::Receiver<BridgeResult<VethUasResult>>>` | The veth UAS listener's receiver, if spawned early. `finish_origination` consumes this instead of spawning a fresh listener when present. |
| `early_media_sent` | `bool` | Guards `CallEarlyMedia` to exactly one send per call attempt, mirroring the existing `ringing_relayed` one-shot guard. |

Lifecycle: all three start unset. The first SDP-parseable provisional in
`on_carrier_response`'s `resp.status < 200` branch sets all three
together (RTP connect + veth spawn + send `CallEarlyMedia` are one atomic
step — none happens without the others). `finish_origination` checks
`early_media_rtp_connected`: if true, skip the RTP-connect and
veth-listener-spawn steps and consume `early_veth_rx` in place of calling
`spawn_veth_uas_listener` again.

## Agent B: outbound attempt local state (`vowifi/mod.rs`)

The existing `try_place_on_line` loop's local variables (currently just
the `call: &mut Call` parameter and the `reader`/`pending_line`) gain:

| Field | Type | Meaning |
|---|---|---|
| `paired_veth_call` | `Option<Call>` | Set once `pair_veth_leg` has run (either from `CallEarlyMedia` or, for carriers with no early media, from `CallPlaced` directly). `CallPlaced`'s handling checks this before calling `pair_veth_leg` again. |

This is the same shape `bridge_outbound_leg` already returns today (a
`Call`) — the only change is *when* it's captured and that a second call
site (`CallEarlyMedia`) can produce it before `CallPlaced` does.

`ActiveOutboundCall` (used once `try_place_on_line` returns `Placed`)
already carries `call` and `veth_call` — unchanged shape once
`try_place_on_line` finalizes; only the finalizing branch's job shrinks
(construct-and-answer(200) vs. answer(200)-only-on-already-paired-legs).

## New wire message

See `contracts/agent-outbound-protocol-delta-early-media.md` for the full
contract delta. Summary:

```rust
enum ControlMessage {
    // ...existing variants unchanged...

    /// Agent A → Agent B, at most once per call. The carrier's first
    /// SDP-bearing provisional response has arrived; Agent A has already
    /// connected its carrier-side RTP socket and has a veth listener up
    /// and waiting.
    CallEarlyMedia { call_id: String },
}
```
