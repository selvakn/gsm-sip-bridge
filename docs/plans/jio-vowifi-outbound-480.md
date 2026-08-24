# Investigation: Jio VoWiFi outbound (MO) calls always end in 480

**Triaged**: 2026-08-24 · **Effort**: unknown, likely carrier-side ·
**Status**: open. **Not** blocked on Jio: the same SIM originates fine over
VoLTE, so MO voice is provisioned and the "carrier limitation" reading is
dead. Next step is running our own stack over LTE to separate an
access-specific entitlement from a fault of ours.

## Symptom

Every outbound call placed on the Jio VoWiFi line (`pi@192.168.100.2`,
`ec20-11`) reaches the carrier, rings briefly, and is torn down with
`480 Temporarily Unavailable` after ~13–14 seconds of real early-media audio
from Jio's own media server — regardless of destination.

Three calls tested, two different destinations, one identical signature:

| call_id | destination     | INVITE → 183 | 183 → 480 | final |
|---------|------------------|-------------:|----------:|-------|
| out-0   | +919789063708    | 298 ms       | 13.34 s   | 480   |
| out-2   | +919789063708    | 202 ms       | 13.38 s   | 480   |
| out-0   | +918807793613    | 87 ms        | 13.38 s   | 480   |

(Destinations are the user's own phone and a second, unrelated number, on
what's presumed to be a different terminating network — kept out of git
history per this repo's no-real-numbers rule; both are placeholders here for
"two unrelated MSISDNs, different networks, same result.")

## What the wire trace actually shows

Captured from Agent A's own log (`/tmp/ims-agent-0.out` inside the bridge
container — stdout only carries Agent B/pjsua_safe events, not the Gm side).

The `183 Session Progress` for every attempt:

```
Server:DC-SIP/2.0
k:100rel,timer
Require:timer
Accept:application/sdp,text/*,application/msml+xml,application/moml+xml
Reason:Q.850;cause=41;text="temporary failure"
m:<sip:msml@56.10.108.1x:5060>
c:application/sdp

v=0
o=- ... IN IP4 ims.mnc869.mcc405.3gppnetwork.org
s=media server session
...
```

- The call is routed to Jio's own **MSML media server** (`msml@...`,
  `s=media server session`), not toward the called party.
- `Reason: cause=41 "temporary failure"` is stamped on the *183*, before our
  client has done anything but receive it.
- Real RTP flows for ~13.4s (early-media relay starts, packets move — this is
  presumably an IVR announcement, not silence).
- Final response: `480 Temporarily Unavailable`, `Reason: cause=31 "normal
  unspecified"`.

This exact shape — "~13.7s of `P-Early-Media` audio then a `480`" — was
already known and named "Jio's diagnosed behavior" as far back as
`specs/037-p-early-media` (2026-08-16, see `docs/todo.md` and
`gsm-sip-bridge/src/ims/agent/origination.rs` comments near `early_relay`).
That work fixed a *client-side* bug (we weren't relaying the early-media
audio to the caller, so it sounded like dead air) but did not diagnose *why*
Jio always intercepts the call in the first place — this doc picks that back
up.

## Theories raised and ruled out

Two external-review theories were checked against the live capture and both
failed to match the evidence:

1. **"Missing PRACK for the reliable 183"** — false. PRACK is implemented
   (`gsm-sip-bridge/src/ims/agent/origination.rs:574`, `prack_if_required`),
   correctly gated on `Require: 100rel`. Jio's 183 sends `Supported:
   100rel,timer` but `Require: timer` only — 100rel is advertised, not
   required, so staying silent is spec-correct (RFC 3262).
2. **"Missing UPDATE with `a=curr:qos ... sendrecv"`** — false. Jio's SDP
   answer carries zero `a=des:qos`/`a=curr:qos`/`a=conf:qos` lines in any of
   the three captures (grepped the full capture for `qos`, `curr:`, `des:`,
   `conf:` — nothing). There is no precondition negotiation happening for an
   UPDATE to complete.
3. **"Missing/malformed `P-Access-Network-Info` (DoT WiFi-calling
   geofencing)"** — false, on two grounds:
   - The header sent (`P-Access-Network-Info: 3GPP-WLAN`) is identical to
     what every successful REGISTER on this line sends, and REGISTER always
     succeeds. TS 24.229 §7.2A.4's grammar for the `3GPP-WLAN` access-type
     doesn't define a MAC/`wlan-node-id` sub-field to begin with.
   - A location/compliance rejection at the access layer reads as a hard
     `403`/`606` *before* the media plane is touched. What's observed is the
     opposite: Jio accepts the INVITE, routes through three `Record-Route`
     hops into its core, connects a real MSML media server, and streams real
     audio — a call admitted into the network, not one blocked at the door.
   - Ruled out further by the destination test in this doc: the same
     intercept happened for two unrelated destination numbers. A per-call,
     per-destination compliance check wouldn't produce an identical canned
     response regardless of who's being called.

