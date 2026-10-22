# Research: AMR passthrough forwarder

Facts below come from reading the code at `96964fa` and from the #111 spike results. Items marked **UNPROVEN** are gated by spike task S1/S2 in tasks.md.

## R1. When the forward-or-transcode decision is made

**Finding**: the spec's sketch (decide in Agent B's hook "when the veth INVITE arrives") does not match the inbound call flow. In `vowifi/mod.rs::bridge_call` Agent B is the *caller* on both legs: it places the client/PBX call and the veth call back to back (`Call::make` ×2, then `pair_calls`) before either is answered. The client's SDP answer does not exist until a human picks up, which can be many seconds later. Agent A holds the carrier in 180 until Agent B reports `CallAnswered` (`await_pbx_answer`, `inbound.rs`). So at INVITE time only the carrier side is known.

**Decision**: every call starts on today's path, untouched. When the client answers, and **before** Agent B sends `CallAnswered` (so Agent A has not answered the carrier and no carrier media can flow), Agent B judges compatibility between the carrier's parameters (sent by Agent A in `IncomingCall`) and the client's negotiated parameters (read from the answered call). Only if compatible, Agent B **upgrades** the call. Otherwise nothing further happens: that call is the transcoded call that is already up.

**Rationale**:
- A non-qualifying call runs exactly today's code with no extra step. That is US2/SC-002 by construction, and FR-005 (the decision cannot fail a call) needs no fallback transport.
- The window between client answer and `CallAnswered` is real and already exists, so FR-004 holds: decided once, before media flows, never changed.
- Only qualifying calls pay for the extra round trips (one control exchange with Agent A, one re-INVITE to the client), roughly 100 ms before the carrier is answered.

**Alternatives rejected**:
- *Optimistic forwarder transport at offer time, fall back on an incompatible answer.* Needs a way to put a real media transport back under the client leg after the fact, i.e. the same re-INVITE machinery but on the path every non-qualifying call would take. It risks the majority case to help the minority.
- *Defer placing the veth call until the client answers.* Changes the A↔B handshake for every call (`BridgeReady` ordering, `VETH_INVITE_TIMEOUT`), and still does not solve the client leg's transport.
- *Decide from the client's REGISTER or an OPTIONS probe.* Neither carries AMR parameters reliably.
- *Offer only AMR to the client.* Breaks every non-AMR client (FR-002).

Outbound is never upgraded (FR-016): `run_outbound_listener` is not touched; the call record says `transcoded:outbound`.

## R2. Putting the forwarder under the client leg (UNPROVEN: spike S1)

**Decision**: use the phase-2 mechanism, applied by re-INVITE:
- `on_create_media_transport(call_id, mi, base_tp)`: if the per-call registry holds a forward entry for `call_id` and `mi == 0`, create `pjmedia_transport_loop_create2` with the forwarder's address/port and `disable_rx = 1`, close `base_tp`, return the loop transport. Otherwise return `base_tp` (today's path, the case for every call that is not being upgraded).
- `on_stream_created2`: if the registry holds an entry for the call, `pjmedia_stream_pause(stream, ENCODING_DECODING)`. It fires on every re-created stream, so the pause survives hold/resume (phase-2 finding 6).
- The upgrade itself is `pjsua_call_reinvite2` with `PJSUA_CALL_REINIT_MEDIA`, which closes and re-creates the media transports, calling the hook again.

**Why unproven**: the phase-2 spike installed the loop transport when the call was *created*. It never re-created it on a live call. Spike S1 must show on stock 2.16, with the production call settings (`aud_cnt=1, vid_cnt=0, txt_cnt=0`): the re-INVITE carries `c=`/`m=` of the forwarder; the old base transport's socket is released; the stream comes up paused; hold/resume still works afterwards; teardown leaves no leaked port. If `REINIT_MEDIA` cannot do this, the fallback is to place only the *qualifying-looking* calls (carrier AMR, flag on, client registered in the built-in registrar) with the loop transport from the start and accept a re-INVITE-based downgrade; that is a redesign and S1 failing blocks implementation.

**Rejected**: a custom `pjmedia_transport` vtable in Rust (large unsafe surface, reimplements what the loop transport gives); patching pjsip (forbidden).

## R3. Telling Agent A, and the veth leg's role (UNPROVEN: spike S2)

**Decision**: signalling for the A↔B hop goes over the existing control channel, not a veth SIP re-INVITE. New messages (contracts/control-protocol.md):
- `IncomingCall` gains `carrier_amr: Option<AmrLegParams>` (`serde(default)`), set by Agent A when the setting is on and the chosen carrier codec is AMR.
- `ForwardMedia { call_id, rtp_port, client: AmrLegParams }` (B→A): "forward to `<veth peer address>:rtp_port`, the client side uses these parameters".
- `ForwardAck { call_id, ok }` (A→B) and `ForwardCancel { call_id }` (B→A).

