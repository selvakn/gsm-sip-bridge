# Research: SMS Delivery Report Conformance

## R1 - What the specs require

TS 24.341 v19.0.0 (text extracted from the ETSI PDF, 2026-10-20):

- §5.3.2.4 (a)-(f): Request-URI = IP-SM-GW (from the inbound `P-Asserted-Identity`); `From` = our public identity; `To` = IP-SM-GW; **(d) `In-Reply-To` = the Call-ID of the received `MESSAGE`**; `Content-Type: application/vnd.3gpp.sms`; body = RP-ACK or RP-ERROR. Note 2: binary content transfer encoding.
- Annex B.6 table B.6-7 (UE to P-CSCF): `Route: <P-CSCF;lr>, <orig@scscf;lr>` (P-CSCF first, then the S-CSCF entry the registrar returned), `P-Preferred-Identity`, `From`, `To`, `Call-ID`, `In-Reply-to`, `CSeq`, `Content-Type`.
- TS 24.229 §5.1.2A.1.1 (not re-fetched; recalled, to be confirmed while implementing): the UE builds the Route set as the P-CSCF URI followed by the saved Service-Route. RFC 3608 covers the Service-Route half; RFC 3261 §8.1.2 / §16.12 covers sending to the configured outbound proxy when there is no route set. RFC 3261 §20.21 defines `In-Reply-To`.
- RFC 3327 `Path` is the registrar's record of the edge proxy. A UE does not copy it into its own route set.

**Decision**: implement `In-Reply-To` and the P-CSCF-first route set. **Alternative rejected**: reuse the REGISTER 200's `Path` as the first hop; it is not a UE route and only coincides with the P-CSCF on some carriers.

## R2 - Where the P-CSCF address comes from

`RegisteredSession` already holds `pcscf_addr: SocketAddr` (the address registered through) and, when a Gm SA was negotiated, `gm_state.theirs.port_s`, the P-CSCF's protected server port. In the #140 capture, the port the P-CSCF used on the Via of its own `MESSAGE` and in `Security-Verify` was the protected server port (9900), not the 5060 used for the first REGISTER.

**Decision**: route to `<sip:{pcscf-ip}:{port_s};lr>` when a Gm SA exists, otherwise to `pcscf_addr` as is. Bracket IPv6 with the existing `format_sip_addr`. **Alternative rejected**: always use `pcscf_addr` (port 5060): traffic would bypass the SA.

## R3 - Inbound Call-ID availability

`handle_message` and `acknowledge` hold the inbound `SipRequest`, so `req.header("Call-ID")` is in reach at all three `send_sms_delivery_report` call sites (the RP-ACK for a decoded message, the RP-ACK for a non-deliver TPDU, the RP-ERROR for an undecodable one). No new plumbing beyond passing it as an argument. The spec's edge case "no usable Call-ID" is then a defensive `Option`: send without `In-Reply-To` and warn.

## R4 - The setting

`sms_delivery_report` is the established pattern: a `[vowifi]` boolean plus a `[volte]` copy that inherits when unset (config/raw.rs, config/mod.rs, docs/configuration.md, `volte/carrier_agent.rs`). Because the report's sender is shared, the new setting follows that pattern exactly.

**Decision**: `sms_report_pcscf_route`, default `true`, `[vowifi]` plus an inheriting `[volte]` copy. Off restores today's route set (Service-Route entries only). `In-Reply-To` has no setting (clarification).

## R5 - Seeing the carrier's answer

Responses to our requests reach `handle_response` in `ims/agent/mod.rs`. A reply to a delivery report matches nothing there and falls to the generic `received response outside an active transaction` line. `Via`/`Call-ID`/`CSeq` on the reply match the report we sent (confirmed in the #140 capture: Call-ID `31c04dde5f3ebad4`, `CSeq: 4 MESSAGE`).

**Decision**: `send_sms_delivery_report` returns the Call-ID it used when the write succeeded; the dispatcher keeps a bounded `VecDeque` of in-flight report Call-IDs (cap 32, oldest evicted). `handle_response` checks it before the generic line: 1xx is left pending, `2xx` removes the entry and logs at debug, `>=300` removes it and logs one `warn!` with the status, reason, the report's Call-ID and any `Warning` header text. **Alternative rejected**: per-report timeout bookkeeping; out of scope (no retries) and the 12 s T2 case already ends in a `500` we can log.

## R6 - Live verification

Memory notes: Jio's REGISTER/INVITE/SUBSCRIBE were each broken by a single header earlier, and Jio requires the report to deliver SMS at all. Vodafone/Vi delivers real SMS without any report, so a missing report is invisible there, but the `2xx` on a sent one is still observable in the log. MEO and T2 cannot be exercised here.

**Decision**: gate merge on live Jio (remote Pi) and Vodafone (local rig) checks (SC-004), with the setting as the immediate rollback and FR-010 (restrict to empty Service-Route) as the code fallback.
