# Phase 0 Research: Inbound Call Bridging over the Host-Side LTE Registration

**Feature**: `017-volte-inbound-bridge` | **Date**: 2026-07-22

Everything marked **verified** was either executed against the live EC200U +
Vi India SIM, or read directly from the tree.

---

## R1: Does the carrier deliver incoming calls to us? **(RESOLVED — YES)**

**Status**: ✅ **Verified on live hardware before any bridging code was
written.** The spec called this possibly fatal; it is not.

A probe held the registration open with the protected server port listening and
declined calls with a busy response. Over one window:

```
NOTIFY from <sip:+918807793613@ims.mnc043.mcc404.3gppnetwork.org>   ← control
INVITE from "Selvakumar Natesan" <sip:+919789063708@...>            ← ×4
ACK    from "Selvakumar Natesan" <sip:+919789063708@...>            ← ×4
```

Four incoming calls, each carrying the caller's number **and display name**,
each followed by an `ACK` of our `486 Busy Here` — so the inbound path works in
**both directions**, not merely inbound.

**Two process notes worth keeping.** The first probe attempt saw nothing and
looked like a clean negative. It had **no positive control**, so it could not
distinguish "the carrier does not route calls here" from "our port is
unreachable" — findings that demand opposite responses, one of which would have
killed the feature. Subscribing to registration events first, purely as a
control, is what made the result interpretable; the notification arrived 95ms
after registering.

A hypothesis raised and **disproved**: that disabling the modem's own IMS stack
would make the network fall back to circuit-switched paging. The modem's AT
ports were watched throughout and it was never paged.

---

## R2: One registration serving both liveness and calls

**Decision**: Reuse the Wi-Fi agent's approach — defer renewal while a call is
active.

**Status**: ✅ Verified by code audit. **Already solved.**

The spec named this the feature's central problem. It is already solved in
`ims/agent.rs`, with the reasoning recorded at the site:

```rust
// Never renew mid-call — that would tear down the transport
// a call's own signaling (e.g. the eventual BYE) still
// needs; renewal is deferred until the call ends.
if active_call.is_some() {
    continue;
}
```

So FR-009 is reuse, not invention. What is **not** already handled is the
interaction with attachment loss (research R15 of feature 015): the LTE
attachment is torn down by the carrier roughly every two hours and the
registration loop re-attaches automatically. That re-attach must not fire
mid-call either, and a call in progress when the attachment genuinely dies must
end with the cause stated (FR-011).

---

## R3: Single process — what the two-process split actually costs

**Decision**: One process holding both the carrier leg and the telephone-system
leg. No private link, no control protocol between them.

**Status**: ✅ Decided; one real constraint discovered.

The Wi-Fi path splits into Agent A (carrier side, inside the tunnel's network
namespace) and Agent B (telephone-system side, outside it) because the
telephone-side library cannot cross that boundary. The LTE path has no
namespace (feature 015 research R4), so the split buys nothing here.

**The constraint that survives the merge**: the telephone-side library is
effectively a per-process singleton, and the codebase already carries a scar
from it —

> reusing `[sip].local_port` for both means two independent
> `pjsua_create`/transport-bind calls racing for the same UDP port, which fails
> outright for whichever one starts second

That is why `AGENT_B_SIP_LOCAL_PORT` exists as a separate constant. This
service will run alongside the circuit-switched daemon in the same container
and same network namespace, so **it needs its own local port too** — a third
one. Reusing either existing port would reproduce exactly the race that
constant was created to avoid.

**Alternatives considered**:
- *Reuse the Agent A/B split* — rejected. A veth pair, a control protocol and a
  second process, for an isolation boundary that does not exist here.
- *Run inside the circuit-switched daemon's process* — rejected. That daemon
  owns cards this service must not touch (R6), and merging their lifecycles
  makes one subsystem's crash the other's outage.

---

## R4: Text messages — both delivery routes

