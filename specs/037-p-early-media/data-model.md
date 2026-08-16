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

New fields, as built:

| Field | Type | Meaning |
|---|---|---|
| `early_veth_rx` | `Option<mpsc::Receiver<BridgeResult<VethUasResult>>>` | The veth UAS listener's receiver, if spawned early. Consulted (not blindly consumed — see below) instead of a fresh `spawn_veth_uas_listener` call when present. |
| `early_media_sent` | `bool` | Guards `CallEarlyMedia` to exactly one attempt per call, mirroring the existing `ringing_relayed` one-shot guard. |

Lifecycle: both start unset. The first SDP-parseable provisional in
`on_carrier_response`'s `resp.status < 200` branch sets both together
(RTP connect + veth spawn + send `CallEarlyMedia` are one atomic step —
none happens without the others).

**Dropped from the original proposal, post-implementation (code review
finding)**: `early_media_rtp_connected` (a bool gating the RTP-connect
step at the real `200 OK`) was removed entirely. Two review findings
against the original design:

1. **Reconnecting the RTP socket at `200 OK` is not actually a gap.**
   `UdpSocket::connect` only updates the kernel's notion of the default
   peer for that socket — a local, instantaneous operation, not I/O.
   Reconnecting to the *same* address (the common case, since `answer`
   usually still comes from the cached `provisional_answer`) costs
   nothing. The proposed skip was solving a non-problem.
2. **Skipping it unconditionally was an actual bug.** Early media does
   not require `Require: 100rel` (FR-008) — an early SDP is therefore not
   always the guaranteed-final answer the way a reliable provisional's
   is. A `200 OK` carrying its own, genuinely different SDP (legal per
   RFC 3264) would have left RTP pointed at a stale address with the skip
   in place. The fix: always `connect()`, exactly as before this feature.

With that fix, `early_media_rtp_connected` had no remaining reader — it
was set and checked together with `early_veth_rx`, so it was fully
redundant with `early_veth_rx.is_some()`. Removed rather than kept as
dead state.

**`early_veth_rx`'s reuse at the real `200 OK` is not unconditional
either (second code review finding)**: its `VETH_INVITE_TIMEOUT` clock
started back at the provisional, not at the real `200 OK` — on a carrier
with a long early-media window (Jio: ~13.7s) it can have already resolved
to a failure by the time `200 OK` arrives. Blindly trusting a stale
failure there would hang up a carrier leg the carrier just successfully
answered, violating FR-006. The real `200 OK` handling now does a
non-blocking `try_recv()` on it first: still-pending → use as-is;
already succeeded → forward the result through a fresh one-shot channel;
already failed/disconnected → fall back to a *fresh*
`spawn_veth_uas_listener` call, giving Agent B a full new window starting
now, exactly as if no early media had ever happened.

**Naming correction from the original proposal**: there is no separate
`finish_origination` function for the `200 OK` leg — that name belongs to
a *different*, pre-existing function that only runs later, once Agent B's
veth call actually arrives (called from `tick_pending_origination`). The
`200 OK` handling that checks `early_media_rtp_connected` and consumes
`early_veth_rx` is inline inside `on_carrier_response` itself, immediately
after the `resp.status != 200` early-return.

## Agent B: outbound attempt local state (`vowifi/mod.rs`)

The existing `try_place_on_line` loop's local variables (currently just
the `call: &mut Call` parameter and the `reader`/`pending_line`) gain a
local `early_veth: Option<Call>` — set once `pair_veth_leg` has paired the
veth leg from the `CallEarlyMedia` arm. `PlaceCallOutcome::Placed` grew a
second field (`Option<Call>`) to carry it out of the function; every
non-`Placed` exit from the poll loop passes it to the new
`abandon_early_veth(endpoint, call, early_veth.take())` helper, which
hangs up and unpairs it if present (a no-op when it's `None` — the
common no-early-media case).

This is the same shape `bridge_outbound_leg` already returned before this
feature (a `Call`) — what changed is *when* it's captured (a second call
site, `CallEarlyMedia`, can now produce it before `CallPlaced` does) and
that `bridge_outbound_leg` itself split into `pair_veth_leg` (make +
pair, no answer) plus the answer step, so both call sites — the original
`CallPlaced`-only path and the new early-paired path — share the pairing
logic instead of duplicating it.

`run_outbound_listener`'s `Placed` handling now dispatches to one of two
finalizers based on whether `early_veth` came back `Some`:
`finalize_paired_outbound_leg` (new — `answer(200)` only, no pairing) or
`bridge_outbound_leg` (unchanged — full pair + `answer(200)`, the
no-early-media path).

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