4. **"The INVITE omits the MTSI headers Jio's S-CSCF keys on"** — false,
   tested on the live line 2026-08-24. `Accept-Contact` with the MMTel ICSI,
   `P-Preferred-Service`, the `+g.3gpp.icsi-ref` Contact feature tag,
   `P-Preferred-Identity`, `Supported: 100rel, timer` and the full MTSI
   `Allow` list were all sent together and the intercept was unchanged to the
   second (183 at +226 ms with the same `cause=41`, same `msml@` contact,
   13.60 s of the same announcement, same `480 cause=31`).
5. **"The INVITE carries no `Security-Verify`"** — false. Our own `PRACK` and
   `ACK` in the failing call, and the 2-minutely `OPTIONS`, are all requests
   sent over the SA without it, and Jio's core routes and answers every one of
   them. See the follow-up doc for the capture.

Details of 4 and 5, and the `[vowifi] originating_headers` probe built to test
them, are in
[jio-vowifi-outbound-480-followup.md](jio-vowifi-outbound-480-followup.md).

## Working conclusion — **overturned 2026-08-24**

> The previous conclusion here was: *"an account/SIM-level entitlement flag —
> this line is provisioned for MT and SMS but not MO voice; nothing in this
> bridge's SIP stack can work around a service that isn't enabled."*
>
> **That is wrong.** This SIM places outbound calls perfectly well. See below.

## The control test: the same SIM originates fine over VoLTE

Run 2026-08-24 with the bridge stopped, the modem's own IMS re-enabled
(`AT+QCFG="ims",1` + `AT+CFUN=1,1`), and the call placed from the *module's*
VoLTE stack instead of ours:

```
+QCFG: "ims",1,1          # modem IMS enabled AND registered to Jio over LTE
+COPS: 0,0,"JIO 4G Jio",7 # E-UTRAN

>>> ATD+91XXXXXXXXXX;
  [  5.8s] CLCC voice: alerting     <- the destination handset actually rang
  [ 10.8s] CLCC voice: active       <- answered, media up
  [ 31.7s] NO CARRIER
```

Same SIM, same subscription, same network, same destination that our own stack
cannot reach. It alerted and connected.

**So MO voice is provisioned.** Every "the account isn't entitled" reading of
this bug is dead, and so is "nothing in our stack can fix it".

## What the difference actually is

The one variable the control test changed, besides whose stack signals, is the
**access network**:

| | our stack | the modem |
|---|---|---|
| access | VoWiFi — ePDG/IPsec, `P-Access-Network-Info: 3GPP-WLAN` | VoLTE — LTE, E-UTRAN |
| MT voice | works | (not tested) |
| SMS | works | works |
| **MO voice** | **intercepted, `480`** | **connects** |

Two candidates remain, and they are cleanly separable:

1. **Wi-Fi-calling outgoing is separately entitled.** MT-over-VoWiFi and
   SMS-over-VoWiFi are allowed on this subscription, MO-over-VoWiFi is not,
   and Jio expresses that as a TAS intercept rather than a `403`. This fits
   every observation, including the destination independence and the generic
   announcement.
2. **Something in our registration or INVITE that only bites on the Wi-Fi
   access.** Less likely after the header work above, but not excluded.

## The next test separates them

Run **our own stack over VoLTE** — `[volte]` instead of `[vowifi]`, which this
repo already supports (specs/015-017, `volte-call`/`volte-bridge`) on the same
modem and SIM. It is a config change on the Pi, not a rebuild.

- Our stack originates fine over LTE → the fault is specific to the Wi-Fi
  access, i.e. candidate 1, and the fix is a conversation with Jio, not code.
- Our stack is intercepted over LTE too → the fault is ours, not the access,
  and the modem's INVITE is the reference to diff against.

Until that runs, "carrier limitation" is **not** a safe thing to write down.

## Where the decision is taken