**Decision**: Handle messages arriving over the registration **and** messages
the network leaves in the modem's own storage.

**Status**: ✅ Both paths exist in the tree and are reused.

| Route | Existing code |
|---|---|
| Over the registration | `ims/agent.rs` handles inbound `MESSAGE` (RFC 3428) and acknowledges it |
| Through the modem | `modules::handle_cmti` → `sms/reader.rs` (`AT+CMGR`, `AT+CMGD`) |
| Record + forward | `sms::record_and_forward` — shared by both |

**Why both are needed** is the part that is easy to miss. Two decisions in the
spec combine to open a hole: messaging is in scope, *and* card assignment is
exclusive so the circuit-switched daemon no longer reads the modem. Our
registration advertises voice capability but **not** messaging capability —

```
Contact: <sip:...>;+g.3gpp.icsi-ref="...icsi.mmtel";audio;+sip.instance="..."
```

— so the carrier may well keep using the modem route. Nothing would then read
those messages, and they would accumulate unread until storage filled.

**Note**: the Wi-Fi path *does* receive messages over IMS with this same
Contact, so a messaging feature tag is evidently not required for delivery.
Whether Vi behaves the same on LTE is unverified, which is precisely why both
routes are covered rather than one being chosen.

---

## R5: Observability — the shared label already exists

**Decision**: Report call activity through the existing measurements, tagged as
this path; keep registration and attachment measurements separate.

**Status**: ✅ Verified by code audit. **Cheaper than expected.**

The existing call metrics are already labelled:

```rust
&["module", "status", "transport"]     // CALLS_TOTAL, SIP_CALLS_TOTAL
&["module", "transport"]               // ACTIVE_CALLS
```

There is already a `transport` dimension. Adding a new value to it is
**additive** — existing dashboard queries continue to match, because label
matching is by subset. So FR-030 and FR-032 are satisfied by using the existing
metrics with a new transport value, with no new metric names and no dashboard
changes.

Registration and attachment stay on the separate `gsm_bridge_volte_*` gauges
introduced in feature 015, which is what lets an operator tell *which*
registration is down.

**Risk that remains**: a dashboard panel that *groups by* transport will now
split into two series. That is a visual change rather than a broken query, but
FR-032 asks for it to be verified rather than assumed.

---

## R6: Exclusive card assignment

**Decision**: Extend the existing subsystem assignment so a card belongs to
exactly one of: circuit-switched, Wi-Fi calling, or this service.

**Status**: ✅ The mechanism exists and the hazard is already documented.

Discovery already assigns modems and already knows this hazard by name —
"modem claimed by both subsystems" — with a live symptom recorded: probing a
port another subsystem was mid-transaction on produced
`AT+CPIN?: no status in response` on an already-registered line.

So exclusivity is not a new concept, just a third member. The **cost** is
stated in the spec and is real: a card here has no circuit-switched fallback,
so when this path is down that card takes no calls at all.

---

## R7: Answering with the right audio format

**Decision**: Choose the answer's format with the same deliberateness the
offer's ordering needed.

**Status**: ✅ The mechanism exists; the lesson is carried forward.

`sdp::build_answer(local_ip, rtp_port, session_id, offer, amr_available,
wideband) -> (String, ChosenCodec)` already selects from the caller's offer and
already takes a `wideband` preference.

Feature 016 research R10 is the reason this gets its own decision: on the
outbound path, offering narrowband *first* caused the carrier to select it, and
packet loss went from 0.3% to 13.6% — because the network grants the
conversational-voice bearer based on what was negotiated. Answering a
mobile-terminating call carelessly would reproduce that, in the direction that
matters more, since an inbound call is a real conversation rather than a test.

---

## R8: Status as a live query

**Decision**: Answer over the existing control channel, reusing its message
shapes.

**Status**: ✅ The protocol already carries what is needed.

`vowifi::control::ControlMessage` already defines `RegistrationStatusReply`
(state, registered_at, expires_at, last_failure), `IncomingCall`, `CallEnded`
and `SmsReceived`. `vowifi-status` already queries agents this way.

