# Quickstart: Inbound Call Bridging over the Host-Side LTE Registration

**Feature**: `017-volte-inbound-bridge` | **Date**: 2026-07-22

## Prerequisites

| Item | Requirement |
|---|---|
| Registration | Features 015 + 016 complete and verified on live hardware |
| Hardware | Quectel EC200U, Vi India SIM, host interface `enx024bb3b9ebe5` |
| P-CSCF | `2400:5200:a100:819::6`, or captured automatically from the Wi-Fi path |
| Build | **The container build** — a plain local build lacks the wideband codec |
| Telephone system | Reachable, with somewhere for the call to land |
| Second phone | To dial in, and to send a text |

**Stop anything else holding the registration** — the Wi-Fi agent, the
registration loop, or a diagnostic call. They displace each other, and the
service will refuse to start rather than fight.

## Step 0 — Confirm calls still reach us

Already established (research R1) but cheap to re-confirm, and it isolates
"the network is fine" from "our service is broken" before you debug the harder
thing:

```bash
gsm-sip-bridge volte-listen --iface enx024bb3b9ebe5 --pcscf <addr> --listen-secs 120
```

Dial the SIM. Expect a registration-event notification within a second
(the positive control), then an incoming call, then an acknowledgement of the
busy response. If **nothing** arrives, stop — the problem is below this feature.

## Step 1 — Gate B1: the first bridged call

```bash
gsm-sip-bridge volte-bridge --iface enx024bb3b9ebe5 --pcscf <addr>
```

Dial the SIM from the second phone. Expect:

1. The service reports an incoming call, with the caller's number **and display
   name** — the network supplies both.
2. Your telephone system rings.
3. Answer it. Talk for at least 60 seconds, both directions.
4. Hang up from the calling side; the telephone-system leg should end too.
5. Repeat, hanging up from the *telephone-system* side; the caller's phone
   should show a normal end.

**This is Gate B1.** Both legs, audio both ways, clean teardown from either end.

## Step 2 — Gate B2: is the call given voice treatment?

While the call is up, on a **second** AT port (the service owns the first):

```
AT+CGEQOSRDP     # look for a voice-class context that was not there before
AT+CGACT?        # look for an additional active context
```

For outgoing calls this showed a dedicated context at the voice quality class
with 136 kbps guaranteed, present only for the call. **Record whether the same
happens inbound.** Its absence is a valid result, and would mean inbound audio
is carried as ordinary data — which cost roughly 45× the packet loss on the
outbound experiment.

## Step 3 — Gate B4: text messages, both routes

```bash
# with the service running, from the second phone:
#   send a text to the SIM
```

Expect it recorded and forwarded to your usual destination, indistinguishable
from today. **Note which route delivered it** — over the registration or
through the modem — because that is unmeasured and both are handled.

Then check the awkward cases:

- Send a text **during a call**. Both must be handled; neither displaces the other.
- Restart the service with a text already sitting in modem storage. It must be
  recovered, not stepped over.
- Take the forwarding destination down and send a text. It must still be
  recorded, and the forwarding failure reported.

## Step 4 — Gate B3: the dashboards

With the service running and at least one call handled:

- Open your existing call dashboards. The call should appear **without any
  panel being modified** — the measurement already carries a transport
  dimension and this adds a value to it.
- Look for panels that **group by** transport: those will now split into two
  series. That is expected and visual, not a broken query, but identify them.
- Check that registration health is still distinguishable per path — you should
  be able to see this registration down while the Wi-Fi one is up.

## Step 5 — US2: leave it running

The real test of a service is time, not a first call.

```bash
# leave it running for at least 4 hours
gsm-sip-bridge volte-status     # periodically
```

Across that window expect several registration renewals and **at least one
attachment teardown** — the carrier does this roughly every two hours. Then:

- Dial in at the end. It must be answered.
- Confirm no renewal or re-attachment ever interrupted a call.
- If you can, be mid-call when a teardown happens: the call must end with the
  attachment named as the cause, and the service must recover.

## Step 6 — The rest

```bash
# second concurrent call: dial from a third phone while a call is up
#   → expect busy, and the first call undisturbed

# telephone system unreachable: stop it, then dial in
#   → expect the caller to get a defined outcome quickly, not silence

# conflict refusal: start the Wi-Fi agent, then start this service
#   → expect refusal, with the reason
```

### Success criteria mapping

| Check | Criterion |
|---|---|
| Call answered and reaches the telephone system promptly | SC-001 |
| Conversation both ways for 60s | SC-002 |
| 4 hours across a teardown, then a call connects | SC-003 |
| No call interrupted by maintenance | SC-004 |
| No silent call reported as successful | SC-005 |
| Every failure names its stage | SC-006 |
| Wi-Fi suite unchanged **and a live Wi-Fi call completes** | SC-007 |
| One implementation serves both paths | SC-008 |
| Status alone says whether a call can be answered | SC-009 |
| Text recorded and forwarded, indistinguishable from today | SC-010 |
| No text lost, none duplicated, whichever route | SC-011 |
| Calls appear in existing dashboards unmodified | SC-012 |
| This registration's health distinguishable from the Wi-Fi one | SC-013 |

## Warnings

**A card here has no fallback.** Exclusive assignment means the
circuit-switched daemon no longer drives it, so while this service is down that
card takes **no calls at all** — where today it would still ring. That is an
accepted cost, and it is why the availability reporting matters.

**Nothing else may hold the registration.** The Wi-Fi agent, the registration
loop and a diagnostic call all present the same identity and displace each other.

**Acknowledge after recording, never before.** For messages, the ordering is the
whole safety property: acknowledge late and the network retries; acknowledge
early and a crash loses the message outright.
