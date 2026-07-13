# Inbound VoWiFi-to-SIP Bridge — Architecture

See `specs/011-vowifi-sip-bridge/` (spec, plan, research, data model, contracts, tasks) for the
full requirements and design rationale. This doc is a shorter map for future maintainers of the
code itself.

## The problem this solves

The existing GSM-SIP bridge answers circuit-switched calls arriving on a modem's cellular voice
channel and bridges them to a SIP/PBX destination. This feature adds a second, independent inbound
path: calls arriving over the carrier's VoWiFi/IMS service (via the ePDG tunnel, built into the
same image as the daemon — see `docker/`) are answered and bridged the same way. The two paths
coexist — the carrier network decides which one actually delivers a given call.

## Why two processes

The VoWiFi leg must run inside the ePDG tunnel's `ims` network namespace — that's the only place
the tunnel's routes and the Gm IPsec (kernel XFRM) policy negotiated during IMS registration
actually match. The SIP/PBX leg needs ordinary LAN reachability to the PBX, which only exists in
the container's default namespace. One process can't satisfy both constraints, so the feature is
two processes joined by a `veth` pair that `docker/entrypoint.sh` creates automatically (only when
`[vowifi].enabled = true` — see that script's structure):

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
- Relays RTP between the two legs, either as raw UDP byte-forwarding (`relay_rtp`, when both legs
  agreed on PCMU) or by terminating the codec on each side and re-encoding (`ims/transcode.rs`) —
  see the audio path below.
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

## The audio path — where the sample rate is decided

A carrier's VoWiFi call is often **AMR-WB: real 16 kHz wideband audio**. Getting it to the PBX
intact means nothing along the way may be 8 kHz — and three things in this bridge independently
could be. With `[vowifi].wideband = true` (the default):

| hop | codec | who decides |
|---|---|---|
| carrier → Agent A | AMR-WB / 16 kHz | `sdp::select_codec` prefers AMR-WB over PCMU |
| Agent A → Agent B (veth) | L16 / 16 kHz | `sdp::select_veth_codec`; Agent A decodes AMR-WB and sends the PCM uncompressed |
| Agent B's PJMEDIA conference bridge | 16 kHz | `EndpointConfig::clock_rate` |
| Agent B → PBX | G.722 / 16 kHz | `prioritize_wideband_codecs` ranks G.722 above PCMU |

The veth link is uncompressed (`L16/16000`, RFC 3551) on purpose: it is a point-to-point link
inside one container, so its 256 kbit/s costs nothing, it is lossless, and it means Agent A — which
speaks RTP by hand — needs no new codec at all, since decoding AMR-WB already leaves it holding
16 kHz PCM. G.722 is likewise chosen over Opus because pjproject builds it in with no external
library and every mainstream PBX already speaks it.

**Narrowband still works, unchanged.** A carrier that offers only PCMU or AMR-NB (both 8 kHz —
Airtel sends both shapes) is answered exactly as before, with the veth link staying on PCMU: byte
passthrough for PCMU, transcode for AMR-NB. Each fallback is independent, so a PJSIP build without
L16, or a PBX that won't take G.722, degrades one hop rather than failing the call. `[vowifi].wideband
= false` puts every leg back at 8 kHz.

Two build-time facts this rests on, both in `docker/`: pjproject registers **L16 only at 44.1 kHz**
unless told otherwise, so `docker/pjsip-config-site.h` sets `PJMEDIA_CODEC_L16_HAS_16KHZ_MONO`; and
G.722 needs nothing. The negotiated codec and rate of every leg is logged when a call connects —
Agent A logs `carrier_codec`/`veth_codec` with their sample rates, and `on_call_media_state_cb`
logs what PJSIP settled on with the PBX.

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
