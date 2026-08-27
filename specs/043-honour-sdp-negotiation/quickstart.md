# Quickstart: verifying batch 4 (SDP-01/02/03, MT-05)

## Unit tests

All of this feature's logic is pure (`sdp.rs`'s parser/builder functions
plus one `agent/inbound.rs` header-echo confirmation) — no live
socket/session harness is needed, unlike batch 3's retransmit-resend
branches. `make test` (whole workspace) is sufficient to exercise every
new function:

- `sdp.rs`'s existing `PJSIP_REAL_VETH_OFFER` fixture (already has a
  trailing `m=text` section) gains an assertion that the answer now
  contains a declined `m=text 0 ...` line.
- A new two-`m=audio`-section fixture proves the *first* is negotiated and
  the *second* is declined, not silently overwritten.
- Direction round-trip tests: each of `sendonly`/`recvonly`/`inactive` in
  an offer produces the mirrored value in the answer; an offer with no
  direction line (or explicit `sendrecv`) still answers `sendrecv`.
- A transport-profile test: an offer whose `m=audio` line says `RTP/SAVP`
  reaches `handle_invite`'s new check and gets declined with the new
  `488`/Warning-305 builder, distinguishable from the existing `488`/
  Warning-304 codec-mismatch decline.
- `agent/inbound.rs`: a confirming test that an inbound INVITE carrying
  `Session-Expires` still produces a `200 OK` with no `Session-Expires` and
  no `Supported: timer` — pinning MT-05's already-correct behavior.

## Hardware round

Same rig and pattern as batches 1-3 (`test/`, on-host EC20 line): rebuild
and retag the image, redeploy, re-register the real line, drive a real
inbound call via the user's phone and confirm no regression (call answers,
audio both ways, clean hangup) — an ordinary handset offer has one audio
section, no direction attribute, and `RTP/AVP`, so it should be answered
byte-for-byte identically to before this feature for that path.

The three new decline paths (extra media section, non-default direction,
unsupported transport) are not things an ordinary phone or this project's
carriers have been observed sending on an initial offer — same posture
already accepted for batch 3's BYE/CANCEL/re-INVITE identity-mismatch
cases, which also went unexercised live and remain covered by unit tests
only. If `siptest` (the softphone stand-in already used for hardware
verification) can be made to send one of these three offer shapes without
risk to the real line, exercise it; otherwise record in the tracking doc
that these three remain unit-test-only, exactly as batch 3's equivalent
gaps are recorded today.