Agent B keeps its veth PJSIP call exactly as today (real transport, it stays the dialog both agents already watch for teardown) but **unpairs it from the conference** when upgrading, so it carries no media. Agent A, on `ForwardMedia`, binds its end, connects it to B's forwarder socket, and on `CallAnswered` spawns the forwarding relay (carrier ↔ that socket) instead of `spawn_transcoding_relay`.

**Rationale**: the veth UAS in Agent A answers one INVITE datagram and never sees another (`spawn_veth_uas_listener`); making it handle re-INVITEs would be new SIP code in the more fragile agent. The control channel already carries per-call state in both directions and already tolerates unknown peers via `serde(default)`.

**Order of the upgrade** (B): bind forwarder sockets → `ForwardMedia` → wait `ForwardAck` (timeout 2 s) → re-INVITE the client onto the forwarder → wait for the client's 200 and the media-state callback → read the client's *post-reinvite* parameters once more and re-check compatibility → send `CallAnswered`. Any failure before `CallAnswered` → `ForwardCancel`, close the forwarder, and (if the re-INVITE was already accepted) re-INVITE back with `REINIT_MEDIA` and no forward entry, i.e. the transcoded path; if even that fails the call fails like any other setup failure. This is the only path where FR-005 could bite; S1 measures how often the client rejects the re-INVITE.

**Spike S2**: confirm that unpairing the veth call from the conference leaves it quiet (no packets sent to Agent A) and that Agent A's veth UAS tolerates a call that sends and receives nothing for the whole call.

## R4. The compatibility rule and where each side's parameters come from

`AmrLegParams` (data-model.md) holds flavour, packing, mode-set (bitmask over modes 0..7 NB / 0..8 WB; absent = all), frames per packet (from `ptime`/`maxptime`), the AMR feature flags (`crc`, `robust-sorting`, `interleaving`, `mode-change-period/neighbor`), the audio payload type and the telephone-event payload type.

- **Carrier side**: parsed from the carrier offer in `ims/sdp.rs` (it already reads `octet-align`, `maxptime`, payload types; this adds `mode-set` and the feature flags to what `ChosenCodec` carries).
- **Client side**: read after the client's answer. Agent B already reads `pjsua_call_get_stream_info` for the codec name (`negotiated_codec`). The same call returns the negotiated `enc_fmtp`/`dec_fmtp` and payload types for the stream; `pjsua_call_get_info().media[0].dir` gives the direction. Parsed by the same `params.rs` code as the carrier side, so there is one parser for both.

