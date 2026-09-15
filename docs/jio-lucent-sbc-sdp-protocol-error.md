# Jio inbound VoWiFi: `cause=503 "SDP Protocol Error"` is an `Allow` header problem

**Status: root-caused and fixed.** Jio's Alcatel-Lucent border elements
reject a `200 OK` whose `Allow` header does not claim **`UPDATE`**, and
report it as `Reason: SIP;cause=503;text="IO: SIP SDP Protocol Error."` —
a boilerplate text that, for the third time on this carrier, has nothing
to do with the SDP body. Adding that one method makes the calls connect;
it was bisected down to the single token, and the carrier never sends an
`UPDATE` in the first place.

An earlier revision of this document concluded the opposite — that the
failure was a carrier-side media bug nothing here could avoid. That was
wrong, and how it went wrong is recorded under "How the first
investigation missed it" below, because the mistake is repeatable.

Investigated live against pi@192.168.100.2, the real Jio VoWiFi line
(`[[vowifi.line]]` `msisdn = "9000000000"`), 2026-09-15.

## Symptom

An inbound VoWiFi call rings, the PBX extension picks up, and within
0.5–2.5 seconds the carrier tears the call down:

```
BYE sip:...
Reason:SIP;cause=503;text="IO: SIP SDP Protocol Error."
```

or, on other calls, the same thing with a different two-letter prefix
(`"PO: SIP SDP Protocol Error."`). Neither prefix correlated with
anything measured.

## The measurement

14 inbound calls were captured (raw SIP+SDP trace at
`/tmp/ims-agent-0.out` inside the container — `vowifi-ims-agent`'s own
stdout, redirected there by `supervise::orchestrate`, **not** visible in
`docker logs`). Every offer's `o=` line — which names the network element
that generated that SDP — was checked against the call's outcome:

| Offer `o=` line | Calls | Outcome |
|---|---:|---|
| `o=LucentPCSF ...` | 2 | **fail**, all `IO`/`PO` |
| `o=LucentIBCF ...` | 10 | 9 **fail** (`IO`/`PO`); 1 declined by us (redial race) |
| `o=JIO_ISBC ...` | 1 | declined by us (redial race, below) |
| **total** | 15 | 11 carrier teardowns, 2 declined by us, 2 answered |
| `o=sip:+91XXXXXXXXXX@ims.mnc869.mcc405.3gppnetwork.org` (bare SIP URI, no SBC rewrite) | 2 | **success**, both-ways audio |

Zero exceptions: every call whose SDP had been rewritten by one of Jio's
Lucent session-border elements — `LucentPCSF` (P-CSCF-side) or
`LucentIBCF` (Interconnect Border Control Function) — failed, regardless
of caller number, codec, access network, or spacing. The same signal is
visible one step earlier, in the INVITE's own `Contact`: the rewritten
calls carry `;x-fbi=stdn-0` and the working ones do not.

**Our answer was byte-identical between a failing and a succeeding
call** — same headers, same SDP, same AMR-WB payload type and framing,
differing only in ports and session-id. So the rewrite itself is not what
breaks the call; it decides *which element inspects our response*.

### The timing says signaling, not media

In every failing call where the `ACK` was logged, the `BYE` lands within
2–45 ms of it:

```
o=LucentIBCF   200→ACK +0.479   200→BYE +0.481
o=LucentIBCF   200→ACK +0.513   200→BYE +0.515
o=LucentIBCF   200→ACK +0.523   200→BYE +0.523
o=sip:…      200→ACK +0.750   200→BYE +5.241   (caller hung up)
```

