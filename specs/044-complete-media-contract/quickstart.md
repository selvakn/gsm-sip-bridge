# Quickstart: verifying batch 5 (RTP-03, RTP-04, SDP-06 ptime)

## Unit tests

All new logic is exercised without a modem or a real carrier:

- `agent/veth.rs`: extend the existing
  `relay_rtp_forwards_packets_in_both_directions_until_stopped` style —
  build real RTP packets via `rtp::build_packet` with differing DTMF PTs
  on each side, confirm the payload-type byte is rewritten only for DTMF
  and only when the two differ; confirm an ordinary audio packet and a
  matching-PT DTMF packet both pass through byte-for-byte.
- `agent/veth.rs` and `transcode.rs`: a real RTP stream with a mid-stream
  SSRC change still delivers every packet to the destination socket — this
  codebase has no log-capture test infrastructure (no `tracing-test` or
  equivalent), so the behavioral guarantee (nothing dropped) is what's
  asserted, not the log line itself.
- `sdp.rs`: confirms the answer states its own fixed `a=ptime:20`
  regardless of what the offer's own `a=ptime` says — the original plan
  (echo the offer's value) was reversed on inspection; see this feature's
  `research.md` Decision 4.

## Hardware round

Same rig and pattern as batches 1-4 (`test/`, on-host EC20 line): rebuild
and retag the image, redeploy, re-register the real line, drive a real
inbound call and confirm no regression — an ordinary call (matching DTMF
PTs on both legs, no SSRC change) should behave byte-for-byte identically
to before this feature. `ptime` needs no live check at all — it's
confirmed fixed regardless of the offer, so there's no behavior variant
to exercise.

The two behaviors that do vary by offer/call shape (DTMF relabeling when
PTs actually differ, an SSRC change) are not things an ordinary phone or
this project's carriers have been observed producing — same posture
already accepted for batches 3 and 4's least-observed findings. If
`siptest` can be made to negotiate mismatched DTMF PTs without risk to the
real line, exercise it; otherwise record in the tracking doc that these
remain unit-test-only.
