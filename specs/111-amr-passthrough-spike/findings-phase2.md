# Spike #111, phase 2: PJSIP signalling only (option 2a's hook)

**Date**: 2026-10-22 · **pjproject**: 2.16 stock build, exactly today's `docker/pjsip-config-site.h`

**Question**: can Agent B keep PJSIP for SIP and SDP, but carry a call's audio on its own sockets? PJSIP must advertise the forwarder in its SDP and must not send, receive or decode anything itself. Public API only, no pjsip patch.

## Verdict

**Go.** The hook needs no SDP rewriting and no pjsip change: two public callbacks and three public calls.

```
on_create_media_transport(call, mi, base_tp):          // per call, per media line
    bind forwarder RTP/RTCP sockets (even port, port+1)
    pjmedia_transport_loop_create2(endpt,
        {addr = Agent B's address, port = forwarder RTP port, disable_rx = 1}, &tp)
    pjmedia_transport_close(base_tp)                    // PJSIP's own socket, never used
    return tp                                           // PJSIP builds c=/m= from tp's info

on_stream_created2(call, {stream}):                     // fires on every (re-)created stream
    pjmedia_stream_pause(stream, PJMEDIA_DIR_ENCODING_DECODING)

on_call_media_state(call):                              // also after re-INVITE / UPDATE
    learn remote RTP/RTCP + tx/rx payload types from pjsua_call_get_stream_info()
    (Agent B makes no conference connection for a forwarded pair)
```

The loop transport (`pjmedia/transport_loop.c`) is a public pjmedia transport. It reports whatever address and port it is configured with, and it has its own group lock (`transport_loop.c:186-191`). So closing `base_tp` inside the callback is safe, even though pjsua otherwise borrows the base transport's lock (`pjsua_media.c:2294-2318`).

## Lab setup

The topology is the same as phase 1. X (veth stand-in) calls B and B calls Y (client stand-in). Both are stock pjsua, restricted to **AMR-WB**. B is `harness/b2.c`.
- X sends RFC 2833 DTMF `123`, puts the call on hold (re-INVITE), releases it (re-INVITE) and hangs up.
- Y auto-answers and plays 8 s of synthetic speech.
- Output is in `results/p2-*.txt`. Reproduce with `harness/run.sh`.

## Findings

**1. The SDP is built around the forwarder, on both legs and on every re-offer** (`results/p2-fwd.txt`). The inbound answer, the outbound offer and both re-INVITE answers carried `c=<B> m=audio <forwarder port>`. `on_call_sdp_created` was used only to observe them.

**2. PJSIP's sockets carry nothing.** Every UDP packet to or from X or Y went through the forwarder's four ports (`flows.py`). With the stream paused, PJSIP's own stream counters read **sent 0 / received 0** on both legs.

**3. The relay is exact.** In both directions:
- **563/563 packets** were identical in sequence number, timestamp, SSRC and payload (`bytecmp.py`);
- AMR-WB frames were bit-identical, including the client's CMR and mode changes (FT 2–7);
- the payload type was rewritten per leg (96 ↔ 101).

The baseline (today's conference-bridge pairing) shows 0 identical packets, i.e. re-encoded.

**4. DTMF works.** RFC 2833 events were remapped (PT 120 ↔ 120 here, generally per leg). Y logged `Incoming DTMF 1, 2, 3`.

**5. RTCP:** forwarded as-is on port+1 (SR/RR from both ends). Whether to forward or terminate per leg is a design choice for the spec.

**6. Re-INVITE (hold/unhold):** works.
- pjsua recreated the stream for each SDP change, and `on_stream_created2` fired each time, so the pause is re-applied automatically.
- `on_call_media_state` fired with HOLD and then ACTIVE, which is where the forwarder re-learns the remote address and payload types.
- One behaviour difference: **the forwarder kept sending to X while X was on hold.** The baseline stopped (465 vs 563 packets). The forwarder must honour the negotiated direction (`sendonly`/`inactive`).

**7. Pausing the stream is necessary.**
- Without `pjmedia_stream_pause`, the conference bridge still ticks the unconnected stream. PJSIP encoded 404 AMR-WB frames into the loop transport, and CPU fell only from 369 ms to 341 ms.
- With the pause, the call cost **73 ms**.

**8. Overhead, measured over the same ~8 s call window** (`results/p2-cpu.txt`, `p2-residence.txt`):

| Agent B | CPU (call window) | Per-packet time in B |
|---|---|---|
| Today: conference bridge, opencore AMR-WB decode + encode, both directions | 369 ms | jitter buffer + 20 ms conference tick |
| Signalling-only + forwarder + stream pause | **73 ms (−80 %)** | **23–35 µs median, p99 ≈ 0.1 ms** |

These are x86 numbers. The ratio should hold or improve on the Pi, where opencore costs relatively more. The remaining 73 ms is the null-device clock ticking the conference bridge, plus SIP and the forwarder. Removing the paused ports from the bridge could cut it further; that wasn't tried.

**9. Choosing forward or transcode per call is possible, but must happen at transport creation.**
- The hook runs *before* `on_incoming_call` for an inbound call. It gets only `call_id`, but `pjsua_call_get_info()` already works there: it reports role, **Call-ID** and remote URI on both the UAS and UAC legs (`results/p2-hook-context.txt`). Returning `base_tp` keeps today's path for that call.
- `ci.media[mi].type` isn't populated yet, so the hook must go by media index. Index 0 is audio given production's call setting (`aud_cnt=1, vid_cnt=0, txt_cnt=0`, `pjsua-safe/src/call.rs:101-103`). The lab first ran with pjsua's default `txt_cnt=1`, which added an `m=text` line on a real socket. That came from my harness, not from production.
- For the inbound leg, Agent B needs the forward/transcode decision before the hook runs. Two options, **neither tested**:
  - Agent A flags it on the veth INVITE in a header, and Agent B reads it with a higher-priority PJSIP module (`pjsip_endpt_register_module`, public), keyed by Call-ID;
  - or Agent B makes the decision itself from the offer it can read in that module.
- For the outbound leg, the app already knows when it calls `pjsua_call_make_call`.

## What this means for option 2a

- **Unsafe FFI surface added to `pjsua-safe`:**
  - two callbacks: `on_create_media_transport` and `on_stream_created2`;
  - three calls: `pjmedia_transport_loop_create2`, `pjmedia_transport_close`, `pjmedia_stream_pause`;
  - the stream-info getter, already used today.

  No SDP structures are touched. That is smaller than the `on_call_sdp_created` SDP editing spec 085 R4 rejected.
- **Build:** unchanged. Stock pjproject, one image. The GSM path and today's transcode fallback are untouched, and the fallback stays per call: the hook returns `base_tp`.
- **The forwarder itself** is `relay_rtp` (`ims/agent/veth.rs:171`) plus:
  - payload-type mapping per leg, for both the codec and telephone-event (relay_rtp already maps DTMF);
  - honouring SDP direction (finding 6);
  - `ims/amr_rtp.rs` repacking when the two legs negotiate different AMR packet layouts (octet-aligned on one, bandwidth-efficient on the other). The lab didn't need this, because both legs agreed.
- **Still open, and the same as for any passthrough design:**
  - end-to-end codec and mode-set negotiation (Agent A commits at precheck; outbound has no slack);
  - the SSRC/seq/ts of the far end now reach the client unchanged. That is fine for one source per call, but a mid-call switch of source (e.g. a fallback) would need rewriting;
  - the PBX must pass AMR through (built-in `[sip_server]` or FreeSWITCH/rtpengine; stock Asterisk doesn't).