The status file written by the feature-015 registration loop stays for that
command, but is not the mechanism here: it cannot answer "is a call in progress
*right now*", which US3 explicitly asks for.

---

## Unresolved items carried into planning

| ID | Item | Blocking? | Where resolved |
|---|---|---|---|
| R9 | Whether the network grants the conversational-voice bearer for **incoming** calls as it does for outgoing | No — measured, not gated | Gate B2, first live bridged call |
| R10 | Whether Vi delivers messages over the registration on LTE, or via the modem | No — both routes covered | Observed during the first message test |
| R11 | Whether a dashboard panel grouping by transport splits visibly | No | Gate B3, checked against the running stack |
| R12 | What a call outliving its registration actually does at the network | No — spec chooses to let it continue | Observed on a long call |

---

## R13: SC-008 verified mechanically, not by assertion

**Status**: ✅ Verified after Phase 6.

FR-019/SC-008 require registration, authentication, signalling protection,
call handling and audio to exist **once** and serve both paths. A copy would
satisfy neither while looking like it did, so this was checked by counting
definitions rather than by reading:

| Capability | Definitions |
|---|---|
| `register_session`, `dispatch_loop`, `handle_invite` | 1 each |
| `serve_inbound`, `run_telephony_side` | 1 each |
| `bridge_call`, `relay_rtp`, `attempt_renewal` | 1 each |
| `build_answer`, `spawn_gm_server`, `handle_message` | 1 each |

Both transports call the same two shared halves — `serve_inbound` (carrier
side) and `run_telephony_side` (telephone side) — from exactly one call site
each. The only apparent duplicates were test functions.

## R14: An acknowledgement-ordering defect found in the production Wi-Fi path

**Status**: ✅ Found and fixed while implementing US5.

`ims::agent::handle_message` acknowledged an inbound SIP `MESSAGE` **before**
relaying it for recording. A crash in that window loses the text outright, and
because the network was told it was delivered it never retries.

This was not found by testing the new path — it was found by writing down the
ordering rule US5 requires and then checking whether the existing code obeyed
it. It did not, and had not for as long as the Wi-Fi path has carried messages.

Fixed by reversing the order. A relay failure now leaves the `MESSAGE`
unacknowledged deliberately: the network retransmitting is the recovery
mechanism, and `volte::sms::Dedupe` absorbs the duplicate.

**Process note worth keeping**: the spec originally declared messaging out of
scope. Clarification pulled it in on the grounds that "out of scope" would mean
texts silently discarded. That decision is what surfaced this defect — the
feature would otherwise never have looked at this code.

## R15: The transport label was about to be silently wrong

**Status**: ✅ Found and fixed in Phase 5.

`metrics::ingest` hardcoded `transport="vowifi"` for every agent report. The
cellular service runs the *same agent code* as the Wi-Fi one, so every VoLTE
call would have been filed under `vowifi` — making the two paths
indistinguishable in exactly the comparison this whole effort exists to make,
while every dashboard continued to look healthy.

The label is now derived from the agent kind. `AgentKind::Volte` exists purely
for this distinction, with a test asserting the two do not collapse.

## R16: The LTE metrics were never published, and the entrypoint could never start the service

**Status**: ✅ Both found by running the service, both pre-existing, both fixed.

Two defects from feature 015 that no test could have caught, found within
minutes of actually starting the thing.

**The entrypoint could never start it.** When `[vowifi].enabled` was not true,
`entrypoint.sh` ran `wait; exit 0` — terminating before the host-side LTE block
far below it. Since enabling both sections together is fatal by design, the LTE
block was **unreachable in every possible configuration**. The VoWiFi stack is
now skipped rather than terminal, so execution reaches the LTE block either
way.

