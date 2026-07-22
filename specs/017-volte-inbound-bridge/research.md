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
