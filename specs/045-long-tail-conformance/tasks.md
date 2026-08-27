---

description: "Task list for batch 6: the long tail (SIP/SDP/SMS conformance)"
---

# Tasks: The long tail — smaller conformance gaps across SIP, SDP, and SMS

**Input**: Design documents from `/specs/045-long-tail-conformance/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/

## Phase 1: User Story 1 - SIP responses state what's true (MT-11, MT-12, MT-13, Priority: P1)

- [ ] T001 [US1] In `gsm-sip-bridge/src/ims/session.rs`: `extract_caller`
      tries `P-Asserted-Identity` first (same user-part parse `From`
      already uses), falls back to `From` when absent (MT-12).
- [ ] T002 [P] [US1] Test: `extract_caller` prefers the asserted identity
      when both headers are present; unchanged when only `From` is.
- [ ] T003 [US1] In `gsm-sip-bridge/src/ims/session.rs`: add
      `access_network_info: &'a str` to `SubscribeParts`; `build_subscribe`
      echoes it in place of the hardcoded `P-Access-Network-Info: 3GPP-WLAN`
      literal (MT-11).
- [ ] T004 [US1] Thread the value through `subscribe_reg_event` and its two
      call sites (`gsm-sip-bridge/src/ims/agent/mod.rs:731,2172`, both
      already have `reg_cfg`/`p.reg_cfg` in scope).
- [ ] T005 [US1] In `gsm-sip-bridge/src/ims/agent/inbound.rs`:
      `UAS_EXTRA_HEADERS` becomes a small per-call header list (built once
      in `handle_invite`) carrying `Allow` plus `P-Access-Network-Info`
      sourced from the line's real access-network value; update the two
      existing tests that pass the old const directly (MT-11).
- [ ] T006 [P] [US1] Tests: `build_subscribe` states whatever
      `access_network_info` it's given; the `200 OK` to an inbound INVITE
      states the real value.
- [ ] T007 [US1] In `gsm-sip-bridge/src/ims/sip_client.rs`: add
      `annotate_via_received_rport(message: &str, peer: SocketAddr) -> String`
      — no-op unless `message` starts with `SIP/2.0 ` (a response, never a
      request); on the top `Via`, add `received=<peer.ip()>` when the
      sent-by host differs, fill a bare `rport` with `rport=<peer.port()>`
      (MT-13).
- [ ] T008 [US1] Add `SipSink::peer_addr(&self) -> Option<SocketAddr>`
      (`TcpStream::peer_addr()` for TCP, the already-stored peer for UDP);
      call `annotate_via_received_rport` inside `SipSink::send` before
      writing to the socket.
- [ ] T009 [US1] In `gsm-sip-bridge/src/sip/server/mod.rs`'s `serve()`: call
      `annotate_via_received_rport(&response, peer)` before its one
      `socket.send_to(...)`.
- [ ] T010 [P] [US1] Tests: `annotate_via_received_rport` — mismatched
      sent-by gains `received=`; bare `rport` gets filled; a request
      (start-line not `SIP/2.0 `) is left untouched; an already-matching
      Via with no `rport` param is left untouched.

**Checkpoint**: Caller identity, access-network value, and Via
annotations all reflect reality; existing response-builder tests
(constructing responses without a real socket) are unaffected.

---

## Phase 2: User Story 2 - SMS decoding handles specific wire shapes (SMS-02, SMS-03, SMS-04, Priority: P2)

