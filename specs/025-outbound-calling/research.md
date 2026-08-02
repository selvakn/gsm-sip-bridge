# Phase 0 Research: Outbound Calling

## R-001: How does a phone's INVITE (SIP server mode) become a real call?

**Decision**: The lightweight registrar (`sip::server`) answers a phone's
INVITE with `302 Moved Temporarily`, `Contact: sip:{aor}@{listen_addr}:{sip.local_port}`.
The phone re-INVITEs that URI, which lands on the pjsua-hosted, non-registering
`Account::local` (spec 024) — a real PJSIP account with a real media stack —
which accepts it as UAS and proceeds exactly like scenario R-004/R-005 below.

**Rationale**: The registrar is deliberately not a PJSIP module (spec 024,
R-001/R-002) — media handling lives entirely in pjsua, behind the
`pjsip-linked` feature that CI does compile for the daemon binary but the
registrar's own crate does not depend on. The registrar can construct and
send a SIP response (it already does, for REGISTER/OPTIONS) but cannot
negotiate SDP or carry RTP. A redirect keeps the registrar exactly as
simple as it is today — one more response type — and hands the parts that
need a real stack to the part of the codebase that already has one.

**Alternatives considered**:
- *Registrar becomes a full B2BUA*: rejected — this is the shape of change
  spec 024 explicitly avoided, and for the same reason: an authentication-
  and-now-media subsystem outside CI's `pjsip-linked` coverage.
- *pjsua listens on the registrar's port directly*: rejected — two SIP
  endpoints cannot share one UDP socket (spec 024's whole reason for the
  two-port design); this would just move the registrar's problem onto pjsua.

## R-002: How does the PBX's INVITE reach the bridge?

**Decision**: No new listener. The PBX sends the INVITE to the same
registered contact the bridge already establishes via `register_trunk` to
place its *own* calls (`sip::mod::SipBridge`'s existing trunk `Account`).
pjsua accepts it as UAS on that account.

**Rationale**: Confirmed by clarification session 2026-08-02 (spec.md) —
standard SIP trunking is bidirectional once registered; adding a second PBX-
facing address would be an unused knob for a channel that already exists.

**Alternatives considered**: a dedicated inbound address/port for outbound
requests — rejected per the clarification; no reasonable use case
distinguishes it from the existing trunk contact.

## R-003: How does a call get placed on a line owned by a different process?

**Decision**: A new, small synchronous command listener
(`control::line_server`) runs in every process that can host an idle line
(the daemon itself for CS modems, each VoWiFi/VoLTE line agent for its own
line). The process that owns the SIP side and received the INVITE — and
therefore knows every line's last-reported idle/busy state from the existing
`AgentReport` stream — picks a candidate and calls it directly over this new
channel with `PlaceCall { destination }`, waiting synchronously for
`Placed | Busy | Failed(reason)`.

**Rationale**: The existing control socket is one-directional and slow
(agent→daemon, `agent_report_interval_seconds` = 10 s default) — see plan.md
Complexity Tracking. A phone or PBX INVITE cannot wait a heartbeat interval.
The alternative of making the *entire* control protocol bidirectional and
low-latency was rejected as larger than necessary: only the "place a call
now" path needs synchronous round-trip semantics, so it gets its own small
listener rather than restructuring the existing reporting channel, which
continues to work exactly as it does today.

**Alternatives considered**:
- *Piggyback on `AgentReport`*: rejected, too slow (plan.md Complexity
  Tracking).
- *Single shared line-selection service all processes proxy through*:
  rejected — adds a fourth process/role to a system that already keeps
  "which process owns what" simple via `owns_sip_side`/`register_trunk`
  arbitration; the SIP-owning process already has the state it needs to pick
  a candidate, it just needs to reach it.
- *Move all lines into one process*: rejected — reopens the whole reason
  VoWiFi/VoLTE lines run in per-line network namespaces (spec 020) for
  tunnel/IMS isolation.

**Race handling**: the SIP-owning process serializes outbound line selection
(one attempt claims a line's "selected, awaiting placement" state before
issuing `PlaceCall`, per FR-008/FR-009a) so two near-simultaneous requests
cannot both target the same last-idle line; the second sees it no longer
idle and is refused exactly as if no line had been idle at all.

## R-004: Circuit-switched dial-out

**Decision**: A new `AtCommander::dial(number: &str)` sends `ATD{number};`
(voice-call form, trailing `;` per 3GPP TS 27.007 semantics), alongside the
existing `answer_call` (`ATA`) and `hangup` (`AT+CHUP`).

**Rationale**: Directly symmetric with `answer_call`; same command/response
parsing path (`send_command`), same error mapping. No new AT command
category is introduced — this is the dial-out counterpart of a pattern
`AtCommander` already implements for CS calls.

**Alternatives considered**: none meaningfully different — this is the
standard, only way to originate a voice call on this modem family.

## R-005: VoWiFi/VoLTE dial-out

**Decision**: `ims::agent` gains an outbound path that originates an INVITE
toward the P-CSCF with the destination number as the Request-URI user part,
mirroring the SDP offer/answer and media setup the agent already performs
when *answering* a carrier-originated INVITE — the difference is which side
sends the initial INVITE, not how the resulting dialog is media-bridged.

**Rationale**: `ims::agent` already owns a full IMS registration and dialog
state machine for the inbound direction (spec 015/016/017); reusing its
existing SDP/media bridging code for the second half of an outbound call
avoids a second, parallel implementation of RTP/media handling for the
carrier leg.

**Alternatives considered**: a separate outbound-only IMS client — rejected,
duplicates registration and media logic that must otherwise be kept in sync
with the inbound path across every future IMS fix.

## R-006: Destination pass-through

**Decision**: Whatever appears in the SIP Request-URI user part of the
originating INVITE (PBX or phone) is used verbatim as the AT `dial()`
argument or the IMS INVITE's Request-URI user part — no transformation.

**Rationale**: Directly required by spec.md FR-010/FR-011; keeps this
feature's surface area to "pass the number through," consistent with the
decision (spec.md Clarifications) that dial-plan/access-code handling stays
entirely on the PBX side.
