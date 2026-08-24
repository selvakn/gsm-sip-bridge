# Follow-up: the untried signaling gaps, tried

**Raised**: 2026-08-24 · **Tested on the live Jio line**: 2026-08-24 ·
**Parent**: [docs/plans/jio-vowifi-outbound-480.md](jio-vowifi-outbound-480.md)

**Verdict: all five gaps are ruled out.** Four were built and sent to Jio on
one call; the fifth was disproved from the same capture without building it.
The 183/480 intercept is unchanged, to the second.

## What was proposed

The parent doc ruled out three theories by examining what the INVITE *sends*.
This doc's premise was that nobody had examined what it **doesn't** send: the
originating header set is pinned by test
(`the_originating_header_set_is_pinned`, `ims/call.rs`) to omit `Supported:`,
`P-Preferred-Identity:` and `Accept-Contact:`, because every other carrier in
production accepts the minimal set.

The premise was factually right — those headers really were absent. The
inference from it was wrong.

## What was built

`[vowifi] originating_headers`, a list of opt-in header groups (see
`docs/configuration.md`). All four were turned on at once — they are additive
and the intercept decision is made before any of them could interact, so a
subset cannot succeed where the union fails:

| Token | Adds |
|---|---|
| `icsi` | `Accept-Contact: *;+g.3gpp.icsi-ref="…mmtel";explicit;require`, `P-Preferred-Service`, and the `+g.3gpp.icsi-ref` feature tag on `Contact` |
| `preferred-identity` | `P-Preferred-Identity`, same identity as `From` |
| `supported` | `Supported: 100rel, timer` |
| `allow` | the full MTSI method list |

That covers gaps 2, 3 and 4, and goes past gap 4 by adding
`P-Preferred-Service` and the Contact feature tag, which the doc didn't
mention and which are the parts an S-CSCF's initial filter criteria are most
likely to key on.

## Result: no change whatsoever

Sent on tag `jio-mtsi-hdrs`, 2026-08-24 11:18 UTC, to `+919789063708`:

```
Contact: <sip:+91…@10.252.222.88:39148;transport=TCP>;+g.3gpp.icsi-ref="urn:urn-7:3gpp-service.ims.icsi.mmtel"
P-Preferred-Identity: <sip:+91…@ims.mnc869.mcc405.3gppnetwork.org>
Accept-Contact: *;+g.3gpp.icsi-ref="urn:urn-7:3gpp-service.ims.icsi.mmtel";explicit;require
P-Preferred-Service: urn:urn-7:3gpp-service.ims.icsi.mmtel
Supported: 100rel, timer
Allow: INVITE, ACK, BYE, CANCEL, OPTIONS, PRACK, UPDATE, INFO, MESSAGE, NOTIFY, REFER
```

| | minimal INVITE | full MTSI INVITE |
|---|---|---|
| 183 arrives | +202 ms | +226 ms |
| 183 `Reason` | `Q.850;cause=41` | `Q.850;cause=41` |
| 183 `Contact` | `<sip:msml@…>` | `<sip:msml@…>` |
| early media | AMR-WB, real speech | AMR-WB, real speech |
| final | `480`, `cause=31` | `480`, `cause=31` |
| 183 → 480 | 13.37 s | 13.60 s |

The **only** difference Jio made was answering `Require: timer,100rel` +
`RSeq: 1` instead of `Require: timer` — i.e. it took up the `100rel` we
advertised and demanded a PRACK for its 183. We PRACKed it correctly and it
answered `200 OK`. Nothing about the intercept moved.

That reproduces the trap recorded in
[[jio-uas-responses-never-reach-carrier]] — *don't advertise extensions you
don't implement*. `100rel` is implemented now, but `timer` (RFC 4028 §7.4
session refresh) is **not**, so `originating_headers = ["supported"]` is a
loaded gun on any carrier that decides to `Require: timer` on a call that
actually connects. It is off by default and should stay off.

## Gap 1 (`Security-Verify` on the INVITE) — disproved without building it

This was ranked "most suspicious", on the reasoning that TS 33.203 requires
`Security-Verify` on all requests over the SA, that we only ever send it on
the protected REGISTER, and that inbound works because *responses* don't
carry it while outbound fails because *requests* do.

The same capture kills it. In the failing call itself, our **PRACK** — a
request, carrying no `Security-Verify` — was routed by the P-CSCF, through
the S-CSCF via the originating `Route`, past the ISC application server, to
Jio's media server, which answered `200 OK` 77 ms later:

```
11:18:42.204  sending SIP request PRACK sip:+91…@ims.mnc869.mcc405.3gppnetwork.org;user=phone
              (no Security-Verify — grep the log: exactly one, on the REGISTER)
11:18:42.281  received SIP response SIP/2.0 200 OK … CSeq:6 PRACK
```

Our `ACK` closes the dialog the same way, and our 2-minutely `OPTIONS`
keepalives are answered `200 OK` throughout. Jio's core accepts and routes
our requests without `Security-Verify`. A P-CSCF enforcing sec-agree would
also answer `494 Security Agreement Required` (RFC 3329 §2.5), not admit the
request and hand it to a media server.

The asymmetry the theory rests on doesn't exist either: inbound and SMS both
work over this registration, and SMS-over-IMS is our own `MESSAGE` **request**
to the IP-SM-GW — required by Jio, and working (see
[[rp-ack-ruled-out-on-a-bad-test]]).

## Gap 5 (minimal SDP) — disproved by the answer

Jio *accepted* the offer: it answered with a full media-server SDP, selected
our AMR-WB payload type, and streamed 13.5 s of real audio to the port we
advertised. An SBC that had flagged the offer as too minimal would not have
negotiated it and then talked to us.

## What this leaves

The decision is taken at the terminating application server on the ISC
interface (`Record-Route: <sip:tn3scfx…;interface=isc>`), ~220 ms after the
INVITE, before any destination is alerted, identically for every destination
and every header set. That is a subscriber service decision, not a protocol
one — see the parent doc's next steps.

## The code

`[vowifi] originating_headers` is kept, defaulted to `[]`, which produces a
byte-identical INVITE to the one every carrier has always received (pinned by
`the_originating_header_set_is_pinned`). It is a probe for the *next* carrier
that intercepts a minimal INVITE, not a fix for this one. Turning one on is a
config edit and a restart — no rebuild.
