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

