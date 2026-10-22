# Feature Specification: AMR passthrough for AMR-capable clients

**Feature Branch**: `120-amr-passthrough-forwarder`
**Created**: 2026-10-22
**Status**: Draft
**Input**: User description: "AMR passthrough for VoWiFi/VoLTE calls (issue #111, option 2a of #109). When a call's carrier leg and its client leg both negotiate AMR or AMR-WB with compatible parameters, Agent B carries the audio with a socket forwarder instead of PJSIP media: PJSIP keeps SIP/SDP only, via on_create_media_transport returning a pjmedia loop transport that reports the forwarder's address/port (rx disabled, PJSIP's own socket closed) and on_stream_created2 pausing the stream; Agent A forwards AMR frames to the veth leg instead of decoding to L16 (repacketising octet-aligned <-> bandwidth-efficient via ims/amr_rtp.rs when the legs differ). Every other call is transcoded exactly as today. The forward/transcode decision is made per call before media starts (Agent B's hook runs when the veth INVITE arrives, before on_incoming_call; Call-ID is available there). AMR only — no other codecs are forwarded. The forwarder must honour SDP direction (hold), remap payload types per leg (codec and telephone-event), and handle RTCP. Evidence and constraints are in specs/111-amr-passthrough-spike/findings-phase1.md and findings-phase2.md. Stock pjproject build, no pjsip patches; the circuit-switched GSM path is untouched."

## Background

Today, on both the VoWiFi and VoLTE paths, a call to an AMR-capable client (spec 085) is decoded and re-encoded twice:

```
carrier ──AMR──▶ carrier-side agent ──16 kHz PCM──▶ telephone-side agent ──AMR──▶ client
                 (decode)                          (re-encode)
```

That costs audio quality (two lossy codec passes), CPU on every call (it matters most on the Pi), and the delay of the telephone-side agent's jitter buffer and mixing tick.

Spike #111 established two things:
- Carrying the carrier's AMR frames through the bridge **inside the SIP stack's media engine** is not viable without patching it (`specs/111-amr-passthrough-spike/findings-phase1.md`).
- Keeping the SIP stack for **signalling only** while the bridge relays the audio itself works on today's unmodified build (`findings-phase2.md`). In the lab, frames arrived bit-identical, DTMF worked, hold/resume worked, and the telephone-side agent spent about 80% less CPU, adding about 30 µs per packet.

This feature puts that into the product, for AMR only. Every other call keeps today's transcoding path unchanged.

## Clarifications

### Session 2026-10-22

- Q: Is outbound forwarding part of this feature's first release? → A: No. This feature is inbound only (carrier → client). Outbound calls stay transcoded, and outbound forwarding is a follow-up once inbound is proven live.
- Q: Is forwarding on by default? → A: Off by default. Operators opt in with the setting; an upgrade with an unchanged configuration changes nothing.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Inbound cellular call reaches an AMR client without being transcoded (Priority: P1)

An operator runs the bridge with an AMR-capable softphone (e.g. baresip) as the ring target, either through the built-in registrar or through a PBX that passes AMR through. A call arrives from the carrier in AMR-WB. The softphone rings and the user answers. The carrier's AMR-WB frames reach the softphone exactly as the carrier sent them, and the softphone's frames reach the carrier the same way, with no decoding or re-encoding inside the bridge.

**Why this priority**: This is the request in #109. It removes the double transcode, which brings a quality gain, a CPU saving and a latency saving on every such call.

**Independent Test**: Place an inbound VoWiFi call to a line whose ring target is an AMR-WB softphone. Capture the carrier-side and client-side media. Confirm that the speech frames on the client side are bit-identical to those on the carrier side in both directions, that audio is intelligible both ways for the whole call, and that the call is reported as forwarded.

**Acceptance Scenarios**:

1. **Given** the carrier offers AMR-WB and the client accepts AMR-WB with compatible parameters, **When** an inbound VoWiFi call is answered, **Then** speech frames pass through the bridge unaltered in both directions and two-way audio works for the whole call.
2. **Given** the same setup over VoLTE, **When** an inbound VoLTE call is answered, **Then** the result is the same.
3. **Given** the carrier's leg uses AMR-NB and the client accepts AMR-NB with compatible parameters, **When** the call is answered, **Then** it is forwarded the same way.
4. **Given** the carrier sends octet-aligned AMR and the client negotiated bandwidth-efficient (or the reverse), **When** the call is answered, **Then** the bridge converts the packing without decoding, and the speech bits are unaltered.
5. **Given** a forwarded call, **When** either party presses keypad digits, **Then** the other side receives them, as on a transcoded call.
6. **Given** a forwarded call, **When** the client puts the call on hold and resumes it, **Then** no audio is sent towards the held party while on hold, and two-way audio returns on resume.

