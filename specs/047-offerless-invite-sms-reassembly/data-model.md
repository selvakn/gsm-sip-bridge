# Data Model: Offerless Call Answering and Multi-Part SMS Reassembly

## SDP-04: Pending Inbound Call

Not a new persisted type — an extra branch through the existing
`handle_invite` control flow, using types that already exist
(`sdp::SdpOffer`/`SdpAnswer`, `DialogInfo`). Recorded here as data because
the spec's Key Entities section names it and its fields/lifecycle are worth
being explicit about.

| Field | Source | Notes |
| --- | --- | --- |
| `call_id`, `to_tag` | already computed before the branch | identical to the normal path |
| our own offer | `sdp::build_offer(local_ip, ims_rtp_port, session_id, CodecOffer::preferring_wideband(ctx.wideband && amr_safe::is_available()))` | reused unchanged from the origination path (research.md Decision 2) |
| the far end's answer | `sdp::parse_answer(ack.body)` once the ACK arrives | reused unchanged; maps to `ChosenCodec` via the promoted `sdp::offered_chosen_codec` |
| wait deadline | new constant, `OFFERLESS_ACK_TIMEOUT` | a protocol-transaction-scale bound (research.md: this is "did the ACK arrive," not a human-scale wait), independent from `RING_TIMEOUT`; not user-configurable, per the spec's own Assumptions |

**Lifecycle**: `INVITE` (no body) → `100 Trying` → `180 Ringing` → PBX
answered → `200 OK` (our offer) → **either** `ACK` (their answer) arrives
before the deadline → codec compatible → proceeds into the same
`ActiveCall` construction the normal path already ends in, **or** ACK never
arrives / arrives incompatible → `BYE` sent from `DialogInfo`, Agent B told
`CallEnded`, call reported not-answered. No state survives past
`handle_invite` returning, in either outcome — nothing new to clean up on
a later teardown path.

## SMS-05: Reassembly and its buffered entries

### `Reassembly` (new, in `volte::sms`, alongside `Dedupe`)

A map from a multi-part message's identity to its `PartialMessage`, behind
one `Arc<Mutex<Reassembly>>` shared between the IMS `MESSAGE` route and the
modem-storage sweep route — the same sharing shape `Dedupe` already uses,
for the same reason (CS-02: a real message can arrive with different parts
over different routes).

| Concept | Shape | Notes |
| --- | --- | --- |
| Key | `(sender: String, reference: u16)` | the reference is a real field now (research.md Decision 6) — previously discarded by `parse_concatenation_udh`. `total` is **not** part of the key — it is stored in the value and checked for consistency (see `Malformed` below); keying on it too would silently split a message with an internally-inconsistent total into two buffers instead of flagging it. |
| Value | `PartialMessage` | see below |
| Capacity | bounded, mirroring `Dedupe`'s `VecDeque` + eviction shape | a pathological flood of distinct multi-part message identities must not grow this unboundedly between sweeps; oldest-first eviction, same as `Dedupe` |

### `PartialMessage`

