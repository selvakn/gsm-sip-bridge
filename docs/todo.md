
Observed pending items
----------------------

- [ ] `siptest` (specs/037-siptest-softphone) has no unified dialog engine —
      T026/T037 in that spec's task list. Registration runs as a blocking
      function in its own background thread, outbound calls run
      synchronously inside the HTTP handler via `spawn_blocking`, and
      inbound calls run on their own dedicated listener thread, instead of
      one `step(Input) -> Vec<Output>` state machine driving a shared
      per-call dialog table. Every documented *behaviour* (302 handling,
      digest auth, CANCEL/timeout, caller-ID capture) is implemented and
      tested regardless. Concrete consequence: no second concurrent dialog
      can be handled — an inbound call arriving while an outbound call is
      already mid-flight has no path to be processed, since the inbound
      listener thread is blocked inside the first call. Fine for the
      current single-call-at-a-time scope; would block true concurrent-call
      support if that's ever needed.
- [ ] `siptest` (specs/037-siptest-softphone) has no early-media (`18x` +
      SDP) support — found live while verifying specs/037-p-early-media
      (2026-08-16). `sip/outbound.rs`'s response loop explicitly skips the
      body on every `180`/`183` and only calls `sdp::parse_answer`/starts a
      media session on `200`, so a `siptest call` against a carrier that
      answers with early media (exactly the Jio case above) always reports
      zero packets sent *and* received — not because the bridge dropped
      anything, but because siptest never opens a socket for it in the
      first place. Confirmed via IPsec SA packet counters
      (`swanctl --list-sas`, counted pre-decryption) that the carrier really
      was sending ~50 pkt/s of real audio the whole time; a real phone
      (which implements early media, unlike siptest) heard it fine on the
      same build. Net effect: `siptest` cannot currently be used to verify
      early-media relay behavior end-to-end — only signaling-level checks
      (`invite_to_180_ms` etc.) and post-`200` media. Fixing this means
      teaching the outbound-call loop to treat a `180`/`183` carrying SDP as
      "start the media session now" (RFC 3262/5009 early-media UAC
      behavior), the same asymmetry `ims::agent::origination` had to learn
      on the bridge side for this exact feature.
- [ ] Jio VoWiFi outbound (MO) calls always end in `480` — **triaged
      2026-08-24**: every outbound call on the Jio line reaches an MSML
      media server, plays ~13.4s of early media, then gets `480
      Temporarily Unavailable` — regardless of destination (confirmed
      against two unrelated numbers, byte-identical signature). Two
      external-review theories (missing PRACK, missing UPDATE/qos
      preconditions, malformed `P-Access-Network-Info`) were checked
      against the live capture and ruled out — none fit the evidence.
      Working theory: an account/SIM entitlement gap on Jio's side (MT
      voice + SMS provisioned, MO voice not), not a client-side defect.
      Plan: [docs/plans/jio-vowifi-outbound-480.md](plans/jio-vowifi-outbound-480.md).
- [ ] Multi part SMS - combine them and discord notify
- [ ] **SMS-07** — national-language shift tables unimplemented (TS 23.038
      Annex A). Attempted and reversed mid-implementation during batch 6
      (specs/045-long-tail-conformance): the mechanism (recognizing the UDH
      IEs that select a national-language locking/single shift table) is
      small, but the part that fixes decoded text is Annex A's character-
      table data itself, which was not shipped from memory without a
      verifiable source — a wrong mapping would silently decode real text
      to the wrong characters, worse than today's honest, already-
      documented gap. Planned for implementation soon; when picked back up,
      source the Annex A tables from the actual 3GPP TS 23.038 spec text
      (not recalled/approximated), and pin a decode test against a known-
      good vector before touching the general decode path.
      See docs/plans/mt-conformance-findings.md batch 6 for the original
      finding and the reversed attempt.
