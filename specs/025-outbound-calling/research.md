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

## R-003: How does a call get placed on a line owned by a different process? (REVISED 2026-08-03)

**Original decision (superseded below for the same-process case)**: a new,
small synchronous command listener (`control::line_server`) running in
*every* process that can host an idle line, including the daemon for CS
modems.

**What changed**: reading `gsm-sip-bridge/src/modules/mod.rs` closely shows
CS modems are **always** owned by the main daemon process's `CardPool`,
regardless of which process ends up hosting the SIP-side registration
(`owns_sip_side` only decides who registers/hosts the registrar, not where
`CardPool` runs — it always runs in the daemon). So whenever the daemon
itself is the process that received the INVITE, reaching a CS modem needs
**no cross-process hop at all** — it's a same-binary function call away.
And the exact mechanism for "async orchestrator commands a specific modem
thread and awaits a reply" already exists, built for `SetMode`/`Reboot`:
`ControlCmd` (`control/protocol.rs`) → `CardPool::handle_control_cmd`
(`modules/mod.rs:1163`) → `ModuleCmd` sent over each modem's
`crossbeam_channel::Sender` (held in `SlotState.cmd_tx`) → the modem's own
blocking loop (`run_module_loop`, spawned via `spawn_blocking`) polls it via
`cmd_rx.try_recv()` and replies through an embedded `tokio::sync::oneshot`.
`SetMode`'s handler (`modules/mod.rs:1196-1261`) is a **synchronous
round-trip already**: clone `cmd_tx`, fresh `oneshot::channel`, send,
`tokio::spawn` a bounded `timeout` await, forward to the caller. This is
identically the shape `ControlCmd::Dial` needs.

**Revised decision**: `ControlCmd::Dial { slot, destination }` /
`ModuleCmd::Dial(String, oneshot::Sender<Result<(), String>>)`, added
alongside `SetMode`/`Reboot`, handles the same-process (CS) case with zero
new IPC surface. The originally-planned `control::line_server`/`line_client`
socket (contracts/line-command.md) is **kept, but rescoped**: it is only
built for the genuinely cross-process case — reaching a VoWiFi/VoLTE line's
agent process from wherever the SIP side happens to be hosted (the daemon,
or a *different* VoWiFi/VoLTE agent). That work is deferred to a later
phase (plan.md Step 4) rather than being a Foundational blocker, since the
CS-only MVP doesn't need it at all.

**Rationale**: Building a new socket-based channel for a case that already
has a working, tested, in-process mechanism would have been the actual
complexity violation (Principle V) — the first pass's mistake was treating
"line selection can be cross-process" as "line selection is always
cross-process."

**Alternatives considered** (for the cross-process case, still valid):
- *Piggyback on `AgentReport`*: rejected, too slow (10s default heartbeat).
- *Single shared line-selection service all processes proxy through*:
  rejected — adds a fourth process/role to a system that already keeps
  "which process owns what" simple via `owns_sip_side` arbitration.
- *Move all lines into one process*: rejected — reopens the whole reason
  VoWiFi/VoLTE lines run in per-line network namespaces (spec 020).

**Race handling**: identical principle, cheaper mechanism for CS — the
`oneshot` round-trip through `ModuleCmd::Dial` is itself the serialization
point (the modem thread only processes one command at a time, in order), so
"claim before dial" falls out of the existing single-threaded-per-modem
design rather than needing a separate provisional-claim step for the
same-process case. The cross-process case (Step 4) still needs the explicit
claim-then-command sequence described in data-model.md.

## R-007: Accepting an inbound INVITE — the pjsua-safe UAS gap

**Decision**: Add `Call::from_id(call_id, state) -> Self` (safe — `Call`'s
only invariant is "this `call_id` is valid in PJSUA", no FFI needed for
construction) and `Call::answer(&mut self, code: u32)` (wraps
`pjsua_call_answer`, `#[cfg(feature = "pjsip-linked")]`/stub split identical
to `hangup`) in `pjsua-safe/src/call.rs`. Register
`cfg.cb.on_incoming_call = Some(on_incoming_call_cb)` in
`pjsua-safe/src/endpoint.rs` next to the existing `on_call_state`
registration, with the callback following the exact template of
`on_call_state_cb` (free `unsafe extern "C" fn`, SAFETY comment inline,
`pjsua_call_get_info` into a zeroed stack struct).

**Rationale**: `pjsua-sys`'s bindgen has no allowlist — it parses the whole
`pjsua.h` unfiltered (`pjsua-sys/build.rs:96-141`), so `pjsua_call_answer`,
`pjsua_acc_config.cb.on_incoming_call`, and `pjsua_call_get_info` are
already generated; no `pjsua-sys` change is needed, only new `pjsua-safe`
wrappers following the file's existing conventions. `CallState::Incoming`
already exists in the enum and `poll_state` already maps
`PJSIP_INV_STATE_INCOMING` to it — the state machine already anticipated
this, it was just never reachable because nothing ever produced an incoming
`Call`. Cost: ~3 new `unsafe` blocks against a 29-block/1.68% baseline on a
5% ceiling — ample headroom.

**Alternatives considered**: hand-roll the PBX/phone-facing leg's
signalling and media the way `ims::agent` does for the carrier leg —
rejected; that leg already has a complete, working PJSIP/media stack via
pjsua for every part except *accepting* the call. Duplicating call
setup/media/teardown there would be strictly more code and a second thing
to keep in sync, not less.

## R-008: VoWiFi/VoLTE outbound origination doesn't touch pjsua at all

**Finding** (corrects the framing of R-005 above, kept for the SDP/media
mirroring rationale which is still accurate): the carrier-facing IMS leg is
**entirely hand-rolled SIP**, independent of pjsua — `ims::agent` uses
`ims::sip_client`'s request/response builders and does its own SDP
(`ims::sdp`) and RTP relay over raw `UdpSocket`s
(`agent.rs::spawn_relay`/`transcode`). It never imports pjsua-safe types.
Only the *internal* veth-to-PBX hop (`vowifi::mod::bridge_call`,
`Call::make(account, &pbx_uri, ...)`) is pjsua-based — that hop is
unaffected by this feature.

Better still: `ims::call::run_call` (the `ims-call`/`volte-call` CLI
diagnostic tool) **already implements** working UAC INVITE origination
against the P-CSCF — `build_invite`/`build_ack`/`build_bye`/`InviteParts`
(`ims/call.rs:653-745`), `sdp::build_offer`/`parse_answer`, the works. It is
currently reachable only from the CLI, not from `ims::agent`'s live loop.

**Decision**: US4 (VoWiFi/VoLTE outbound) is a *reuse* task: generalize/
export `ims::call`'s UAC builders (or lift them beside `sip_client`'s
existing builders) and wire an origination trigger into `ims::agent`'s live
session state (its `ActiveCall`, `agent.rs:610-628`, currently only
populated `from_invite` — i.e. only for UAS-answered dialogs). No pjsua
changes, no new SDP/RTP code — the R-007 UAS work is irrelevant to this
path entirely.

**Rationale**: avoids a second, parallel outbound-INVITE implementation
when a tested one already exists; the only real work is making it live-loop
reachable instead of CLI-only.

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
