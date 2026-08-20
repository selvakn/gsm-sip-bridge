# Changelog

All notable changes to this project are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project does not yet commit to [Semantic Versioning](https://semver.org/)
across releases.

## [Unreleased]

### Fixed

- **A graceful container stop left every line's tunnel `if_id` claimed,
  costing the next restart ~2.5 minutes of silence per line.** Stop used to
  only signal every child process and remove each line's namespace *name* —
  never destroy the tunnel interface, which is the only thing that actually
  releases the XFRM `if_id` it claims. The next start would then find that
  id still held by a leftover device inside an unaddressable namespace, and
  wait on the kernel to reap it. Stop now waits for every child, terminates
  each line's IKE_SA, flushes this deployment's own XFRM state, and deletes
  each line's tunnel interface and veth pair before its namespace; a
  force-killed run's leftovers are reclaimed on the next start via a
  namespace directory now shared with the host. VoLTE lines' veth pairs,
  never deleted either, get the same cleanup.

- **The container's DNS resolver was replaced by the carrier's whenever an
  ePDG tunnel came up.** strongSwan's `resolve` plugin was writing the
  assigned IMS DNS servers into `/etc/resolv.conf`, *replacing* the resolvers
  Docker put there at container start rather than adding to them — leaving
  one carrier-controlled nameserver and no fallback. Whether that still
  resolves anything depends entirely on whether the host happens to have a
  route to it: observed live on a Jio line with no IPv6 default route, the
  assigned v6 resolver was unreachable outright and every outbound HTTPS call
  the daemon makes failed, Discord critical alerts and SMS forwarding
  included. Nothing reported it — the alert channel itself was the casualty —
  and it recurred on every IKE re-auth, so a container restart only bought
  until the next one. The plugin now writes to `/run/ims-resolv.conf`
  instead, leaving the system resolver alone; the config request sent to the
  ePDG is unchanged, so this cannot perturb a carrier that is fussy about it.

- **Jio VoWiFi inbound calls.** Our `200 OK` never declared its own
  capabilities (`Allow`/`Supported`), which Jio rejected ~460ms in with a
  boilerplate `cause=503 "SDP Protocol Error"` that had nothing to do with
  the SDP — five different answer bodies had failed identically before this
  was found. Also required: answering on the Gm *client* leg rather than
  the socket a request arrived on (Jio-specific, gated off by default), and
  a REGISTER `Contact` that echoes the tags the caller's own preferences
  require.
- **SMS over IMS.** Inbound `MESSAGE` never reached us at all on Jio until
  the REGISTER `Contact` declared `+g.3gpp.smsip` — a real device sharing
  the same line's registration carried the tag and received SMS; ours
  didn't and never saw one arrive.
- **A crash and message corruption on any binary SIP body.** Every SIP
  receive path converted raw socket bytes to text via a lossy UTF-8
  conversion *before* finding the message boundary or slicing
  `Content-Length` bytes of body. A carrier's SMS-over-IP body
  (`application/vnd.3gpp.sms`) is an arbitrary binary TPDU, not text; the
  lossy conversion could shift the buffer's length away from what actually
  arrived, corrupting whatever followed it on the same TCP connection and,
  in one observed case, panicking the connection's reader thread outright.
  Framing now happens on the original bytes throughout.
- IPv4 traffic on the Gm IPsec tunnel interface was silently dropped —
  `disable_policy` was only ever applied to IPv6.
- A registration's P-CSCF address could appear to "change" across a normal
  IKE rekey, tearing down an otherwise healthy line and reinitiating it
  unnecessarily.
- An abandoned outbound origination attempt was re-abandoned on every
  dispatch-loop tick instead of once, spinning at the loop's poll rate and
  occupying the only outbound slot.
- Compact SIP header forms (`v:`, `i:`, `f:`, `t:`, `l:`) were not
  recognised, which on a carrier using them exclusively broke both
  `Content-Length` framing (desyncing the reader on any response carrying a
  body) and transaction matching.
- Outbound calls: a `183 Session Progress` requiring `100rel` was never
  acknowledged (no `PRACK` support), so the network retransmitted the
  provisional until it gave up and abandoned the call; a response
  answering our own `PRACK` was mistaken for the INVITE's final response,
  causing an `ACK` for a call that was never actually answered; a carrier
  whose SDP answer arrives in the reliable provisional (leaving the `200
  OK` bodiless) was treated as having sent no answer at all; the
  originating identity used on the `INVITE` did not match the one used on
  its `ACK`/`CANCEL`/`BYE`; a `BYE` built without a `Contact` on the
  triggering response lost its `sip:` scheme.
- An XFRM policy leak: cleaning up a Gm IPsec session only removed the
  4 canonical policies for a single transport, while the current setup
  path installs 4 canonical + 4 cross-product policies for **both** TCP
  and UDP (16 total) — every re-registration or reinitiate leaked 12 of
  them permanently in the kernel's policy table.
- A malformed or adversarial `Security-Server` offer naming `q=nan` could
  win selection unconditionally and then never be displaced by a real
  offer (`x > NaN` is always `false`).
- A reordered retransmission of an older reliable provisional response
  (arriving after a newer one had already been acknowledged) could be
  `PRACK`ed a second time.

### Added

- A from-scratch decoder for 3GPP SMS-over-IP (TS 24.011 RP-DATA wrapping
  a TS 23.040 SMS-DELIVER TPDU): GSM 7-bit default alphabet and UCS-2 text,
  with or without a concatenation UDH. The real originating number now
  comes from the TPDU's `TP-OA`, not the SIP `From` — which on a real
  network names an IMS core relay element, not the sender.
- A deployment can now choose the REGISTER request-URI form
  (`register_request_uri`): the literal P-CSCF address, or the
  TS 24.229-mandated home-domain realm. Carriers disagree about which one
  they'll accept.

### Security

- Full, unredacted SIP datagrams (every header, plus the body — including,
  since the SMS decoder above, SMS text) were logged at `debug` level, a
  level this project's own deployments run with routinely in the field.
  Downgraded to `trace`, which preserves the same diagnostic capability for
  a deliberate debugging session without it landing in logs left running
  day to day.
