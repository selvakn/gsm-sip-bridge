
Observed pending items
----------------------

- [ ] **RFC 4028 session refresh (`Supported: timer`) is not implemented.**
      We never advertise `timer`, so nothing is broken today — but Jio's own
      `183` carries `Require: timer` and `Session-Expires: 300` *unprompted*,
      i.e. it will demand refreshes on a call that actually connects. If a
      carrier ever gets a 2xx to us with `Require: timer`, RFC 4028 §7.4 makes
      the refresh our obligation and a connected call would drop at the
      session interval. Two consequences:
      - `[vowifi] originating_headers = ["supported"]` advertises `timer` and
        is therefore a promise this client cannot keep. It is off by default
        and documented as a hazard; it must not be turned on for a carrier
        whose calls connect until this is implemented.
      - Confirmed live on Jio 2026-08-24: advertising `Supported: 100rel, timer`
        made Jio escalate its `183` from `Require: timer` to
        `Require: timer,100rel`, so the carrier does act on what we advertise.
      Work: honour `Session-Expires`/`Min-SE` on the INVITE and its responses,
      pick the refresher per §7.1, and send a re-INVITE or `UPDATE` at half the
      interval. See
      [docs/plans/jio-vowifi-outbound-480-followup.md](plans/jio-vowifi-outbound-480-followup.md).
      Origin: the same "don't advertise extensions you don't implement" lesson
      that produced the PRACK bug (`specs/037-p-early-media`).

- [ ] Outbounds calls via the GSM (PC / VoLTE / VoWIFI), from pbx as well as sip client
      — **triaged 2026-08-06**: CS, VoWiFi, and PC/SC are audio-verified on real
      hardware (specs/025-outbound-calling T023/T072/T073); VoLTE specifically has
      never been independently exercised for *outbound* calling (T050e left this
      deliberately open). Believed to work — shares `ims::agent`'s origination code
      with VoWiFi — but unconfirmed. Plan: [docs/plans/volte-outbound-verification.md](plans/volte-outbound-verification.md).
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
- [x] ~~Explore if caller identity can carry the name as well in the vowifi /
      volte stacks~~ — **implemented and live-verified 2026-09-03.** Indian
      carriers already send the TRAI-mandated CNAP name unprompted, as the
      SIP display-name on `From`/`P-Asserted-Identity` — confirmed on a real
      inbound VoWiFi INVITE. The bridge was discarding it
      (`ims::session::extract_caller` kept only the URI's user part), so both
      onward legs showed the number, or the number repeated as the "name".

      Added `ims::session::extract_caller_name` (same PAI-then-From
      precedence as `extract_caller`) and `caller_identity_is_private`
      (RFC 3325 `Privacy: id`/`user` withholding); carried the name Agent A →
      Agent B via a new `ControlMessage::IncomingCall.caller_name` field
      (`#[serde(default)]` for wire compatibility); `vowifi::bridge_call` now
      puts it in the `P-Asserted-Identity` display name (was the number
      repeated) plus a new `X-GSM-Caller-Name` header, and it's what
      `Account::set_identity` shows as the `From` display name in
      SIP-server mode. Falls back to today's number-only behavior when a
      carrier sends no name. Covers VoWiFi and VoLTE (shared
      `ims::agent`/`bridge_call` path); circuit-switched is unaffected —
      `+CLIP`'s `<alpha>` field is a modem phonebook match, not network CNAP.

      Live-verified both paths: a real inbound call from a second (Jio) line
      to the local rig, captured with `RUST_LOG=...sip_client=trace` plus a
      `tcpdump`/`tshark` capture of the outbound PBX-leg INVITE, showed
      `P-Asserted-Identity: "Firstname Lastname" <tel:+919000000000>` and
      `X-GSM-Caller-Name: Firstname Lastname` (previously the number on
      both). With `[sip_server]` enabled and `siptest` registered as the
      ringing account, the same call's `GET /calls` reported
      `From: "Firstname Lastname" <sip:+919000000000@...>` — the registered
      phone now sees the real name.

      No spam-indication field exists in the wire capture (no `Identity`/
      RFC 8224, no `verstat`, no vendor `X-` header) — India runs no
      STIR/SHAKEN; TRAI's anti-spam design is CNAP itself, plus the `140`-
      prefix telemarketing/`1600`-prefix transactional numbering convention,
      already available in the calling number this bridge extracts.
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