---

### User Story 2 - Every other call behaves exactly as today (Priority: P1)

An operator whose ring target does not take AMR (stock Asterisk, a G.722 desk phone), or whose carrier leg is not AMR, upgrades the bridge. Every call is transcoded exactly as before: same codecs on every leg, same audio path, same behaviour. A call where AMR is negotiated on both sides, but with incompatible parameters, is also transcoded, and it connects normally.

**Why this priority**: Most deployments sit behind a PBX with no AMR. Any regression on those calls outweighs the benefit of forwarding. And the decision to forward can never be allowed to fail a call.

**Independent Test**: Run the existing regression calls with an external PBX and with the built-in registrar, inbound and outbound, against far ends without AMR, before and after the upgrade. Compare the negotiated codecs per leg, two-way audio and call outcome; all must match. Then repeat with a client that offers AMR but with an incompatible mode set, and confirm the call connects, transcoded, with two-way audio.

**Acceptance Scenarios**:

1. **Given** a ring target that does not support AMR, **When** inbound and outbound calls are placed, **Then** every leg negotiates the same codec as before the upgrade and the audio path is the transcoded one.
2. **Given** a client that supports AMR-WB but restricts it to modes the carrier leg may exceed, **When** a call is set up, **Then** the call connects transcoded, with two-way audio, and the logs say why it was not forwarded.
3. **Given** the carrier leg is AMR-WB and the client only accepts AMR-NB (or the reverse), **When** a call is set up, **Then** it is transcoded as today.
4. **Given** any configuration, **When** a call goes over the circuit-switched GSM path, **Then** its behaviour is unchanged.

---

### User Story 3 - Outbound calls are unaffected (Priority: P2)

A softphone registered to the bridge dials out through the SIM, offering AMR-WB. The call is transcoded exactly as today, even with forwarding turned on. Outbound forwarding is deferred to a follow-up feature (see Clarifications).

**Why this priority**: Outbound has less room for the decision, because the client's offer arrives before the carrier's answer is known. Keeping it out of the first release lets the inbound design be proven live first. This story makes sure the deferral is explicit and tested rather than accidental.

**Independent Test**: With forwarding on, dial out from an AMR-WB softphone. Confirm the call connects with two-way audio on today's transcoded path, and that it is reported as transcoded because outbound forwarding is not supported.

**Acceptance Scenarios**:

1. **Given** forwarding is on and a client offering AMR-WB, **When** the client dials out, **Then** the call connects transcoded, exactly as before this feature, and is recorded as transcoded with the reason "outbound".

---

### User Story 4 - Operator can see and control which calls are forwarded (Priority: P2)

An operator turns forwarding on with one setting; it is off by default. They can tell, per call, whether it was forwarded or transcoded, and why it was not forwarded. Turning the setting off and restarting returns every call to today's path.

**Why this priority**: Forwarding changes the media path of live carrier calls. Operators need to confirm it is working and need a kill switch when diagnosing a carrier or client problem.

**Independent Test**: With an unchanged configuration, place a qualifying inbound call and confirm it is transcoded with the reason "forwarding disabled". Turn the setting on, restart, place a qualifying call and a non-qualifying one, and confirm each call's record and log show the path taken, with the reason. Turn the setting off again, restart, and confirm the qualifying call is transcoded.

**Acceptance Scenarios**:

1. **Given** an unchanged configuration (setting absent), **When** a qualifying inbound call is placed, **Then** it is transcoded exactly as today.
2. **Given** a call that was forwarded, **When** the operator inspects that call's log or call record, **Then** it says forwarded, with the AMR flavour and the packing on each leg.
3. **Given** a call that was not forwarded although one side offered AMR, **When** the operator inspects it, **Then** it says transcoded, with the reason (e.g. AMR flavour differs, mode sets incompatible, client did not accept AMR, forwarding disabled, outbound).
4. **Given** forwarding is turned off, **When** any call is placed, **Then** it is transcoded exactly as today.

---

### Edge Cases

- **The client renegotiates mid-call** (re-INVITE or UPDATE):
  - If AMR stays compatible, forwarding continues, including after a change of the client's address or port.
  - If the client asks for something forwarding can't carry (e.g. switches to G.722), the bridge declines that renegotiation and the call continues on the existing forwarded media. It must not drop to one-way audio or silence.
