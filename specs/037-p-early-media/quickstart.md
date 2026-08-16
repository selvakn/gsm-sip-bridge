# Quickstart: Early Media Relay for Outbound Calls

**Feature**: 037-p-early-media

No configuration is added by this feature — it changes what happens
during an outbound call already set up per `specs/025-outbound-calling`'s
quickstart. Use that to get outbound calling working first if it isn't
already.

---

## 1. Verify the no-early-media case is unchanged (regression check)

Place an outbound call through a carrier/line known to go straight from
ringing to answer, with no pre-answer audio. Confirm call setup looks and
sounds exactly as it did before this feature (SC-002) — plain ringback (or
silence, depending on the far end), then answer.

## 2. Verify pre-answer audio is now audible (Story 1 / SC-001, SC-004)

Place an outbound call through a carrier known to send a pre-answer
announcement before answering (observed live on Jio). Confirm:

- The announcement is audible to the caller, starting close to when the
  carrier actually sent it — no need to inspect logs or a packet capture
  to notice it (this was the original diagnostic gap this feature closes).
- When the carrier eventually answers for real, the audio continues
  without a click, restart, or silent gap (SC-005).

## 3. Verify clean abandonment mid-announcement (Story 2 / SC-003)

While the pre-answer announcement from step 2 is still playing, hang up
the caller's leg. Confirm:

- The carrier-side call attempt is abandoned (a `CANCEL` goes out; check
  `vowifi-status` call history for a clean cancelled/abandoned entry, not
  a stuck or still-active line).
- No manual intervention (restart, line reset) is needed before the next
  call on that line.

## 4. Verify clean failure mid-announcement

If reproducible: find a scenario where the carrier plays pre-answer audio
and then fails the call (busy, rejects, or the destination is genuinely
unreachable) instead of answering. Confirm the caller's leg ends cleanly
with no further audio, rather than being left open.

## 5. If the caller hears nothing during a known-early-media carrier

- Confirm the carrier is actually sending a provisional response with an
  SDP body before its `200 OK` (a capture or the bridge's own SIP trace
  logging will show this) — if the carrier's `200 OK` is the first thing
  carrying SDP, there is no pre-answer audio to relay and this is
  expected, unchanged behavior.
- Confirm the local leg answered `183` (not `180`) for that attempt — a
  `180` here means `CallEarlyMedia` didn't fire; check Agent A's logs for
  whether the provisional's SDP body failed to parse.
