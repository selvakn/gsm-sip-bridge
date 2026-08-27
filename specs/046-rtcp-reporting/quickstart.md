# Quickstart: verifying RTCP reporting (RTP-01)

## Unit tests

Most of this feature is pure enough to test without a modem, a carrier, or
a socket — the packet builders and parsers especially. Matching each file's
existing test style:

- **`rtcp.rs` (new), packet building**: an SR states the SSRC, packet and
  octet counts it is given; octet counts exclude RTP headers; a compound
  packet carries SDES with a CNAME after the SR; a leg that has sent
  nothing emits an RR rather than an SR (C-2.7); a BYE names the reported
  SSRC (C-2.8).
- **`rtcp.rs`, packet parsing**: a well-formed RR yields its fraction
  lost, cumulative lost, jitter, LSR and DLSR; a truncated packet is
  rejected rather than half-read; a compound packet containing an
  unfamiliar type still yields the members that *are* understood (C-4.4);
  an APP or XR packet alone is ignored without error.
- **`rtcp.rs`, round-trip derivation**: RTT from a known LSR/DLSR pair; an
  `LSR` of zero yields `None`, not a zero RTT (C-5.2) — the case most
  likely to be got wrong, since zero is a plausible-looking answer.
- **`rtcp.rs`, schedule**: the base interval derives from bandwidth and
  packet size as specified; every randomised interval lands inside the
  ±50% band; the mean over many draws sits near the base (C-3.1-C-3.3).
- **`rtcp.rs`, source validation**: a datagram from the peer IP on an
  unexpected *port* is accepted (Decision 7 — this is the case a
  `connect()`ed socket would have wrongly dropped); one from a different
  IP is rejected before parsing (C-4.1); a report naming an unseen SSRC is
  accepted (C-4.2).
- **`sdp.rs`**: `parse_offer` reads an `a=rtcp` port; a malformed or zero
  value yields `None` (C-1.7); an offer with no `a=rtcp` is unaffected.
- **`sdp.rs`, answer**: an answer with an RTP+1 endpoint is byte-identical
  to today's (C-1.1) — pin this against the existing fixtures, it is the
  main regression guard; an answer with a declared endpoint gains exactly
  one `a=rtcp:` line; an answer with no endpoint still carries
  `b=RS:800`/`b=RR:2400` (C-1.2, C-1.3) — the FR-017 case that the first
  draft of the spec got wrong.
- **`media_stats.rs` / relay paths**: `SendAccounting` counts payload
  octets and packets on both relay implementations; counts do not reset on
  an SSRC change (C-2.5); the pass-through path publishes the observed
  SSRC and the transcoding path publishes its own (C-2.2).
- **Relay non-regression**: audio and DTMF still forward exactly as before
  on both paths, and `SsrcTracker`'s logging is unchanged (C-6.1-C-6.4).
  Extend the existing batch-5 relay tests rather than writing parallel
  ones.
- **`control/protocol.rs` + `metrics/ingest.rs`**: the new
  `ObservedEvent::MediaQuality` round-trips through serialization and
  reaches the right metric with the right label values; every label-bearing
  field is a closed enum (C-5.6).
- **`tests/test_metric_renames.rs`**: the new metric names are covered, per
  the suite's existing pattern.

## What needs a live socket

Two things cannot be unit-tested in this codebase's style and are
hardware-or-nothing, same constraint batch 3 recorded for its retransmit
branches:

- The three-tier port bind actually selecting a tier (it depends on what
  the OS hands out).
- The RTCP thread's full loop against a real peer — reports leaving on
  schedule, reports arriving, the BYE on teardown.

## Hardware round

Same rig and pattern as batches 1-6 (`test/`, on-host EC20 line): rebuild
and retag, redeploy, re-register the real line, drive a real inbound call.

**What to check, in priority order:**

1. **No regression.** The call rings, answers, carries `media="both-ways"`,
   and ends cleanly — the bar every prior batch was held to. This feature
   binds a new socket and spawns a new thread on the answer path, so an
   ordinary successful call is a meaningful test on its own.
2. **Which tier was taken.** The log should say. Tier 1 (RTP+1) is the
   expected outcome; tier 2 appearing routinely would mean the retry bound
   is wrong. Tier 3 appearing at all needs investigating.
3. **Reports leaving.** Capture on the carrier leg and confirm SRs at
   roughly one per second (the `b=RS:800` derivation), with the same SSRC
   as the RTP alongside them and counts that grow.
4. **Reports arriving.** Whether the carrier sends RR at all is the open
   question this feature cannot answer in advance — Vi and Jio both declare
   RTCP bandwidth in their own SDP, so they should. If they do, confirm
   the loss/jitter figures reach the end-of-call log line and the metrics.
   **If they do not, that is itself the finding** — record it, because it
   means one-way-audio investigations still have no far-end evidence and
   US2's value was not realised.
5. **The BYE on teardown.** Confirm it goes out and that hangup is not
   perceptibly slower (SC-007).

**Note on the codec path.** As in every prior round, an AMR-WB call takes
the *transcoding* relay. The pass-through relay only runs when both legs
negotiate PCMU, which most real calls on this line have not done. So the
pass-through path's SSRC observation (C-2.2, second row) will most likely
remain unit-tested only — the same posture batches 5 and 6 accepted for
`veth::forward` changes, and worth stating in the findings doc rather than
implying full coverage.

## Definition of done

- `make format && make lint && make test` clean across the whole
  workspace, including test targets, clippy `-D warnings`.
- A real inbound call verified per the list above.
- `docs/plans/mt-conformance-findings.md` updated: RTP-01 and SDP-06's
  `a=rtcp` half moved from deferred to landed, **with FR-023a's residue
  recorded explicitly** — the internal veth leg still declares RTCP
  bandwidth it does not back, and the originated-call path still has no
  RTCP at all. Neither is closed by this feature, and the doc must not
  read as if RTP-01 were closed everywhere.
