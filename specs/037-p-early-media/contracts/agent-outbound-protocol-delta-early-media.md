# Contract delta: outbound early media

Amends `specs/025-outbound-calling/contracts/agent-outbound-protocol.md`
(itself amended by `specs/029-interruptible-origination-wait/contracts/agent-outbound-protocol-delta.md`).
Only the differences are stated here; everything not mentioned is
unchanged.

**Wire format: one new variant.** `CallEarlyMedia { call_id: String }`,
same tagged-JSON shape as every other `ControlMessage` variant
(`#[serde(tag = "event", rename_all = "snake_case")]`):

```json
{"event": "call_early_media", "call_id": "out-42"}
```

---

## 1. New message: `CallEarlyMedia`

**Direction**: Agent A → Agent B.

**Sent**: At most once per call attempt (one-shot, like today's
`CallRinging`) — the first time the carrier's response to the outbound
INVITE is a provisional (`180`–`183`) whose body parses as SDP. May arrive
before, after, or in place of `CallRinging` for the same attempt (a
carrier can send early media on the very first provisional, or ring
plainly first and add SDP on a later one — either order is legal).

**Agent A obligations before sending**:

1. Parse the provisional's SDP body and `connect()` the carrier-facing RTP
   socket to the address it names — the same connect `finish_origination`
   already performs at `200 OK`, just done here instead, once.
2. Spawn the veth UAS listener (`spawn_veth_uas_listener`, unmodified) and
   retain its receiver for `finish_origination` to consume later — do not
   spawn a second one at the real `200 OK`.
3. Send `CallEarlyMedia{call_id}`.

**Agent B obligations on receipt**:

1. Place the veth-side `Call::make` toward Agent A's now-waiting listener
   and `pair_calls` it to the already-accepted local (phone/PBX) call —
   the same two steps `bridge_outbound_leg` already performs at
   `CallPlaced`, extracted so both call sites share them (`pair_veth_leg`
   in `data-model.md`).
2. `call.answer(183)` on the local leg instead of `180` — PJSIP builds the
   SDP answer itself from the now-paired conference-bridge slot, per the
   existing early-pairing precedent (`bridge_call`'s inbound pairing,
   which also happens before either leg is answered).
3. Retain the resulting veth `Call` so the eventual `CallPlaced` (or
   `CallFailed`) for this attempt does not repeat step 1.

**If `CallEarlyMedia` never arrives for an attempt** (carrier sends no
SDP-bearing provisional): behavior is completely unchanged from
`specs/025`/`specs/029` — `CallPlaced` does the full `pair_veth_leg` +
`answer(200)` sequence exactly as it does today.

---

## 2. `CallPlaced` — same message, reinterpreted when early media preceded it

**Before** (specs/025/029): `CallPlaced` always meant "place and pair the
veth leg now, then `answer(200)`."

**After**: if this attempt already received `CallEarlyMedia` (i.e. Agent B
already holds a paired veth `Call` for it), `CallPlaced` means only
`call.answer(200)` on the already-paired local leg — no new `Call::make`,
no re-pairing. If it did not, `CallPlaced` behaves exactly as before.

No new field is needed to signal this — Agent B already knows locally
whether it paired early, from its own state (`data-model.md`'s
`paired_veth_call`).

---

## 3. Progress table — one added row, one reinterpreted row

| Agent A sends | Agent B does | Phase |
|---|---|---|
| `CallAttempting` | switch to the long wait | unchanged |
| `CallRinging` | `answer(180)` — caller hears ringback (only if `CallEarlyMedia` hasn't already answered `183`) | unchanged |
| **`CallEarlyMedia`** | **pair the veth leg now; `answer(183)` — caller hears the carrier's pre-answer audio** | **NEW** |
| `CallPlaced` | if already paired (`CallEarlyMedia` fired): `answer(200)` only. Otherwise: pair + `answer(200)`, as before | **reinterpreted** |
| `CallFailed` | answer the phone leg with a mapped status; hang up the paired veth leg if one exists (new — see §4) | reinterpreted |

`CallRinging` and `CallEarlyMedia` are independent one-shot flags for the
same attempt — a carrier can trigger either, both (in either order), or
neither.

---

## 4. Teardown: `CallFailed`/`CallEnded` reaching an early-paired attempt

Before this feature, nothing needed tearing down on Agent B before
`CallPlaced` — the local leg was only ever plainly ringing, no veth leg
existed yet. Now a `CallFailed` or a caller-initiated `CallEnded` can
arrive while a veth leg is already paired to the local leg.

- **Carrier fails/cancels while early media is active** (Agent A's
  `fail()` → `AwaitingCancel` path): Agent A sends `CallFailed{call_id,
  reason}` exactly as it does today for a plain-ringing failure — the
  message is unchanged, only *when* it can now legally arrive (mid-early-media,
  not only pre-`CallEarlyMedia`) is new. Agent B, on `CallFailed`, hangs
  up the paired veth `Call` if one exists (new) in addition to answering
  the local leg with the mapped failure status — `Call::hangup()` is
  state-agnostic (`pjsua-safe/src/call.rs`), so this is not new PJSIP
  surface, just a new call site.
- **Caller abandons while early media is active**: unchanged message
  (`CallEnded` from Agent B to Agent A, per `specs/029`'s delta — already
  legal at any point after `PlaceCall`). Agent A's handling is unchanged;
  it already sends `CANCEL`/`BYE` as appropriate regardless of whether
  early media had started.

---

## 5. Timing

No new timeouts. `VETH_INVITE_TIMEOUT` (5s) now bounds the veth handshake
from `CallEarlyMedia`'s trigger point instead of (or in addition to, if
early media never fires) `CallPlaced`'s — same constant, earlier possible
start.
