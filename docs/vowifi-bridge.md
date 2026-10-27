# Inbound VoWiFi-to-SIP Bridge — Architecture

See `specs/011-vowifi-sip-bridge/` (spec, plan, research, data model, contracts, tasks) for the
full requirements and design rationale. This doc is a shorter map for future maintainers of the
code itself.

> **Credit:** the whole VoWiFi path (ePDG tunnel, IMS-AKA, Gm IPsec) follows the trail mapped
> out by the [Osmocom](https://osmocom.org/) project's
> [VoWiFi with Asterisk](https://osmocom.org/projects/foss-ims-client/wiki/VoWiFi_with_Asterisk)
> research — their foss-ims-client wiki, `strongswan-epdg` fork, and sysmocom's VoLTE Asterisk
> patches are the foundation this implementation was built and validated against.

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
two processes joined by a `veth` pair that `gsm-sip-bridge supervise` creates automatically (only
when `[vowifi].enabled = true` — see `gsm-sip-bridge/src/supervise/orchestrate.rs`;
`docker/entrypoint.sh` is now just a thin shim that execs into it, specs/021-entrypoint-supervise-rust):

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
  it before `Expires` lapses with exponential backoff on failure (`next_backoff`). The renewal
  falls due on the schedule of [3GPP TS 24.229 §5.1.1.4.1](https://www.etsi.org/deliver/etsi_ts/124200_124299/124229/18.10.00_60/ts_124229v181000p.pdf)
  (`registration_refresh_offset` in `ims/mod.rs`): 600 seconds before expiry when the registrar
  granted more than 1200 seconds, halfway through the grant otherwise, and never sooner than 60
  seconds after the registration. A one-hour grant therefore renews after 50 minutes. A grant
  under 120 seconds logs a warning, because the binding can lapse before its refresh.
- Refreshes the registration **in place**: a protected `REGISTER` on the live Gm connection with
  the authorization of the last accepted registration (the last nonce and the last calculated
  response, TS 24.229 §5.1.1.4.2), the registration's own Call-ID and a REGISTER-only `CSeq` that
  rises by one each time (RFC 3261 §10.2). It is a transaction owned by the dispatch loop
  (`ims/agent/registration_refresh.rs`): sent, retransmitted on the IMS timers (T1 2 s, T2 16 s) and
  timed out (Timer F, 128 s) from the loop's passes, so calls, notifications and SMS are served
  while it is outstanding. **An active call or a pending outgoing call does not defer it**; it
  leaves every dialog, route, the media and the registration-event subscription alone. Its answer
  is accepted only if it matches the request's Call-ID, CSeq number, method and Via branch, and the
  lifetime comes from this line's own `Contact`. The P-CSCF, not the UE, adds the
  `integrity-protected` parameter (TS 24.229 §5.2.2.2).
- Failures of a refresh keep the existing session. A `403`, `408`, `500` or `504` (and a fresh
  authentication challenge, `401`, until background re-authentication exists) asks for a full
  re-registration, which waits for a call to end like any disruptive recovery
  (`Maintenance::RegistrationRecovery`). A `503` with `Retry-After` longer than Timer F marks the
  P-CSCF unavailable and fails over; a shorter one retries the same P-CSCF after the wait. A `423`
  retries once with `Min-Expires`. After two final error answers in a row the next cycle uses the
  full re-registration, and in-place refresh is tried again after that succeeds. A registration
  that expires during a call is reported unregistered and the call is left running.
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

A line's SIM normally comes from a modem, bridged into pcscd's virtual reader by
`vowifi-usim-bridge`. It can instead come from a real PC/SC smart-card reader (e.g. OmniKey AG
3x21) with the SIM seated directly in it — see `docs/omnikey-pcsc-vowifi.md`.

## Phone-facing codecs

The PBX/phone leg can use AMR-WB, AMR-NB or Opus as well as G.722 and PCMU (spec 085, #109). AMR-WB
at about 12.65 kbit/s is roughly a fifth of G.722's 64 kbit/s, which matters on a metered or weak
link to the phone. `[sip].codecs` sets the order; unset, nothing changes for a phone that does not
ask for AMR or Opus, and one that prefers them can now negotiate them.

- The bridge still decodes and re-encodes: the carrier's audio is not passed through untouched (see
  #111 for passthrough).
- A PBX in the path must support the codec itself. Stock Asterisk has no AMR, so the built-in
  registrar (`[sip_server]` mode) with an AMR-capable softphone is the primary target.
- AMR is offered octet-aligned by default (`[sip].amr_octet_align`), because most softphones accept only that packing; set it to `false` for bandwidth-efficient. A phone that makes the offer is followed either way.
- PCMU must stay in the list; the internal link between the two agents falls back to it.
- The circuit-switched path never offers these codecs.

The negotiated phone-leg codec shows as `phone_codec=` in `vowifi-status` recent calls.

## AMR passthrough — `gsm-sip-bridge/src/amr_forward/`, `vowifi/forward.rs`

Off by default (`[vowifi].amr_passthrough`, inbound calls only). Normally Agent A decodes the carrier's AMR to PCM and Agent B encodes it again for the phone. When the carrier leg and the phone leg both settled on AMR-WB (or both AMR-NB) with compatible parameters, the speech frames are forwarded without being decoded.

**When a call is forwarded.** Same flavour on both legs; each leg's `mode-set` equal (unrestricted counts as every mode); no CRC, robust-sorting, interleaving or redundancy; frames per packet that fit the other leg; telephone-event negotiated on both legs or neither. Packing (octet-aligned vs bandwidth-efficient) may differ: it is converted frame by frame. Anything else is transcoded exactly as before, and the verdict can never fail a call. Carriers often restrict AMR-WB to `mode-set=0,1,2,3`; a phone that does not state the same restriction in its answer is, by the rule, transcoded (`reason=mode_set`). `[vowifi].amr_mode_set = "relax"` waives that for the phone-to-carrier direction: the call is forwarded, the codec mode request towards the phone is clamped into the carrier's set, and phone frames outside it are counted in the `mode-set guard summary` line (Agent A's log) at the end of the call. Live on Jio, a phone sending mode 8 against `mode-set=0,1,2,3` had working audio. `relax_drop` drops those frames instead, which silences a phone that always sends one mode.

**The upgrade sequence.** Every call starts on the transcoded path. After the phone answers, and before Agent B sends `CallAnswered` (so the carrier is not yet answered and no carrier audio has flowed), Agent B judges compatibility. Only a qualifying call is upgraded: `ForwardMedia` → `ForwardAck` (Agent A binds its end of a UDP hop to Agent B and keeps it pending) → Agent B starts its forwarder → the phone leg is re-INVITEd (`PJSUA_CALL_REINIT_MEDIA`) onto the forwarder's address, with PJSIP's own stream paused → the answer is re-checked → the veth call is unpaired and paused → `CallAnswered`. Any failure leaves a transcoded call (`ForwardCancel`; the phone is re-INVITEd back if it had already moved).

**Behaviour.** SSRC, sequence number and timestamp pass through untouched; only the audio and telephone-event payload types are relabelled per leg. Nothing is sent to a held phone, and a changed phone address is followed. A phone re-INVITE that drops the codec, or keeps it with parameters the call could no longer be forwarded under (a restricted `mode-set`, CRC, redundancy, a packet time the carrier cannot take, a telephone-event mismatch), is answered 488 and the call carries on; the offer is judged by the same rule that admitted the call. Changes the relay can follow (payload types, packing, address) are accepted. A phone behind NAT, whose RTP comes from an address other than the one in its SDP, is dropped by the source check unless `[vowifi].amr_client_rtp_latch = true`, which follows the first well-formed packet instead. RTCP towards the carrier is unchanged; the phone gets none.

**PBX caveat.** The phone side must pass AMR through. The built-in `[sip_server]` does; stock Asterisk does not (it would negotiate its own codec with the bridge, and the call is transcoded).

**Reading the path.** Agent B logs `media path decided call_id=… path=forwarded flavour=amr-wb carrier_packing=… client_packing=…` or `path=transcoded reason=…`, Agent A's `call answered and bridged` line has `forwarding=true|false`, and `vowifi-status` recent calls show `media_path=forwarded:amr-wb:oa/be` or `transcoded:<reason>`. Reasons: `disabled`, `outbound`, `carrier_not_amr`, `client_not_amr`, `flavour_differs`, `mode_set`, `feature`, `ptime`, `dtmf`, `ack_timeout`, `reinvite_failed`.

## What's verified vs. what needs real hardware

Everything above is unit- and integration-tested without live hardware (175 `gsm-sip-bridge` lib
tests, plus a `pjsip-linked`-gated test exercising the two-call pairing against a real linked
PJSIP) — see `specs/011-vowifi-sip-bridge/tasks.md` for the full breakdown. A live inbound call
over a real carrier network, through both agents, to a real PBX extension has **not** been
exercised end-to-end — that requires the physical Quectel EC200U, a VoWiFi-provisioned SIM, and a
reachable PBX, none of which were available during implementation. Run
`specs/011-vowifi-sip-bridge/quickstart.md` against real hardware before relying on this in
production.
