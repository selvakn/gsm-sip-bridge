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
  SSRC change logs it (assert via `tracing`'s test-capture, matching this
  codebase's existing log-assertion patterns) and every packet still
  reaches the destination socket.
- `sdp.rs`: an offer with `a=ptime:` produces an answer stating the same
  value; an offer without one still gets the existing default — same
  const-fixture-plus-`sdp.contains(...)` style as `maxptime`'s own tests.

## Hardware round

Same rig and pattern as batches 1-4 (`test/`, on-host EC20 line): rebuild
and retag the image, redeploy, re-register the real line, drive a real
inbound call and confirm no regression — an ordinary call (matching DTMF
PTs on both legs, no SSRC change, no explicit `ptime`) should behave
byte-for-byte identically to before this feature.

The three new behaviors (DTMF relabeling when PTs actually differ, an
SSRC change, a non-default `ptime`) are not things an ordinary phone or
this project's carriers have been observed producing — same posture
already accepted for batches 3 and 4's least-observed findings. If
`siptest` can be made to negotiate mismatched DTMF PTs or send a
non-default `ptime` without risk to the real line, exercise it; otherwise
record in the tracking doc that these remain unit-test-only.