**Every `gsm_bridge_volte_*` metric was invisible.** They register into
`metrics::REGISTRY`; the scrape handler called `prometheus::gather()`, which
collects the *default* registry. The gauges were set faithfully and collected
by nobody. This is precisely the failure `sms::record_and_forward` already
warns about in its own doc comment — "would land in a Prometheus registry
nothing ever reads" — reached by a different route.

Found while checking SC-013, that this path's health is distinguishable from
the Wi-Fi path's. It was not — because it was not published at all. What made
it visible was asking the live `/metrics` endpoint rather than reading the code
that sets the gauges.

A third, milder one on top: agent reports routed the cellular service's
registration to the *VoWiFi* gauges, producing
`gsm_sip_bridge_vowifi_tunnel_up{module="volte"} 1` — claiming an ePDG tunnel
that does not exist on this path — while the VoLTE gauges read zero. An
operator alerting on either would have been told the opposite of the truth.

**Lesson, consistent with every previous feature in this series**: hardware
finds what unit tests cannot. Here it was not even hardware — merely *running
the service and reading its own output* found three defects that a green suite
had nothing to say about.

### Verified live after the fixes

```
[volte].enabled + bridge_inbound — answering inbound calls over LTE
IMS PDN established  apn=ims.mnc043.mcc404.gprs  bearer=6  IPv6-only
IMS PDN attached     iface=enx024bb3b9ebe5  routed=true
P-Access-Network-Info 3GPP-E-UTRAN-FDD  cell=40443D55E62E831F
registered to PBX    rsp.selvakn.in:6060   agent="volte-bridge"
listening for Agent A 127.0.0.1:5075       (loopback, not a veth)
REGISTER 401 -> 200 OK
registered, listening for inbound calls    agent="volte-ims-agent"
NOTIFY event=reg subscription_state=active

gsm_bridge_volte_registered 1
gsm_bridge_volte_pdn_up 1
gsm_sip_bridge_active_calls{module="volte",transport="volte"} 0
```

One process, both halves as threads, its own SIP port (5073) alongside the
circuit-switched daemon's — no bind race (research R3).

## R17: The first live bridged calls — three of ours, then one of theirs

**Status**: ⚠️ Bridge proven end to end; blocked on a carrier policy rejection.

Three dials, each finding something different.

### Dial 1 — the two halves could not find each other

Collapsing the Agent A/B split onto loopback gave the telephone-side half a new
port (`LOOPBACK_SIP_PORT`), but the carrier-side listener still bound the Wi-Fi
path's hardcoded `VETH_SIP_PORT`. Both sides now read one constant.

What makes this worth recording is **how far it got before failing**: the INVITE
arrived, both legs were placed, the PBX rang, and *a human answered* — only then
did it time out. A port mismatch that surfaces only after someone picks up a
phone is not the failure mode anyone predicts.

Second defect on the same dial: the setup failure sent no final response, so
the caller kept hearing the ringback our `180` started until they gave up. Now
answered with `480 Temporarily Unavailable` — not `486 Busy`, because the line
is not busy and the distinction is what tells a caller whether to redial now or
later (FR-005).

### Dial 2 — bridged, then dropped in 170ms

The bridge worked:

```
transcoding relay  carrier=AmrWb pt=101 octet_aligned=false <-> veth L16
call answered and bridged
... 170ms later: call ended, reason=caller_hangup
```

### Dial 3 — with SIP-level logging, the carrier said why

```
BYE
Reason: SIP;cause=503;text="PT: AAA: result_code=0 exp_result_code=5065"
```

Diameter experimental result **5065 = IP-CAN_SESSION_NOT_AVAILABLE**
(TS 29.214). The P-CSCF asked the PCRF over Rx to authorise media for the
session; the PCRF could not bind it to an IP-CAN session for our address.

Our signalling is correct and complete: `100`, `180`, `200 OK` with a valid
answer, and **the network ACKed it** (`received ACK, dialog confirmed`) before
tearing down 65ms later. This is a policy/charging rejection, not a signalling
fault.

