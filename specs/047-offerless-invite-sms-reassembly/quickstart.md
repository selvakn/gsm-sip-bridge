# Quickstart / Verification Plan

## Automated

`make format && make lint && make test` — the mandatory pre-commit gate
(CLAUDE.md), covering the whole workspace including test targets. All new
logic in this batch is pure and directly unit-tested per the Integration-
First Testing principle:

- `ims::sdp`: the `offerless_invite`/no-`m=`-line branch is exercised at
  the `handle_invite`-adjacent level with a synthetic empty-body request;
  `build_offer`/`parse_answer` themselves are unchanged and already
  covered by the origination path's existing tests.
- `ims::sms_pdu`: `parse_concatenation_udh`'s new reference-carrying return
  shape, direct cases for both IEI widths (8-bit and 16-bit reference).
- `volte::sms::Reassembly`: `admit_part` — completes on the last part
  regardless of arrival order, tells apart two concurrent same-sender
  same-total messages by reference, `Malformed` on a bad total/seq,
  idempotent re-admission of an already-held seq, `take_expired` at/under/
  over the 3-minute bound.

## What real hardware can and can't confirm this round

**SMS-05 is directly testable live**: send a text from the user's own
phone long enough to be split into multiple parts (roughly >70 GSM7
characters, or >134 with a UCS2 body per the fixed per-part overhead a
concatenation UDH adds). Confirm the line delivers one joined message, not
several `[N/M]`-labelled fragments — same test shape as the SMS-EMOJI-01
live-verification round.

**SDP-04 is not directly testable live** — this project has observed no
carrier or device sending an offerless INVITE to this line across every
prior hardware round (batches 1–7's own logs). Verifying the new branch
therefore stays unit-test-only this round, the same posture prior batches
recorded for their own low-probability paths (batch 4's non-default
direction/transport declines, batch 6's malformed-body case) — this is not
a gap specific to this feature.

**Regression check (both findings)**: one ordinary real inbound call
(offer present, as always) and one ordinary single-part SMS must behave
exactly as before — SC-005. Use the existing `siptest` PBX-extension
answer flow already exercised by every prior batch's hardware round.

## Recording the result

Whatever this round finds — live confirmation, or "still unit-test-only,
no carrier/device here has been observed doing this" — gets written into
`docs/plans/mt-conformance-findings.md`'s batch 8 entry either way, per
this project's established practice of recording a negative or
unexercised result rather than letting the doc imply a broader
verification than actually happened.
