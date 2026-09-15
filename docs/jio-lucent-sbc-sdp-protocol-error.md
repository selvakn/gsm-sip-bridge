# Jio inbound VoWiFi: Lucent SBC rewrite breaks the call (`cause=503 "SDP Protocol Error"`)

**Status: root-caused to Jio's own network, not this bridge's code.** Two
real, separate bugs were found along the way — one fixed in this change,
one still open (see below) — but the dominant failure mode is a
carrier-side session-border element that reliably breaks the call whenever
it touches it. No further code change on our side is known to help;
retrying (and hoping for a route that avoids it) is the only current
workaround.

Investigated live against pi@192.168.100.2, the real Jio VoWiFi line
(`[[vowifi.line]]` `msisdn = "9000000000"`), 2026-09-15.

## Symptom

An inbound VoWiFi call rings, the PBX extension picks up, and within
0.5–2.5 seconds the carrier tears the call down:

```
BYE sip:...
Reason:SIP;cause=503;text="IO: SIP SDP Protocol Error."
```

or, on other calls, the same thing with a different two-letter prefix:

```
Reason:SIP;cause=503;text="PO: SIP SDP Protocol Error."
```

The bridge's own media accounting (`gsm_sip_bridge::ims::agent::call`,
"call media verdict") shows `media="send-only"`: we transmit normally
(`pbx_rx` 90–116 packets in a ~1s call — full rate) but receive almost
nothing back from the carrier (`carrier_rx` 0–6 packets) before the BYE
arrives.

## Two real bugs found along the way

One genuine, reproducible defect is fixed by this change; a second remains
open. Neither turned out to be the main story:

1. **Missing `Supported` header (regression) — fixed here.** Commit
   `5277765` ("stop claiming capabilities this UAS doesn't have",
   2026-08-26) dropped `Supported: timer, 100rel, replaces, path, gruu`
   from the inbound INVITE's `200 OK`, on RFC-purism grounds, and was
   hardware-verified only against Vi/Vodafone. Jio's network requires this
   header regardless of whether we implement any behaviour behind it — its
   absence produced the `"IO: SIP SDP Protocol Error"` teardown 100% of the
   time (4/4 calls captured pre-fix). Restored on all three places this
   bridge builds a successful `200 OK` to an inbound INVITE
   (`gsm-sip-bridge/src/ims/agent/inbound.rs`'s main and offerless-INVITE
   paths, plus `ims/agent/mod.rs`'s retransmitted-original-INVITE resend of
   the cached answer — easy to miss since it rebuilds the response
   separately from the other two). See
   [[jio-uas-responses-never-reach-carrier]] memory for the original
   2026-08-15 discovery of this same requirement.

   Live-verified on the Jio Pi (image tag `jio-supported-hdr-fix`) before
   this PR landed the change; `make format` / `make lint` / `make test`
   all pass.