### The control that made it interpretable: an outbound call now fails too

Rather than assume this was inbound-specific, the same registration was used to
place an outbound call — the exact thing feature 016 verified working, with a
dedicated voice bearer and 0.3% packet loss:

```
183 Session Progress -> 180 Ringing -> 380 Alternative Service
```

**So it is not inbound-specific.** Something in how the network treats this
PDN/registration has changed since feature 016. Candidates, none yet
distinguished:

- The PDN was torn down and re-attached ~6 times in quick succession during
  this session; a stale IP-CAN session or a rate limit at the PGW/PCRF would
  present exactly this way.
- `380 Alternative Service` conventionally means "use the CS domain", which
  would suggest the network is currently declining to serve this subscriber
  over PS at all.

This is the same lesson as R1: a negative result without a positive control is
uninterpretable. Checking the outbound direction cost one command and moved the
diagnosis from "the inbound bridge is broken" to "the network is currently
refusing both directions".

### Also found and fixed: a malformed `To` header on in-dialog responses

`build_uas_response` appended `;tag=` unconditionally. An initial INVITE
arrives untagged and our response establishes the dialog — correct. But an
**in-dialog** request (BYE) arrives with our tag already present, so we emitted
`To: <...>;tag=X;tag=X`, which RFC 3261 §8.2.6.2 forbids. Benign here (the
dialog was ending anyway) and live on the Wi-Fi path too.

### What is proven, and what is not

**Proven**: inbound INVITEs reach the service; both legs are placed and paired;
the PBX rings and answers; the carrier leg negotiates AMR-WB; the transcoding
relay starts; the dialog is confirmed by the network's own ACK. The bridging
code does what it was built to do.

**Not proven**: that a call survives, that audio flows both ways, and Gate B2
(whether a voice bearer is granted for mobile-terminating calls). None of these
can be answered until the network stops rejecting the session.

## R18: The 5065 rejection is not ours — everything under our control is verified correct

**Status**: ⚠️ Blocked on the carrier. Every hypothesis we could test is eliminated.

After the operator restarted and the service re-registered cleanly, three more
inbound calls were placed. All three bridged successfully and all three were
torn down with the identical reason:

```
Reason: SIP;cause=503;text="PT: AAA: result_code=0 exp_result_code=5065"
```

### What was eliminated, and how

> ⚠️ **Superseded by R19.** The first row of this table is wrong, and the
> conclusion that follows from it is wrong. The hypothesis was right; the test
> was incapable of falsifying it. Read R19 before acting on anything here.

| Hypothesis | Test | Result |
|---|---|---|
| Stale IP-CAN session from repeated re-attach | Operator restart, fresh PDN + registration | ⚠️ **Test was invalid — see R19.** A restart never detaches from EPS |
| Inbound-specific | Placed an **outbound** call on the same registration | ❌ Also fails (`380 Alternative Service`) |
| Wrong media address | Compared SDP `c=` against the interface | ❌ Correct; both globals are in the assigned `/64` |
| Illegal SDP answer | Compared answer against the offer, payload by payload | ❌ Legal — pt 101 was offered, fmtp echoed correctly |
| Bad signalling | Read the full exchange at SIP level | ❌ Correct — **the network ACKs our 200 OK**, then BYEs 65ms later |
| Modem's own IMS competing | `AT+QCFG="ims"`, `AT+QIMS?` | ❌ `2,0` / `DISABLE` — correctly off |
| Not attached to LTE | `AT+CEREG?` | ❌ `0,1` — registered, home |

The offer/answer pair from one call, for the record:

```
offer:  m=audio 15662 RTP/AVP 96 8 18 100 101 97 106
        a=rtpmap:101 AMR-WB/16000
        a=fmtp:101 max-red=0
answer: m=audio 32808 RTP/AVP 101
        a=rtpmap:101 AMR-WB/16000
        a=fmtp:101 max-red=0
```