| Field | Type | Notes |
| --- | --- | --- |
| `total` | `u8` | from the first part seen; every later part for this key must agree, or the mismatch makes the message `Malformed` (FR-016) |
| `parts` | map `seq (u8) → text (String)` | sparse — only positions actually received |
| `rp_mr` per part *not* stored here | — | the per-part delivery ack (FR-012/Decision 10) happens at admission time, before/independent of this struct, so it never needs to be replayed later |
| `last_updated` | `Instant` | reset on every admitted part (including a duplicate re-admission of an already-held seq — see Decision 9's retry note); what `expire_due` compares against the 3-minute bound (`SC-004`/FR-013) |

### `PartOutcome` (what `admit_part` returns)

| Variant | Meaning | Caller's action |
| --- | --- | --- |
| `Complete(String)` | every position `1..=total` now present | join in order, deliver as one message (Decision 9), **do not** clear the buffer entry until that delivery actually succeeds — mirrors `Dedupe::confirm`/`forget`'s retry-safety shape, so a delivery failure followed by the network's own retransmission of just the triggering part can re-reach `Complete` without needing every other part re-sent |
| `Pending` | still missing at least one position | ack this part now regardless (FR-012); nothing forwarded yet. **Must not** be treated as durably delivered (no `Dedupe::confirm`) — it is sitting only in this process's memory until it either completes or is flushed |
| `Malformed` | `total == 0`, or this part's `seq` is `0` or `> total`, or this part's `total` disagrees with an already-buffered value for the same key | fall back to today's existing per-part delivery (FR-016) — the individual, still-labelled text, exactly as before this feature |

If admitting a part forces an *other*, unrelated buffer out (capacity
eviction, or a detected reference reuse — see below), `admit_part` does
**not** return that content directly — it moves into `Reassembly`'s own
retry queue instead (see `QueuedFlush` below). This is a second-round
correction (code review finding, 2026-08-28, round 1; Greptile finding,
round 2): the first fix had `admit_part` return the flushed content for a
single, immediate, one-shot delivery attempt, which lost the content
outright on a transient control-channel failure — no different from the
eviction-drops-content bug it was meant to close, just with the loss
window narrowed to one unlucky moment instead of removed.

**Reference reuse**: TS 23.040 does not guarantee a concatenation
reference is globally unique (an 8-bit reference wraps at 256), so two
unrelated messages can legally share `(sender, reference, total)` within
one buffer's lifetime. A part landing on an already-filled position with
*different* text than what's held there cannot be a retransmission
(identical text is the ordinary idempotent-retry case, see below) — it is
treated as the old message being superseded: the old buffer's held parts
are queued for delivery, and a fresh buffer starts for the new part. This
is a partial mitigation, not a complete detector: two colliding messages
whose parts happen to land on disjoint positions (no position ever sees
conflicting text) cannot be told apart from the PDU alone — recorded as a
residue, not silently assumed handled.

**Explicit non-goal**: `PartOutcome` does not need a `Duplicate` variant of
its own. A part physically retransmitted by the network is already caught
one layer up, by the existing `Dedupe` check on `(sender, labelled body)` —
`admit_part` is only ever reached for a part `Dedupe` has not already seen,
so FR-014 ("must not double-count a repeated part") is satisfied by a layer
that already exists, not by new logic here. The one case `admit_part` does
see the same `seq` twice is the retry-after-failed-delivery path above,
where re-admitting the identical text at the identical position is a
no-op that still correctly reports `Complete`.

### `QueuedFlush` and the retry queue (`Reassembly::pending`)

Every path that gives up on a buffer without an ordinary `Complete`
delivery — capacity eviction, reference reuse, or `expire_due`'s 3-minute
timeout — moves its held parts into one shared internal queue instead of
handing them to the caller for a single attempt. Each entry carries a
small monotonic `id` (`u64`) so a caller can acknowledge exactly the ones
that succeeded without disturbing the rest.

| Method | Effect |
| --- | --- |
| `expire_due(now)` | scans `buffers` for anything past `REASSEMBLY_TIMEOUT`, moves it into the queue. Does not touch already-queued entries. |
| `ready_for_delivery()` | a non-destructive snapshot of everything currently queued — `(id, sender, sequence, total, text)` per entry. Safe to call repeatedly; nothing is removed. |
| `mark_flush_delivered(id)` | removes one entry — call only once its delivery has actually succeeded. |

The caller (`agent::mod::flush_expired_reassembly`, driven by
`LoopState::on_idle_tick`, ~1s cadence) calls `expire_due` then
`ready_for_delivery` every tick, attempts delivery for each entry, and
`mark_flush_delivered`s only the ones that succeeded. Anything left stays
queued and is retried the next tick — a transient control-channel failure
self-heals within about a second instead of losing the content on the
first attempt. `Dedupe`'s claim for a still-queued (not yet delivered)
entry is deliberately left untouched on a failed attempt (no `forget`) to
avoid racing the modem-storage route into independently relaying the same
content while this bridge's own retry is about to succeed.

## Changed existing type: `DecodedSms.part`

| Before | After |
| --- | --- |
| `Option<(u8, u8)>` — `(sequence, total)` | `Option<ConcatPart>` — `{ reference: u16, sequence: u8, total: u8 }` |

`parse_concatenation_udh` already reads the reference byte(s) for both the
8-bit (`0x00`) and 16-bit (`0x08`) concatenation IEIs; it is changed to
return them instead of discarding them (research.md Decision 6). Every
existing match on the old tuple shape (production code and tests) is
updated mechanically — no behavior changes for a single-part message
(`part` stays `None`), and the existing `[seq/total]` label callers keep
reading `.sequence`/`.total` off the new struct.

## Relationships

```text
SIP MESSAGE (IMS)  ──┐                              ┌── ControlMessage::SmsReceived
                      ├── decode (ims::sms_pdu) ──┤   (single body: joined-if-reassembled,
modem AT+CMGL (CS)  ──┘                              │    labelled-if-not, unchanged shape)
                                                      │
                          Dedupe (existing, unchanged)
                          — suppresses a retransmitted
                            identical part —
                                    │
                            admitted (fresh) part
                                    │
                         Reassembly::admit_part
                          (new; keyed by sender+
                           reference+total)
                                    │
                    Pending ──ack, hold── / ──Complete/Malformed── deliver
                                    │
                    eviction / reference reuse / expire_due
                          → Reassembly's retry queue
                                    │
                       (on_idle_tick, ~1s) ready_for_delivery
                          → per-part fallback delivery
                          → mark_flush_delivered on success,
                            else retried next tick
```
