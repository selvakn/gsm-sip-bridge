# Inbound VoWiFi-to-SIP Bridge — Architecture

See `specs/011-vowifi-sip-bridge/` (spec, plan, research, data model, contracts, tasks) for the
full requirements and design rationale. This doc is a shorter map for future maintainers of the
code itself.

## The problem this solves

The existing GSM-SIP bridge answers circuit-switched calls arriving on a modem's cellular voice
channel and bridges them to a SIP/PBX destination. This feature adds a second, independent inbound
path: calls arriving over the carrier's VoWiFi/IMS service (via the ePDG tunnel in `docker/epdg/`)
are answered and bridged the same way. The two paths coexist — the carrier network decides which
one actually delivers a given call.

## Why two processes

The VoWiFi leg must run inside the ePDG tunnel's `ims` network namespace — that's the only place
the tunnel's routes and the Gm IPsec (kernel XFRM) policy negotiated during IMS registration
actually match. The SIP/PBX leg needs ordinary LAN reachability to the PBX, which only exists in
the container's default namespace. One process can't satisfy both constraints, so the feature is
two processes joined by a `veth` pair that `docker/epdg/entrypoint.sh` creates automatically:

```
          netns "ims"                                    container default netns
        ┌─────────────────────────────┐                ┌─────────────────────────────┐
 P-CSCF │  Agent A (vowifi-ims-agent)  │  veth-ims  <->  │  Agent B (vowifi-sip-agent) │  PBX
◀──────▶│  gsm-sip-bridge/src/ims/     │  veth-sip       │  gsm-sip-bridge/src/vowifi/ │◀────▶
  IMS   │  agent.rs                    │                │  mod.rs                     │ SIP
        └─────────────────────────────┘                └─────────────────────────────┘
```

## Agent A — `gsm-sip-bridge/src/ims/agent.rs`

- Keeps a persistent IMS-AKA registration alive via `super::register_session` (kept alive rather
  than immediately cleaned up, unlike the one-shot `ims-register`/`ims-call` CLI tools), renewing
  it before `Expires` lapses (`renewal_due` in `ims/mod.rs`) with exponential backoff on failure
  (`next_backoff`).
- Acts as a SIP UAS on **two** fronts for a single call: the carrier's Gm-protected IMS transport
  (answering inbound `INVITE`s), and a second, unauthenticated plain-SIP link on
  `crate::vowifi::VETH_SIP_PORT` that Agent B's PJSIP dials into once it decides to bridge a call.
  Both reuse the same `SipRequest`/`build_*` primitives in `ims/sip_client.rs` — only the
  carrier-facing side needs IMS-AKA/Gm-IPsec.
- Relays RTP between the two legs as raw UDP byte-forwarding (`relay_rtp`), not
  decode/re-encode — both legs are PCMU by construction (an AMR-WB-only carrier offer is declined,
  since there's no transcode path to Agent B's fixed-PCMU PJSIP leg yet).
- Answers `vowifi-status` registration-health queries on `crate::vowifi::AGENT_A_STATUS_PORT`.

## Agent B — `gsm-sip-bridge/src/vowifi/mod.rs`

- Builds its own `pjsua_safe::Endpoint`/`Account` rather than reusing `crate::sip::SipBridge`:
  `SipBridge` holds a single `active_call: Option<Call>`, correct for the circuit-switched bridge
  but incompatible with holding *two* concurrent calls and pairing them.
- On `IncomingCall` from Agent A, places two PJSIP calls — one to the PBX (reusing the
  destination-URI and caller-ID header logic the CS bridge already uses) and one back to Agent A's
  veth-facing UAS — and pairs them via `pjsua_safe::Endpoint::pair_calls`, so PJSIP's conference
  bridge connects their media once both reach `PJSUA_CALL_MEDIA_ACTIVE`.
- Tracks recent call outcomes in a bounded ring buffer (`RecentCalls`) and answers
  `vowifi-status` queries with them.

## The PJSIP two-call bridge — `pjsua-safe/src/endpoint.rs`

The existing CS-GSM bridge's `on_call_media_state_cb` unconditionally bridges a call's conference
slot to slot 0 (the sound device). This feature generalizes that: a `BRIDGE_PAIRS` registry
(`Endpoint::pair_calls`/`unpair_call`) lets two calls be paired to bridge to *each other* instead.
A call with no pairing registered falls back to the original slot-0 behavior unchanged — this is
the only path the CS-GSM bridge exercises, so its behavior is byte-for-byte unmodified (FR-006).

## Agent A ↔ Agent B control channel — `gsm-sip-bridge/src/vowifi/control.rs`

Newline-terminated JSON over TCP on the veth link (`contracts/agent-control-protocol.md`), mirroring
the wire framing of `crate::control::protocol` (the CLI↔daemon protocol) without reusing its
request/single-response shape — this protocol is event-driven in both directions. Agent B is the
TCP server (`[vowifi].control_port`); Agent A connects per call. `StatusQuery` is a third use of
the same channel, for `vowifi-status`.

## Configuration

`[vowifi]` in `config.toml` (see `config.toml.example`) — disabled by default. Both agents read the
same config file; the SIP/PBX destination itself reuses the existing `[sip]`/`[bridge]` sections
rather than duplicating them.

## What's verified vs. what needs real hardware

Everything above is unit- and integration-tested without live hardware (175 `gsm-sip-bridge` lib
tests, plus a `pjsip-linked`-gated test exercising the two-call pairing against a real linked
PJSIP) — see `specs/011-vowifi-sip-bridge/tasks.md` for the full breakdown. A live inbound call
over a real carrier network, through both agents, to a real PBX extension has **not** been
exercised end-to-end — that requires the physical Quectel EC200U, a VoWiFi-provisioned SIM, and a
reachable PBX, none of which were available during implementation. Run
`specs/011-vowifi-sip-bridge/quickstart.md` against real hardware before relying on this in
production.