Note also that the carrier's offer **differs between calls** — one carried EVS
plus four AMR-WB variants, another only AMR/AMR-WB with no EVS. Both were
answered correctly, and both were rejected identically, so codec selection is
not the variable.

### What this leaves

Diameter Rx 5065 is `IP-CAN_SESSION_NOT_AVAILABLE`: the PCRF could not bind the
media session to an IP-CAN session for our address. Every input we control is
correct, so the remaining explanations are carrier-side:

1. The subscriber is not (or no longer) provisioned for VoLTE media on a
   host-managed IMS PDN, and the PCRF has no Gx session to bind against.
2. `result_code=0` is not a valid Diameter result — it suggests the AAR got
   **no answer** from the PCRF, i.e. a P-CSCF↔PCRF failure internal to the
   network, with 5065 as the SBC's fallback interpretation.

Corroborating: **VoWiFi on this same SIM works perfectly today** (a full call
was bridged over the ePDG at 17:31 with AMR-WB and a clean teardown). The ePDG
path has its own policy binding, so a VoLTE-specific policy failure would look
exactly like this.

### The honest state of the feature

**The bridging code is done and proven.** Inbound INVITEs arrive; both legs are
placed and paired; the PBX rings and answers; AMR-WB is negotiated on the
carrier leg; the transcoding relay starts; the network confirms the dialog with
its own ACK. That is the whole of US1's signalling path, demonstrated
repeatedly on live hardware.

**Gates B1, B2 and the US2 soak cannot be closed from here.** They need the
carrier to authorise the media session. No amount of further work on this
codebase changes that, and pretending otherwise by declaring B1 passed on
"it bridged" would be exactly the kind of claim FR-017 exists to prevent.

## R19: It *was* the stale IP-CAN session — the test that "eliminated" it could not have detected it

**Status**: ✅ **RESOLVED on the outbound path.** A full EPS detach cleared it;
the next call connected with two-way audio. Inbound remains to be re-tested.

R18 closed with "no amount of further work on this codebase changes that". That
was wrong, and the way it was wrong is the most useful thing in this document.

### The invalid test

R18's first row eliminated "stale IP-CAN session" on the evidence of an
operator restart with a "fresh PDN". Measured directly this time, immediately
after a clean `docker stop` — i.e. after `pdn::tear_down` had run to completion:

```
AT+CGACT?    +CGACT: 1,0  +CGACT: 2,0  +CGACT: 3,0   ← all contexts down
AT+CGATT?    +CGATT: 1                               ← still attached
AT+CEREG?    +CEREG: 0,1                             ← registered, home
AT+CGPADDR   all zeros                               ← addresses released
```

`pdn::tear_down` issues `AT+CGACT=0` and nothing else (`pdn.rs:398`). It
deactivates the PDP context; it never detaches from EPS. **The UE never left
the network**, on any restart performed during this investigation, including
the operator's. So "fresh PDN" meant a new PDP context on a *continuous* EPS
attach — which is precisely the condition under which a stale Gx session at the
PCRF would survive. The test could not have distinguished the hypothesis it was
used to reject.

Every hypothesis in R18's table was tested against something we could observe.
This one was tested against something we could only observe *from the host*,
and the state that mattered lived in the core network.

### The actual detach, and what it changed

```
AT+CGATT=0   → +CGATT: 0,  +CEREG: 0,2    ← packet-domain detach, searching
AT+CFUN=4    → +CFUN: 4,   +CEREG: 0,0    ← RF off, fully deregistered
             ← 90 s quiesce with RF off →
AT+CFUN=1    → +CEREG: 0,1, +CGATT: 1     ← re-attached, stable over 3 polls
```

The PDN came back on an **entirely different prefix** — `2402:8100:78f1:dae4::/64`,
where every previous attach in this session had landed inside
`2402:8100:78b9:85f5::/64`. A new prefix is a new IP-CAN session, which is the
observable signal that the old one was actually released rather than reused.

### The result