`compatible(carrier, client) -> Result<(), Reason>` implements FR-003 literally: (a) same flavour; (b) `carrier.send_modes ⊆ client.accept_modes` and `client.send_modes ⊆ carrier.accept_modes`; (c) neither side requires a feature the other refuses, and none in use; (d) frames per packet each side sends ≤ what the other accepts. SDP `mode-set` constrains what the *receiver of that fmtp* accepts, so each leg contributes one set used as both its accept set and (as the answerer's choice) its send set. `Reason` is an enum with a stable `as_str()` for logs (contracts/logs.md).

This is a pure function and the heart of the test suite (FR-020a).

## R5. Packing conversion without decoding

`ims/amr_rtp.rs` today converts to and from the codec library's MIME frame layout, and deliberately *drops* SID/NO_DATA frames and writes a fixed CMR of 15. Forwarding needs the opposite: every frame type kept, the CMR preserved, several frames per packet supported.

**Decision**: new `amr_forward/repack.rs` with `repack(payload, kind, from_octet_aligned, to_octet_aligned) -> Option<Vec<u8>>`, built on the existing `BitReader`/`BitWriter`/`speech_bits` (exposed `pub(crate)`). Same-packing legs skip it entirely (byte copy plus payload-type relabel). Property test: `repack(repack(x, a→b), b→a) == x` for every frame type and 1–3 frames per packet; and a cross-check that the speech bits of each frame are identical through the existing `payload_to_frame`.

CRC, robust-sorting and interleaving are never repacked: they fail compatibility rule (c), so those calls transcode.

## R6. The relay (shared by Agent A and Agent B)

`amr_forward/relay.rs` generalises what `ims/agent/veth.rs::forward` already does for PCMU, rather than copying it:
- **Payload-type relabel** for the audio type and the telephone-event type, per leg (existing `relabel_dtmf_payload_type` logic, plus the audio type).
- **Repack** per direction, only when the legs' packing differs.
- **Direction gate**: an `AtomicU8` per direction. Packets towards a held/inactive leg are dropped at the relay and not counted as sent. Agent B updates it from `on_call_media_state` (client's `media[0].dir`); the carrier side is unchanged, because Agent A declines carrier re-INVITEs today (`agent/mod.rs`, "in-call session modification is not supported"), so the spec's carrier-renegotiation edge case needs no new code and is recorded as an inherited limit.
- **Remote address** for the client leg is learned from the stream info at each `on_call_media_state` (follows a client address/port change on re-INVITE, FR-011). No symmetric-RTP latching (YAGNI; a LAN or registrar-registered softphone).
- **Counters**: the same `MediaMeter` counters Agent A already keeps (`carrier_rx_counter`, `pbx_rx_counter`), bumped on receive, so one-way-audio verdicts (FR-013) work unchanged. Agent B's forwarder keeps its own per-direction counters for the log line.
- **SSRC changes** keep using `SsrcTracker`; never a reason to drop.
- **Threads**: two per direction pair, polling with the existing 200 ms read timeout and a `stop` flag, exactly the shape of `relay_rtp`. Torn down with the call.

Agent A uses it between its carrier socket and the veth forwarding socket, with the existing `CarrierRtcpBundle` roles hooked in as today. The tracker needs the codec clock rate: 16000 for WB, 8000 for NB (the PCMU constant in `forward` is replaced by a parameter).

## R7. RTCP

- Carrier side: unchanged. Agent A keeps its report loop; the relay feeds the same tracker/accumulator roles, so SR/RR describe what A really sends and receives (FR-012, first sentence).
- A↔B hop: none.
- Client side: the loop transport advertises RTP and RTCP ports; Agent B **discards** RTCP arriving there and sends none. "Valid RTCP or none" is satisfied with none, and no report can misdescribe the stream (FR-012, second sentence). Synthesising reports is deferred until a client is seen to need them.

## R8. Observability (FR-013, FR-014)

- One INFO line per call from Agent B: `media path decided` with `call_id`, `path=forwarded|transcoded`, `reason` (stable enum string; absent when forwarded), `flavour`, `carrier_packing`, `client_packing`. Agent A logs the matching `carrier_amr`/`forwarding` fields in its existing `call answered and bridged` line (it already prints `transcoding`).
- `CallRecord` (control protocol) gains `media_path: Option<String>` (`serde(default)`), e.g. `forwarded:amr-wb:oa/be` or `transcoded:flavour_differs`. `vowifi-status` already prints recent calls and shows it.
- Reasons: `disabled`, `outbound`, `carrier_not_amr`, `client_not_amr`, `flavour_differs`, `mode_set`, `feature`, `ptime`, `dtmf`, `ack_timeout`, `reinvite_failed`.

## R9. Config

`[vowifi].amr_passthrough` (bool, default `false`), beside `wideband`. Global, takes effect on container recreate, exported as `AMR_PASSTHROUGH` in the shell env like `IDLE_KEEPALIVE`. VoLTE: `volte/carrier_agent.rs` builds its own agent parameters (it hard-codes `wideband: true`); it gets the value from the same section so one setting covers both transports (FR-015, "single deployment-wide setting"). Invalid values already fail at startup under strict parsing.

## R10. DTMF (FR-009)

RFC 4733 events are relabelled between the two legs' telephone-event types as `relay_rtp` already does. A call is **not** upgraded (`reason=dtmf`) when one leg negotiated a telephone-event type and the other did not, because forwarding would then send events a leg never agreed to receive, or lose events the other leg expects (today the transcoding path synthesises or drops them). In-band-only DTMF over AMR is never detected by a relay either way, so it is the same as today's PCMU relay.

## R11. Mid-call client renegotiation (FR-011)

- Compatible re-INVITE/UPDATE (hold, resume, address change): handled by pjsua; `on_stream_created2` re-pauses, `on_call_media_state` refreshes remote address, direction and payload types in the forwarder.
- Incompatible (e.g. switch to G.722): `on_call_rx_reinvite` (public pjsua callback) inspects the offer for a forwarded call and sets the response to 488. **UNPROVEN: S1 also covers this.** The call continues on the existing forwarded media.

## R12. Teardown and leak safety (FR-017)

A `ForwardSession` owns the forwarder sockets, the `stop` flag and the relay thread handles; `Drop` sets `stop`, closes the sockets and joins. It is created before the first side effect of the upgrade and dropped on every exit path, including failures before `CallAnswered` and abnormal teardown (client BYE, carrier CANCEL, control-channel drop). The per-call registry entry in `pjsua-safe` is removed in `on_call_state` when the call is `DISCONNECTED`, so a reused PJSIP call id never inherits a forward entry. Agent A's forwarding socket and relay are owned by the same `stop` flag it already uses.

## R13. Testing layers (Constitution I)

1. Pure: compatibility matrix (every FR-003 clause, both directions), repack round-trip, payload-type relabel, config default/invalid.
2. Real sockets on loopback: relay forwards bit-identically both directions, relabels, repacks, honours the direction gate, survives a source address change, stops and releases ports.
3. Real PJSIP in the privileged container (`test/`): spike harness promoted to a test; hook returns the forwarder on upgrade and base transport otherwise; paused stream sends 0/0; hold/resume; incompatible re-INVITE → 488; no port leak after hangup.
4. Control protocol: round-trip and old-peer parsing, in the style of the existing `control.rs` tests.
5. Live (FR-020b/c): Vodafone rig and Jio Pi inbound with an AMR-WB softphone: pcap bit-compare, DTMF, hold/resume, 5-minute soak, one outbound stays transcoded, regression with a non-AMR target, CPU and delay measured for SC-004/005.
