# Quickstart: verifying batch 6 (the long tail)

## Unit tests

All new logic is pure/small and exercised without a modem or real
carrier, matching each file's existing test style:

- `session.rs`: `extract_caller` prefers `P-Asserted-Identity` when
  present, falls back to `From` when absent.
- `session.rs`: `build_subscribe` states whatever `access_network_info`
  it's given, not a hardcoded value.
- `sip_client.rs`: `annotate_via_received_rport` — a mismatched sent-by
  gains `received=`; a bare `rport` gets filled; a request (not a
  response) is left untouched; an already-matching Via is untouched.
- `sms_pdu.rs`: an SMS-STATUS-REPORT TPDU is recognized as
  `UnsupportedTpdu`, not misread as SMS-DELIVER; a truncated/malformed
  SMS-DELIVER TPDU is recognized as `Undecodable`; `build_rp_error`
  states the right cause; a `0xE8`-style DCS decodes as UCS2; a
  national-language escape decodes correctly.
- `agent/mod.rs`: `handle_message`'s new match arms send a plain `200 OK`
  for `UnsupportedTpdu` and an RP-ERROR delivery report for
  `Undecodable`, in both cases never falling through to relay `req.body`
  as text.
- `agent/inbound.rs`: the `200 OK` to an inbound INVITE states the real
  access-network value; an INVITE whose `Content-Type` isn't SDP is
  declined before `parse_offer` ever runs; a confirming test that
  `100rel` is never advertised and `Require: 100rel` is still declined
  (MT-04).
- `volte/sms.rs`: the modem-storage sweep's AT command sequence includes
  `AT+CNMI=2,1,0,0,0`.
- `modules/worker.rs`: `parse_sms_response` attributes fields correctly
  when a quoted field contains a comma.

## Hardware round

Same rig and pattern as batches 1-5 (`test/`, on-host EC20 line): rebuild
and retag the image, redeploy, re-register the real line, drive a real
inbound call and an SMS, confirm no regression on the ordinary path.

Most of this batch's new behaviors (a non-default DCS group, a
national-language table, a malformed TPDU, a mismatched `Via` sent-by, a
non-SDP INVITE body) are not things this project's carriers have been
observed producing — same posture already accepted for this review's
other least-observed findings. MT-11's access-network fix and MT-12's
`P-Asserted-Identity` preference are the two most likely to be visible on
an ordinary real call/registration, so specifically check the SUBSCRIBE
and the `200 OK`'s headers during the hardware round.