An outbound call on the same registration, the same control R18 used to prove
the failure was not inbound-specific:

```
100 Trying → 183 Session Progress ×3 → 180 Ringing → 200 OK
call answered, starting RTP  remote_rtp=[2400:5200:a100:826::4]:40486  codec=AmrWb

call report
  direction : both-ways — audio flowed in both directions
  sent      : 975 packets / 312000 samples
  received  : 767 packets / 245440 samples
  loss      : 5 (0.6%)
  jitter    : 11.1 ms
```

No `380 Alternative Service`. No `5065`. Twenty seconds of conversational
audio in both directions at 0.6% loss — comparable to the 0.3% feature 016
measured on a healthy dedicated voice bearer, and 20-fold better than the
13.6% that feature's research recorded when the call was *not* given voice
treatment (016 research R10).

`volte-call` exits 0 only when the call was answered **and** audio flowed both
ways, so this is the tool's own verdict, not an interpretation of the log.

### Corrections this forces elsewhere

**R18's second remaining explanation is also wrong.** It read `result_code=0`
as "the AAR got no answer from the PCRF". A Diameter answer carries *either*
`Result-Code` *or* `Experimental-Result-Code`, never both; the SBC prints both
fields of its result structure and the unused one reads as `0`. The AAR **was**
answered — the PCRF affirmatively said `IP-CAN_SESSION_NOT_AVAILABLE`. Which
is exactly what a PCRF holding a stale session for a different address would
say, and is consistent with everything above.

Both of R18's "remaining explanations" were therefore carrier-side and neither
was correct. The cause was on our side after all: not in the bridging code, but
in what our teardown path fails to do.

### The lesson

R18 is a well-built elimination table with one row whose test was incapable of
producing a negative. The table's *form* — hypothesis, test, result — is what
made it persuasive, and the form is intact; only the power of one test was
absent, and nothing in the format prompted anyone to ask about it.

The general rule this suggests: **when a hypothesis concerns state held by
something outside the system under test, the test must be shown to have reached
that state.** "We restarted and it was identical" is evidence only if the
restart provably did something to the remote state. Here it provably did not,
and `AT+CGATT?` would have said so at any point over those several hours.

The corollary is that the fix belongs in the code, not in an operator runbook:
teardown that cannot detach is teardown that cannot recover. That is
`hardening.md` H2, which was written as speculative hardening an hour before it
turned out to be the root cause.

### Process note: the deployed image lagged the commits describing it

Noticed while preparing the re-test. The `docker-gsm-sip-bridge:latest` image
in use was built at 00:08; commit `42f783e` — which contains the `To`-header
fix R17 records as "found and fixed" — landed at 00:21. The live trace at 00:15
still shows `To: <...>;tag=f142a42c;tag=f142a42c`, so the image genuinely
predated the fix, and every call discussed in R17 and R18 ran on a binary
older than the research describing them.

None of it changes those conclusions — the defect is benign and 5065 is decided
well below SIP. But "we fixed it" and "the thing we tested had the fix" are
separate claims, and only the first was being tracked. The image is rebuilt from
`HEAD` before any gate is closed.

### What is proven now, and what is not

**Proven**: the carrier authorises media for this SIM on a host-managed IMS PDN
after a clean detach; outbound calls connect and carry two-way audio at
conversational quality.

**Not proven**: that an **inbound** bridged call now survives. The whole of
US1's signalling path was already demonstrated repeatedly (R17, R18) and the
only thing that ever killed those calls was the Rx rejection — but Gate B1
requires a call that rings the telephone system, is answered, and carries audio
both ways for 60 seconds, and no such call has been placed since the detach.
**B1 stays open until someone dials the SIM.** B2 (whether a dedicated voice
bearer appears for mobile-terminating calls) is likewise still unmeasured.


## R20: Hardening applied after the 5065 fix — and what was already covered

**Status**: ✅ Code changes landed. This folds in the durable content of the
former `hardening.md`, which was a working backlog written while the
investigation was blocked and is deleted now that its items are resolved.