- [ ] T011 [US2] In `gsm-sip-bridge/src/ims/sms_pdu.rs`: add
      `TpduMessageType` (`Deliver`/`SubmitReport`/`StatusReport`/`Reserved`,
      from the first octet's low 2 bits) and extend `DecodedRp` with
      `UnsupportedTpdu { rp_mr: u8, kind: TpduMessageType }` and
      `Undecodable { rp_mr: u8 }` (SMS-02/SMS-03).
- [ ] T012 [US2] `decode_vnd_3gpp_sms`: classify TP-MTI before calling
      `SmsDeliverTpdu::parse`; a non-Deliver type returns
      `Ok(DecodedRp::UnsupportedTpdu{..})`; a Deliver type that fails to
      parse returns `Ok(DecodedRp::Undecodable{..})` (never loses `rp_mr`
      the way a bare `Err` would).
- [ ] T013 [US2] Add `build_rp_error(rp_mr: u8, cause: Option<u8>) -> Vec<u8>`,
      mirroring `build_rp_ack`'s existing shape (TS 24.011 §7.3.4 RP-ERROR).
- [ ] T014 [US2] In `gsm-sip-bridge/src/ims/agent/mod.rs`'s `handle_message`:
      two new match arms alongside the existing `Ack`/`Error` ones —
      `UnsupportedTpdu` sends a plain `200 OK` (like `Ack`/`Error`);
      `Undecodable` sends the RP-ERROR delivery report (via the same
      `send_sms_delivery_report` mechanism `acknowledge` already uses for
      RP-ACK) instead of falling through to `body = req.body.clone()`.
- [ ] T015 [P] [US2] Tests: an SMS-STATUS-REPORT-shaped TPDU is recognized
      as `UnsupportedTpdu`, not misread as SMS-DELIVER; a truncated
      SMS-DELIVER TPDU is recognized as `Undecodable`; `build_rp_error`
      states the given cause; `handle_message` never relays either case
      as text.
- [ ] T016 [US2] In `gsm-sip-bridge/src/ims/sms_pdu.rs`'s `Alphabet::from_dcs`:
      add the `0xE0`-`0xEF` (Message Waiting Indication, Store, UCS2) case
      — bit 2 selects the alphabet, same convention as the existing
      `0xF0` branch (SMS-04).
- [ ] T017 [P] [US2] Test: a DCS in `0xE0`-`0xEF` with bit 2 set decodes as
      UCS2, not GSM7.
- [x] ~~T018/T019 (SMS-07)~~ — deferred mid-implementation, not done. The
      mechanism (recognizing the `0x24`/`0x25` UDH IEs) is small, but
      shipping without verified TS 23.038 Annex A table data risks
      decoding real text to the *wrong* characters — worse than today's
      honest, already-documented gap. See `research.md` Decision 8.

**Checkpoint**: Every TPDU shape this bridge might receive is recognized
correctly; a decode failure gets an RP-ERROR, never a raw-bytes relay.

---

## Phase 3: User Story 3 - Modem commands and SDP bodies are validated (CS-03, CS-04, SDP-05, Priority: P3)

- [ ] T020 [US3] In `gsm-sip-bridge/src/volte/sms.rs`'s
      `sweep_modem_storage`: send `AT+CNMI=2,1,0,0,0` alongside the
      existing `AT+CMGF=0` (CS-03, parity with
      `modules::worker::ModuleWorker::open`'s existing policy).
- [ ] T021 [P] [US3] Test/verification: the sweep's AT command sequence
      includes `AT+CNMI=2,1,0,0,0`.
- [ ] T022 [US3] In `gsm-sip-bridge/src/modules/worker.rs`'s
      `parse_sms_response`: replace the naive `line.split(',')` with a
      quote-aware splitter (respects `"..."` boundaries) before extracting
      the sender field (CS-04).
- [ ] T023 [P] [US3] Test: a `+CMGR` line whose quoted `<scts>` (or
      `<alpha>`) field contains a comma still attributes `sender` correctly.
- [ ] T024 [US3] In `gsm-sip-bridge/src/ims/agent/inbound.rs`'s
      `handle_invite`: check the INVITE's `Content-Type` before calling
      `sdp::parse_offer` — accept `application/sdp` or absent (today's
      implicit assumption), decline anything else with the same shape as
      `message_content_type_supported`/`415` (SDP-05).
- [ ] T025 [P] [US3] Tests: an INVITE with `Content-Type: application/sdp`
      or none is unaffected; one with an unrecognized `Content-Type` is
      declined before `parse_offer` runs.
- [ ] T026 [US3] In `gsm-sip-bridge/src/ims/agent/inbound.rs`'s test
      module: a confirming test that `100rel` is never advertised on any
      UAS response and `Require: 100rel` is still declined by the existing
      MT-03 gate (MT-04 — no production code change).

**Checkpoint**: All ten findings land, plus MT-04's confirming test.

---

## Phase 4: Polish & Cross-Cutting

- [ ] T027 Update `docs/plans/mt-conformance-findings.md`: mark
      MT-04/MT-11/MT-12/MT-13/SDP-05/SMS-02/SMS-03/SMS-04/CS-03/CS-04
      `[x]` with "Landed" writeups matching batches 1-5's style; record
      MT-06/SDP-04/SMS-05/SMS-07 as explicitly deferred (not `[x]`, not `[-]`),
      pointing at this feature's `research.md` Decision 1.
- [ ] T028 Add one entry to `RELEASE_NOTES.md` under `## Unreleased`.
- [ ] T029 `make format && make lint && make test` (whole workspace,
      clippy `-D warnings`).
- [ ] T030 Hardware round on the `test/` docker rig per `quickstart.md`:
      rebuild/retag, redeploy, re-register, drive a real call and SMS,
      confirm no regression; use `/discord-notify`.

## Dependencies & Execution Order

Phases 1-3 are independent of each other (different files, different
concerns) — implement in priority order (P1 → P2 → P3) for commit
sequencing, matching Constitution Principle III. Within Phase 1, T001-T002
(MT-12) and T003-T006 (MT-11) touch `session.rs`/`inbound.rs` but
different functions; T007-T010 (MT-13) is fully independent (`sip_client.rs`
+ `sip/server/mod.rs`). Within Phase 2, T011-T015 (SMS-02/03) must land
before T016-T017 (SMS-04) only in the sense that they touch the same
file — no functional dependency. Phase 4 depends on Phases 1-3.