That is the carrier processing our `200 OK` and refusing it — the same
signature `ims::sdp::SdpOffer::dtmf` records for the 2026-08-14
`telephone-event` bug ("the ACK arrived, and 2ms later the carrier sent
`BYE`"). It is not a media timeout: there is no time for one.

## The cause

`ims::UAS_INVITE_ALLOW` — the `Allow` on a response to an inbound INVITE —
now claims `UPDATE`, which this UAS does not implement:

```
Allow: INVITE, ACK, CANCEL, BYE, OPTIONS, UPDATE, MESSAGE, NOTIFY
```

`ims::UAS_ALLOW`, the honest list of what the dispatch loop actually
serves, is unchanged and still what a `405` or an `OPTIONS` answers with.
The claim is scoped to the one response a carrier validates.

Bisected live, one variable per step, every call routed through a Lucent
element:

| `Allow` sent | Calls | Outcome |
|---|---:|---|
| `UAS_ALLOW` (the honest seven) | 12 | 0 answered — all `503 "SDP Protocol Error"` |
| + `PRACK, UPDATE, INFO, REFER` | 4 | **4 answered** |
| + `PRACK, UPDATE` | 3 | **3 answered** |
| + `UPDATE` | 3 | **3 answered** |
| + `PRACK` (same length, one token different) | 1 | 0 answered — failed identically |
| `UAS_INVITE_ALLOW`, as shipped, no config involved | 2 | **2 answered** |

So it is `UPDATE` specifically: not the list's length, not MMTel methods
in general. The control step is what makes that a conclusion rather than
a guess.

Twelve Lucent-routed calls were answered across the four passing
configurations, and none was torn down. Every one carried real two-way
audio (`carrier_rx` 28–603,
`far_end_reported=true` from the carrier's own RTCP receiver reports) and
ended when the caller hung up (`cause=200 "User Triggered"`). One call in
the first batch was declined by us — the redial race below, not the
carrier.

Three things the same captures rule out:

- **`P-Access-Network-Info` is not involved.** The winning rounds ran
  with the header at `3GPP-WLAN`. (The originating path's `IEEE-802.11`
  fix — `docs/plans/jio-vowifi-outbound-480.md` — is real, and
  unrelated.)
- **Jio never sends `UPDATE`.** The whole capture contains only
  INVITE/ACK/BYE. The border element validates the advertised set and
  never exercises it. One arriving anyway still draws the honest `405`.
- **`PRACK`, `INFO` and `REFER` are not wanted.** Dropping all three kept
  10/10 calls working.

This is the third instance of the same class on this carrier, all three
reported as `cause=503 "SDP Protocol Error"`:

1. 2026-08-14 — answer omitted the offer's `telephone-event` payload
   types (a real SDP fault).
2. 2026-08-26 to 2026-09-15 — `5277765` dropped `Supported` from the
   `200 OK` on RFC-purism grounds, verified only against Vi/Vodafone;
   restored in `a38f725`.
3. This one — `Allow` claiming only what we implement.

The lesson those three share: **on Jio, a capability header on the `2xx`
is validated, and the `503 "SDP Protocol Error"` text is boilerplate.**
Do not read the text as evidence about the SDP body, and do not narrow
one of these claims to match what the code actually serves without a live
Jio call to back it.

## How the first investigation missed it

Worth recording, since every step looked reasonable:

- **It read `media="send-only"` as a finding.** That verdict is computed
  over a window the `BYE` itself ended — 0.4–0.6 s. `carrier_rx=0..10`
  there means "no time to send", not "carrier is not sending".
  `pbx_rx` counts from a different, earlier start (the Agent B leg is up
  from ring time), so the two numbers are not comparable and the
  "we transmit at full rate, they send nothing" framing was an artifact.
- **It took `cause=503 "SDP Protocol Error"` at face value**, despite
  this repo's own release notes recording that the same text had already
  been proven to be boilerplate unrelated to the SDP body.
- **It stopped at a true correlation.** "Only SBC-rewritten calls fail"
  is correct and was established rigorously — but it describes *which
  element inspects our response*, not what the element objects to. The
  conclusion "nothing we send changes this" did not follow, and was not
  tested: the answer was compared only against our own other calls,
  never against what the carrier's own handsets advertise.

## Separate, still open: rapid-redial teardown race

`pjsua-safe`'s `Call::hangup()` (`pjsua-safe/src/call.rs:175`) fires
`pjsua_call_hangup` and returns immediately — it does not wait for
PJSIP's own async teardown (media port release, conference-bridge slot
free) to complete. Redialling within roughly 15 seconds of the previous
call ending fails or is rejected (`pbx_unreachable`). It accounted for
one declined call in each capture above, and with the `Allow` fix in
place it is now the dominant remaining inbound failure mode. Needs
`hangup()` (or its caller) to block on PJSIP's confirmed-disconnected
callback before signalling the line ready.

## Practical guidance

- Space test calls out by 20+ seconds, or the redial race above will
  produce failures that look carrier-caused and are not.
- When diagnosing a future occurrence, check `/tmp/ims-agent-{N}.out`
  inside the container. The two signals worth reading first are the raw
  offer's `o=` line (which element is in the path) and the gap between
  our `200 OK`, the `ACK`, and any `BYE` — a teardown inside ~50 ms of
  the `ACK` means the response was refused, and no amount of media
  analysis will explain it.
