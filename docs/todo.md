
Observed pending items
----------------------

- [x] ~~RFC 4028 session refresh (`Supported: timer`) is not implemented.~~
      — **implemented 2026-09-04, specs/049-session-timer-refresh.** The
      outbound (UAC) leg's `200 OK` handling (`agent/origination.rs`) now
      reads `Session-Expires` regardless of what this bridge itself
      advertised (`[vowifi] originating_headers` stays unchanged, off by
      default — purely reactive), resolves the refresher role per RFC 4028
      §7.2 (an explicit `refresher` param used as-is; a non-compliant
      response that omits it defensively defaults to this bridge itself,
      guaranteeing the call survives either way), and either sends this
      bridge's own periodic `UPDATE` refresh at half the interval (§7.4) or
      accepts the carrier's own in-dialog `UPDATE` refresh — a new, small,
      pure state machine (`agent/session_refresh.rs`, mirroring
      `agent/ping.rs`'s `PingState` shape) drives both directions, hooked
      into the dispatch loop the same way `handle_attachment_loss` already
      is. A failed refresh, in either direction, ends the call with a
      distinct, diagnosable `EndedBy::SessionTimerExpired` /
      `reason::SESSION_TIMER_EXPIRED` rather than a silent drop at the
      interval.

      **Scope decisions** (see `specs/049-session-timer-refresh/research.md`
      for the full rationale, each grounded in the actual RFC 4028 text,
      not recalled from memory): refresh transport is `UPDATE` only, both
      directions — no re-INVITE-based refresh is built, since RFC 4028 §7.4
      itself recommends `UPDATE` over re-INVITE and a bodyless `UPDATE`
      avoids touching this bridge's SDP offer/answer machinery entirely. A
      failed refresh (timeout or any non-2xx) is fatal on the first
      attempt — no per-response-code retry ladder, since no carrier
      reachable here has ever required `timer` on a call that actually
      connects (Jio's `183` still never reaches `200 OK` — the "kept open"
      disposition below is now moot, since the feature was built ahead of
      either of its two original triggers, at the user's explicit
      request).

      **Verification**: fixture-driven unit tests throughout (`cargo test`
      — the state machine, the new `UPDATE` builder, the `200 OK`
      capture, and the dispatch-loop accept/decline paths), plus a
      regression-only hardware round — no carrier reachable here has ever
      sent `Session-Expires` on a connecting call, so, like specs/048's
      MT-06, the new logic itself cannot be exercised live regardless of
      effort spent trying (`specs/049-session-timer-refresh/quickstart.md`).

- [x] ~~Outbounds calls via the GSM (PC / VoLTE / VoWIFI), from pbx as well as
      sip client~~ — **triaged 2026-08-06; closed 2026-09-04.** CS, VoWiFi, and
      PC/SC were already audio-verified on real hardware
      (specs/025-outbound-calling T023/T072/T073); VoLTE was the last
      unverified path (T050e left this deliberately open) and is now done.

      **What was actually wrong** (found across two passes the same day — the
      first pass's "no dispatcher exists for VoLTE at all" was too broad, see
      `specs/025-outbound-calling/tasks.md` T050e for that history): with
      `[volte].bridge_inbound=true`, `orchestrate_volte::start_multiline`
      already spawns `volte-bridge`, which calls the *same*
      `vowifi::run_telephony_side` function VoWiFi's Agent B uses — registrar,
      `run_outbound_listener`, everything. The dispatcher was never missing;
      the actual bug was that `RuntimeLine` (what `try_place_on_line` iterates
      over) had no `status_port` field, so every VoLTE line's `PlaceCall`
      dialed VoWiFi's fixed `AGENT_A_STATUS_PORT` (5071) instead of its own
      per-line-derived port (5076/5080/5084/…). **Fixed**: added
      `RuntimeLine.status_port`, threaded through the VoWiFi and VoLTE
      (`BridgeLine::to_runtime_line`, new) construction sites, used by
      `try_place_on_line`/`print_status`. New regression tests. Outbound
      calling over VoLTE requires `[volte].bridge_inbound=true` (the legacy
      registration-only default still has no route to outbound, by design).

      **A second, unrelated bug found and fixed along the way**: the same
      live-verification session hit "IMS PDN attached but the carrier's router
      advertisement was never accepted" (`routed=false`). Root cause: `AT+QNETDEVCTL=1,<cid>,1`
      (`volte::pdn::bring_up`) — the command that actually makes the host
      interface transition and regain carrier — is only issued when the modem
      doesn't already report that context bound (`AT+QNETDEVCTL?`). A prior
      session's `volte-register` run left context 3 marked bound on the modem
      without a clean detach, so the next attach silently skipped the rebind
      and the interface never got a fresh carrier/DAD/RA cycle — persistent
      across retries and process restarts, since the retry loop calls
      `attach()` again but the modem-side binding doesn't self-clear. Not a
      code bug (no fix needed) — `gsm-sip-bridge volte-pdn --action down`
      clears it (issues the unbind), confirmed via a raw `AT+QNETDEVCTL?`
      probe showing `0,0,0,0` before and after. Recorded in case it recurs:
      `docs/operations.md`'s "Symptom: attached but nothing works" section
      already documents the check (`AT+QNETDEVCTL?`), just not this specific
      stale-binding cause.

      **Live-verified, full pass, 2026-09-04**: local Vodafone/EC200U rig,
      `volte-bridge --modem` diagnostic path (fixed build), `[sip_server]`
      mode, a `siptest`-registered phone dialing out. Real IMS registration
      (`REGISTER response status=200`), real outbound INVITE to the carrier
      (100 → 183 → 180 → 200 OK), real bidirectional audio — `siptest`
      reported `direction: both ways`, `success: true`, AMR-WB↔L16
      transcoding relay running both directions. Corroborated independently by
      the daemon's own logs (`call media verdict media="both-ways"
      carrier_rx=843 pbx_rx=921 ... outcome="answered"`) and metrics
      (`gsm_sip_bridge_outbound_attempts_total{outcome="placed"} 1`,
      `gsm_sip_bridge_calls_total{status="answered",transport="volte"} 1`,
      `gsm_sip_bridge_volte_registered 1`). Config/image reverted afterward;
      VoWiFi confirmed `Registered`/`gm_connection: up` again.
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