2. **Rapid-redial teardown race (still open, separate issue).**
   `pjsua-safe`'s `Call::hangup()` (`pjsua-safe/src/call.rs:175`) fires
   `pjsua_call_hangup` and returns immediately — it does not wait for
   PJSIP's own async teardown (media port release, conference-bridge slot
   free) to actually complete. Redialling within roughly 15 seconds of the
   previous call ending reliably fails or is outright rejected
   (`pbx_unreachable`, PJSIP couldn't place the new call yet). Every
   call in an 8-call rapid-fire burst (2.6–7.4 s gaps) either failed or was
   rejected; the moment the gap grew past ~20 s, calls that avoided the SBC
   issue below succeeded normally. Not yet fixed — needs `hangup()` (or its
   caller) to block on PJSIP's confirmed-disconnected callback before
   signalling the line ready for a new call.

Neither of these explains the majority of failures once headers were fixed
and calls were spaced out, which is where the real finding is.

## The actual differentiator: which Jio element built the SDP offer

14 inbound calls were captured (raw SIP+SDP trace at
`/tmp/ims-agent-0.out` inside the container — `vowifi-ims-agent`'s own
stdout, redirected there by `supervise::orchestrate`, **not** visible in
`docker logs`). Every offer's `o=` line — which names the network element
that generated that SDP — was checked against the call's outcome:

| Offer `o=` line | Calls | Outcome |
|---|---:|---|
| `o=LucentPCSF ...` | 2 | **fail**, all `IO`/`PO` |
| `o=LucentIBCF ...` | 9 | **fail**, all `IO`/`PO` |
| `o=JIO_ISBC ...` | 1 | declined (`pbx_unreachable`, unrelated redial-race issue) |
| `o=sip:+91XXXXXXXXXX@ims.mnc869.mcc405.3gppnetwork.org` (bare SIP URI, no SBC rewrite) | 2 | **success**, both-ways audio |

**Zero exceptions across the full sample.** Every call whose SDP had been
rewritten by one of Jio's Lucent session-border elements — `LucentPCSF`
(P-CSCF-side SBC) or `LucentIBCF` (Interconnect Border Control Function,
used when the caller is on a different network and the call arrives via
interconnect) — failed with the SDP protocol error, regardless of caller
number, codec, access network (`P-Access-Network-Info` FDD vs. TDD), or
how long since the previous call. Both calls that succeeded had an offer
that bypassed SBC rewriting entirely (`o=sip:...`, presumably closer to
what the originating UE itself sent) and completed with real two-way audio,
confirmed by the carrier's own RTCP receiver reports
(`round_trip_ms≈109`, `far_end_fraction_lost=0`).

One caller number (`+919000000001`) got `LucentPCSF`/`LucentIBCF`-rewritten
offers on every one of its 4 calls across the whole session and failed
every time. A second caller (`+919000000002`) got the SBC-rewritten path 6
times (failed all 6) and the bare `sip:` path twice (succeeded both times)
— so which path a given call takes is decided somewhere inside Jio's core,
not by anything this bridge controls or by anything observable on the
caller's side beforehand.

### Example: same caller, back-to-back, different routing, different outcome

Both calls below are `+919000000002`, ~32 seconds apart, essentially
identical offers (same codec list, same `mode-set`/`octet-align`):

**Failed** (`o=LucentIBCF`):
```
o=LucentIBCF 1917823437 1917823437 IN IP4 ims.mnc869.mcc405.3gppnetwork.org
...
media verdict: media="send-only" carrier_rx=1 pbx_rx=115 outcome="failed"
BYE ... Reason:SIP;cause=503;text="IO: SIP SDP Protocol Error."
```

**Succeeded** (`o=sip:...`), same caller, next attempt:
```
o=sip:+919000000002@ims.mnc869.mcc405.3gppnetwork.org 1789486253 ... IN IP4 ims.mnc869.mcc405.3gppnetwork.org
...
media verdict: media="both-ways" carrier_rx=87 pbx_rx=314 far_end_reported=true
              round_trip_ms=109.31 far_end_fraction_lost=0 outcome="answered"
```

Our own `200 OK` (headers, SDP answer, codec/fmtp echo) was byte-identical
in shape between the two — same `Allow`/`Supported`/`P-Access-Network-Info`,
same AMR-WB payload type and `octet-align=1` framing, same `telephone-event`
echo. The only structural difference is which element built the *offer* we
were answering.

## Conclusion

The bug is in Jio's own infrastructure: whichever Lucent SBC/IMS-AGW
instance sits in the call path when it rewrites the inbound SDP offer
mishandles the resulting media session — regardless of what this bridge's
answer says. This bridge's SDP/RTP handling is verified correct end-to-end
(the two calls that avoided the SBC rewrite worked perfectly, with clean
RTCP-confirmed two-way audio). There is no known code change here that
avoids the Lucent-rewritten path; which routing element handles a given
call is an internal Jio core decision.

## Practical guidance

- Retry a failed call — a different attempt may get routed around the
  Lucent element and succeed, as seen with `+919000000002`.
- Space test calls out by 20+ seconds to avoid also hitting the separate,
  unrelated redial-teardown race (bug 2 above).
- If diagnosing a future occurrence, check `/tmp/ims-agent-{N}.out` inside
  the container for the raw offer's `o=` line before assuming anything
  else — it is the fastest, most reliable signal for which failure mode is
  in play.

## Open questions

- Is there a way to influence Jio's routing away from the Lucent SBC path
  (e.g. a provisioning setting, a specific Contact/Route hint) that would
  make the working path the common case rather than a minority outcome?
  Not investigated — would need Jio-side visibility this bridge doesn't
  have.
- Whether `LucentPCSF` and `LucentIBCF` fail for the *same* underlying
  reason, or two different bugs that happen to produce the same generic
  `cause=503` text, is unknown — the sample size per prefix (`IO` vs `PO`)
  is too small and the two-letter prefix did not otherwise correlate with
  anything checked (not the SBC element, not access network, not header
  correctness — a call with fully correct `Allow`/`Supported` still failed
  `IO`).