- **The carrier renegotiates mid-call** (e.g. session refresh or IMS precondition updates): forwarding continues with the carrier's renegotiated address and payload types, under the same rules.
- **Payload types differ between legs** (e.g. the carrier uses 104 for AMR-WB and the client 96; telephone-event types also differ): each packet is relabelled for the leg it is sent on.
- **Silence and comfort-noise frames** (SID, NO_DATA, DTX gaps) from either side pass through as they are. They don't cause gaps, clicks, teardown or a false "no audio" alarm.
- **Codec mode requests** (CMR), where one end asks the other to change bitrate: these pass through end to end, so rate adaptation works between the carrier and the client.
- **A burst of carrier-side jitter or packet loss**: the bridge no longer smooths it on a forwarded call. The client's own jitter buffer absorbs it, and the bridge doesn't add or drop packets.
- **One-way-audio and media-health monitoring**: keeps working on forwarded calls (packets counted per direction), so an existing one-way-audio diagnosis still fires correctly.
- **Many simultaneous calls on multiple lines**: each call's forwarding is independent. A forwarded and a transcoded call can run side by side.
- **The client goes silent or unreachable mid-call**: the bridge behaves as it does today on a transcoded call (no new crash or hang). Call teardown releases the forwarding resources.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: On the VoWiFi and VoLTE bridges, a call MUST be forwarded when its carrier leg and its client leg have both negotiated AMR-WB, or both AMR-NB, with compatible parameters (FR-003). Forwarded means the speech frames cross the bridge without being decoded or re-encoded.
- **FR-002**: Every call that does not meet FR-001 MUST take today's transcoding path, unchanged. No codec other than AMR-WB and AMR-NB is ever forwarded.
- **FR-003**: Two AMR legs are **compatible** when all of these hold:
  - (a) both are the same flavour (both AMR-WB or both AMR-NB);
  - (b) every bitrate mode each side may send is one the other side accepts, i.e. each sender's allowed mode set is within the receiver's (an unrestricted mode set accepts all modes);
  - (c) neither side requires an AMR feature the other refused (redundancy, CRC, robust sorting, interleaving), and none of these are in use;
  - (d) the number of frames per packet each side sends is acceptable to the receiving side.

  The packing (octet-aligned or bandwidth-efficient) MAY differ between legs; the bridge converts it without decoding.
- **FR-004**: Whether a call is forwarded or transcoded MUST be decided per call, before media flows on either leg. It MUST NOT change during the call.
- **FR-005**: The forward-or-transcode decision MUST NOT cause a call to fail. A call that does not qualify connects transcoded, with the same setup outcome it would have had before this feature.
- **FR-006**: The SIP/SDP the bridge sends to the carrier MUST be unchanged by this feature: same codecs offered or answered, same capabilities, same parameters. Only the bridge's internal handling of the audio differs.
- **FR-007**: On a forwarded call, these MUST pass through unaltered:
  - the speech bits of every frame;
  - frame types, including SID and NO_DATA;
  - codec mode requests;
  - the timing of every frame.

  The only permitted changes are the per-leg relabelling of payload types (FR-008) and the packing conversion (FR-003).
- **FR-008**: On a forwarded call, packets MUST be relabelled with the payload type the receiving leg negotiated. This applies to both the audio codec and the keypad-event (DTMF) type.
- **FR-009**: Keypad digits (DTMF) MUST work in both directions on a forwarded call at least as well as on today's transcoded call. A call whose DTMF arrangement forwarding can't preserve MUST be transcoded instead.
- **FR-010**: On a forwarded call, the bridge MUST honour each leg's negotiated media direction. Nothing is sent towards a leg that is held or inactive, and sending resumes when it is made active again.
- **FR-011**: On a forwarded call, the bridge MUST follow mid-call renegotiations that keep AMR compatible, including a change of either remote's address or port, without interrupting audio. A renegotiation that would make the call incompatible MUST be declined, with the call continuing as before.
- **FR-012**: RTCP towards the carrier MUST behave as it does today on a transcoded call. The client leg MUST receive valid RTCP or none. It MUST never receive RTCP that misdescribes its own stream.
- **FR-013**: Media-health monitoring MUST work on forwarded calls as it does on transcoded calls: per-direction packet counts, and one-way-audio and no-audio detection and alerts.
- **FR-014**: Each call's log and call record MUST state whether it was forwarded or transcoded. A forwarded call MUST record the AMR flavour and each leg's packing. A call that was not forwarded although AMR was offered on both sides MUST record the reason.
- **FR-015**: Operators MUST be able to turn forwarding on or off with a single deployment-wide setting. It MUST be **off by default**: with the setting absent, every call is transcoded exactly as today. Invalid values MUST stop the bridge at startup, consistent with strict config parsing.
- **FR-016**: Forwarding applies only in the inbound direction (carrier → client). Outbound calls (client → carrier) MUST take today's transcoding path regardless of the setting, and are recorded with the reason "outbound".
- **FR-017**: Everything the bridge holds to forward a call (network ports, processing) MUST be released at the end of every call, including calls that fail during setup or are torn down abnormally.
- **FR-018**: The circuit-switched GSM path, the codecs negotiated on it, and its audio handling MUST be unchanged.
- **FR-019**: Documentation MUST explain:
  - when a call is forwarded and when it isn't;
  - the setting;
  - that a PBX in the path must itself pass AMR through (stock Asterisk does not, so the built-in registrar mode is the primary target);
  - how to tell from logs which path a call took.

  Release notes MUST state that forwarding is off by default, how to turn it on, and that it applies to inbound calls only.
