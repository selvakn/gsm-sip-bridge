# Phase 1 Data Model: DTMF relabeling and SSRC visibility

No persistent storage. Two small additions, each local to the function it
extends. (SDP-06's `ptime` half added no data model — see `research.md`
Decision 4: it turned out to need a confirming test, not a new field.)

## `agent::veth::forward` (extended, pass-through relay)

| Parameter | Type | Role |
|---|---|---|
| `src_dtmf_pt` | `Option<u8>` (new) | The *sending* leg's own negotiated DTMF payload type — what a DTMF packet from `src` arrives labeled as |
| `dst_dtmf_pt` | `Option<u8>` (new) | The *receiving* leg's own negotiated DTMF payload type — what it must be relabeled to before forwarding |

Local state (not a struct — a plain local in the function body):

| Name | Type | Role |
|---|---|---|
| `last_ssrc` | `Option<u32>` | The most recently seen SSRC on this direction; `None` until the first packet |

## `transcode::relay_direction` (extended, transcoding relay)

Same `last_ssrc: Option<u32>` local addition as `forward` — RTP-04 only;
RTP-03 does not apply here (batch 1's RTP-02 already gives this path its
own correct per-leg DTMF payload-type handling via `dst_codec.dtmf_payload_type`/`RtpSender`).

## Relationship to the spec's Key Entities

| Spec term (`spec.md`) | Concrete type |
|---|---|
| Pass-through relay | `agent::veth::forward`/`relay_rtp` |
| DTMF payload type | `ChosenCodec::dtmf_payload_type` (existing, from batch 1), threaded into `forward` as `src_dtmf_pt`/`dst_dtmf_pt` |
| SSRC | `rtp::ParsedPacket::ssrc` (existing field, newly read) vs. each relay direction's own `last_ssrc` local |
| Packetization interval (`ptime`) | No new type — `build_answer_for`'s existing hardcoded `a=ptime:20`, confirmed correct as-is |

## Behavior, per packet (pass-through relay, both directions)

```
datagram received on `src`
        │
        ▼
  rtp::parse_packet ──None (malformed)──▶ forward raw bytes unchanged (unparsed today; unaffected)
        │ Some(pkt)
        ▼
  pkt.ssrc != last_ssrc (and last_ssrc was Some)? ──Yes──▶ log SSRC change; last_ssrc = pkt.ssrc
        │ (either way, continue)
        ▼
  pkt.payload_type == src_dtmf_pt AND dst_dtmf_pt is Some AND src_dtmf_pt != dst_dtmf_pt?
        │ Yes                                   │ No
        ▼                                        ▼
  rewrite payload-type byte in the buffer   forward buffer unchanged
        │                                        │
        └──────────────────┬─────────────────────┘
                            ▼
                    send to `dst`
```

The transcoding relay's equivalent diagram is unchanged except it has no
"forward raw bytes unchanged" branch (a packet that doesn't parse is
already dropped there today) and no DTMF-relabel step (already handled by
its own `RtpSender`/`dst_codec.dtmf_payload_type` machinery).