The list originally opened "these are not fixes for the 5065 rejection." One of
them (H2) was the fix. That is why the backlog is worth preserving in the
research record rather than dropped: writing down what was
unprincipled-but-working is what surfaced what was already broken.

### The metrics-label bug the fix surfaced

Confirming the recovered calls were on VoLTE (not VoWiFi over some stray ePDG
tunnel) turned up a labelling defect of the same class as R15. The two call
counters disagreed on transport for the *same* calls:

```
gsm_sip_bridge_calls_total{module="volte",...,transport="volte"}      2   ✓
gsm_sip_bridge_sip_calls_total{module="volte",...,transport="vowifi"} 2   ✗
```

The bridge is one process with two independently-reporting halves. The carrier
half reports `AgentKind::Volte` → `transport="volte"`. The telephone half runs
the shared Wi-Fi telephony code, which reported `AgentKind::Sip` →
`transport="vowifi"`, so every VoLTE call's PBX-leg outcome
(`SIP_CALLS_TOTAL`) was filed under Wi-Fi. R15 fixed the gauges and
`CALLS_TOTAL`; this counter was missed because that code path legitimately *is*
`Sip` on the real Wi-Fi path.

Fixed by adding `AgentKind::VolteSip` — the telephone half of the VoLTE bridge,
to `Volte` what `Sip` is to `Ims`. It maps to `transport="volte"` but stays a
distinct kind, because the two halves are independent reporters with their own
`epoch`/`seq`: sharing one `(kind, module_id)` liveness key would let each
erase the replay-detection record the other needs, re-applying already-counted
events. The telephony reporter now derives its kind from the transport it is
bridging.

### The five hardening items

| ID | Resolution |
|---|---|
| **H2** | **Fixed — this was the root cause.** `pdn::tear_down` now forces a packet-domain detach (`AT+CGATT=0`) after deactivating the context, logging the `CEREG` transition so a detach that the modem refuses is visible rather than silent. See R19 for the confirmation. |
| **H1** | **Fixed.** `netcfg::configure_steps` now sets `autoconf=0` alongside `accept_ra=2`, so the accepted RA installs the default route but the kernel no longer mints a second, SLAAC-derived global from the MAC. One deterministic source address instead of two left to RFC 6724 tie-breaking. Not the 5065 cause (both globals sat in the same delegated `/64`), but nothing in the code *chose* the right one. |
| **H3** | **Already covered — no new code.** The re-attach back-off H3 asked for already exists: both renewal loops (`registration::run` and `ims::agent::serve_inbound`) back off exponentially, 5 s → 300 s, and `bring_up` is idempotent. The incident's churn was not a tight attach loop — it was teardown→restart *cycling*, each leaving a stale session, which H2 fixes. Adding another back-off would be redundant and would risk slowing the ~2-hourly recovery. |
| **H4** | **Closed — not implementable, tested on the modem. Do not retry.** A real VoLTE handset marks its context with the P-CSCF-discovery and IM-CN-signalling parameters of `+CGDCONT` (TS 27.007 positions 9–10). The EC200U caps `+CGDCONT` at eight parameters: `AT+CGDCONT=?` lists `(0,1),(0,1)` ending at position 8, and setting position 9 returns `+CME ERROR: 53` while eight parameters returns `OK`. This corroborates 015 Gate G1 from the *request* side (that gate tested the read side — `CGCONTRDP`, `QPCO`, `QCFG="pcscf"`), and it eliminates the theory that our PDN could be made to look "more IMS" to the network. Configuration-as-primary (`--pcscf`) remains the only route on this hardware. |
| **H5** | **Done.** R18's reading of `result_code=0` as "no answer from the PCRF" is corrected in R19: a Diameter answer carries either `Result-Code` or `Experimental-Result-Code`, never both, so the AAR *was* answered — with `IP-CAN_SESSION_NOT_AVAILABLE`. |