Every capture puts a terminating application server in the path
(`Record-Route: <sip:tn3scfx6617mw…;interface=isc>`) and the `cause=41` is
already stamped when the 183 comes back ~220 ms later — before any destination
is alerted, and regardless of destination, P-CSCF (three different ones seen),
media-server IP, or header set. That is a service decision taken by Jio's TAS. The control test above shows
it is *not* a decision about the subscription as a whole — the same
subscription originates fine over LTE — so it is a decision about this call,
this access, or this registration.

## The announcement says nothing

Decoded and listened to, 2026-08-24 (`tools/rtp2wav.py` on a `veth-sip0`
capture): *"Your call cannot be completed at the moment, please try again
later."* Generic network boilerplate — no barring notice, no balance notice,
no "this service is not activated". It rules out the hope that Jio would name
the reason, and it is consistent with either an entitlement refusal or a
routing failure inside Jio's TAS. It does not distinguish them.

## Circuit-switched comparison is not available on this line (but VoLTE is)

The obvious control test — place the same call with this SIM off our IMS stack
— cannot be run on Jio. Measured on the modem 2026-08-24:

```
AT+COPS?          +COPS: 0,0,"JIO 4G Jio",7      # 7 = E-UTRAN, LTE only
AT+QCFG="ims"     +QCFG: "ims",2,0               # 2 = the modem's IMS is disabled
ATD+91…;          OK / NO CARRIER (0.3 s)        # no attempt reaches the network
```

Jio operates no 2G/3G, so there is no circuit-switched service to fall back
to, and the modem's own IMS stack is deliberately off (`modem-ims`, so it does
not re-register our IMPU and tear our binding down). `ATD` therefore fails
instantly and tells us nothing about provisioning.

The equivalent control test is to re-enable the modem's own IMS
(`AT+QCFG="ims",1` + a module reboot) and dial from *its* VoLTE stack — same
subscription, same network, a stack Jio certainly trusts. **That was run, and
it is what overturned this document's conclusion** (see above). It costs the
ePDG tunnel and this line's registration for ~10 minutes and needs reverting
afterwards; the line was verified back to `state: Registered`,
`gm_connection: up` and no SMS errors when it was.

## Next steps

- ~~**Get the announcement transcribed.**~~ Done — see above; it says
  nothing useful. The capture recipe, kept because it is the general way to
  hear any call on this bridge without a rebuild:

  ```bash
  # on the Pi, while placing a call:
  sudo tcpdump -i veth-sip0 -n -s 0 -w /tmp/annc.pcap udp
  # then, locally — Agent A's transcoded L16 toward Agent B's RTP port:
  python3 rtp2wav.py annc.pcap out.wav <agent-B-rtp-port>
  ```

  The port is in the log line `Agent B advertised a non-veth RTP address …
  using=10.99.0.2:<port>`. L16/16000 is big-endian PCM: strip the 12-byte RTP
  header, byte-swap, write a WAV header. (Capturing on the veth avoids
  decoding the carrier's AMR-WB, and avoids the Gm ESP entirely.)
  `siptest`'s own `[media].recording_dir` will **not** hold it if the softphone
  is behind NAT from the Pi — the relayed early media never reaches it.
- **Place an outbound call with this SIM off our IMS stack entirely** — the
  modem's own CS/VoLTE origination (`ATD`) on the same subscription. This is
  the one test that separates "the account is not provisioned for MO voice"
  from "our client is doing something wrong", and nothing in this document
  can distinguish those two. It needs care: the modem is carrying the live
  tunnel's USIM traffic.
- **Run our own stack over VoLTE** (`[volte]`, same modem and SIM) — the test
  that separates "Wi-Fi-calling outgoing is not entitled" from "our INVITE is
  at fault". Config change, no rebuild. See "The next test separates them".
- Then, and only if that points at the access: ask Jio whether "WiFi Calling
  outgoing" is entitled separately from base VoWiFi on this SIM. Worth asking
  with the VoLTE result in hand, since "outgoing calls don't work" alone will
  get the generic script.
- If Jio confirms MO voice isn't provisioned and there's no way to enable
  it, downgrade this from "bug to fix" to "known carrier limitation" in
  `docs/todo.md` and stop pursuing it.

## Reproduction

```bash
# Pi already has [vowifi] configured with the Jio line (ec20-11).
# siptest registers as extension 1002 and dials through the bridge:
siptest call --destination <E.164 destination>
```

Watch `/tmp/ims-agent-0.out` inside the `gsm_gsm-sip-bridge_1` container for
the `183`/`480` pair; `docker logs` alone only shows the Agent B/pjsua_safe
side, not the Gm-facing SIP.