- **FR-020**: Before merge, verification MUST include:
  - (a) automated tests covering the compatibility rule (FR-003), payload-type relabelling, packing conversion, direction handling, and the fallback to transcoding;
  - (b) live calls with an AMR-WB softphone on the Vodafone rig and on the Jio line, inbound, each confirming bit-identical frames, DTMF, and hold/resume, plus one outbound call confirming it stays transcoded;
  - (c) a regression pass showing unchanged behaviour with a non-AMR ring target.

### Key Entities

- **Media path decision**: per call, either *forwarded* or *transcoded*. It is fixed before media starts and recorded with its reason.
- **AMR leg parameters**: per leg, the negotiated AMR flavour, allowed mode set, packing, frames per packet, use of redundancy/CRC/robust sorting/interleaving, the audio payload type, and the keypad-event payload type. Compatibility (FR-003) is judged between the carrier leg's and the client leg's parameters.
- **Forwarding setting**: one deployment-wide on/off switch, off by default (FR-015).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: On forwarded calls, 100% of speech frames received by the client are bit-identical to those sent by the carrier, and the reverse, measured over a 5-minute inbound call.
- **SC-002**: With an unchanged configuration, or with no AMR-capable ring target, 100% of calls in the regression set have the same per-leg codecs, two-way audio and call outcome as before the upgrade. The set covers an external PBX and the built-in registrar, inbound and outbound, plus the GSM path.
- **SC-003**: 100% of calls that offer AMR but fail the compatibility rule still connect with two-way audio (no call lost to the forwarding decision).
- **SC-004**: The bridge's processing cost for a forwarded call is at least 70% lower than for the same call transcoded, measured on the Raspberry Pi deployment.
- **SC-005**: The delay the bridge adds to a forwarded call's audio is at least 20 ms lower than on the same call transcoded.
- **SC-006**: Forwarded calls on both the Vodafone and Jio lines sustain 5 minutes of intelligible two-way audio, with working DTMF and hold/resume, and no drop-outs attributable to the bridge.
- **SC-007**: An operator can tell from a single call's log entry whether it was forwarded and, if not, why, without reading anything beyond the documentation.

## Assumptions

- **Inbound only for now.** Outbound forwarding is a follow-up feature once inbound is proven live (Clarifications).
- **Scope is AMR-WB and AMR-NB only.** Other codecs are cheap to transcode, and forwarding them would mean moving the SIP stack's codecs, resampling and jitter handling into the bridge. EVS and other future codecs are out of scope.
- **Same build, no SIP-stack patches** (constraint from #111). The SIP stack is the same unmodified build as today, and forwarding uses only its public interfaces. No second image or binary is introduced.
- **The carrier leg's codec choice is unchanged.** The carrier-side agent still chooses the carrier codec at call precheck as today. This feature only decides what happens to the audio afterwards.
- **No smoothing on forwarded calls.** On a forwarded call the bridge no longer smooths network jitter; the client's own jitter buffer does. This is the normal behaviour of a media relay, and AMR clients (softphones) have jitter buffers.
- **The client leg's SSRC, sequence numbers and timestamps are the far end's own.** The bridge does not rewrite them, so the client sees one consistent stream for the whole call. Since the path never changes mid-call (FR-004), no stream discontinuity arises.
- **Ringback stays as today.** It is unaffected, because the bridge's own ringback tone was never heard on this path.
- **A PBX in the path must pass AMR through.** Stock Asterisk does not, so in practice forwarding applies to the built-in registrar mode, or to a PBX/media relay that passes AMR through (e.g. FreeSWITCH, rtpengine). Calls through a PBX that does not pass AMR through are simply transcoded.
- **Live verification** uses the Vodafone rig (local) and the Jio line (remote Pi), per the project's existing practice.
