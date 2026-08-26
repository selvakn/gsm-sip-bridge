# Phase 1 Data Model: SDP offer/answer identity for negotiation honesty

This feature adds no persistent storage. It extends the one struct that
already represents a parsed offer (`SdpOffer`, `src/ims/sdp.rs:282-307`)
and adds two small, purely-data types alongside it.

## SdpOffer (existing entity, extended)

| Field | Type | Role |
|---|---|---|
| `remote_rtp` | `SocketAddr` | Unchanged — the selected (first) audio section's connection address/port |
| `offered` | `Vec<OfferedCodec>` | Unchanged — codecs from the selected audio section |
| `dtmf` | `Vec<(u8, u32)>` | Unchanged |
| `maxptime` | `Option<u32>` | Unchanged |
| **`direction`** | `MediaDirection` (new) | The selected audio section's own stated direction; `SendRecv` if absent, matching today's implicit behavior |
| **`proto`** | `String` (new) | The selected audio section's raw `m=` transport token (e.g. `"RTP/AVP"`), captured but not validated by `parse_offer` itself — see Decision 4 |
| **`other_media`** | `Vec<DeclinedMedia>` (new) | Every `m=` section in the offer other than the selected audio one, in original order |

## MediaDirection (new entity — classification, not stored state)

```
enum MediaDirection { SendRecv, SendOnly, RecvOnly, Inactive }
```

Parsed from the selected audio section's `a=sendonly`/`a=recvonly`/
`a=inactive`/`a=sendrecv` line (default `SendRecv` if none present).
`build_answer_for` maps it to the answer's own direction attribute per RFC
3264 §6.1:

| Offer stated | Answer states |
|---|---|
| `SendOnly` | `RecvOnly` |
| `RecvOnly` | `SendOnly` |
| `Inactive` | `Inactive` |
| `SendRecv` (or absent) | `SendRecv` |

## DeclinedMedia (new entity)

What's needed to emit an RFC 3264 §6 declined-stream line (`m=<kind> 0
<proto> <fmts>`) for one `m=` section the offer included that this bridge
will not negotiate.

| Field | Type | Notes |
|---|---|---|
| `kind` | `String` | The media type word from the offer's `m=` line (`"video"`, `"text"`, `"audio"` for a duplicate audio section, etc.) — echoed verbatim |
| `proto` | `String` | That section's own transport token — echoed verbatim; a declined section's protocol is not this bridge's concern |
| `fmts` | `String` | That section's format-list, echoed verbatim (the port being `0` is what marks it declined, not the format list) |
| `before_audio` | `bool` | Whether this section appeared before the selected audio section in the offer, so the answer can reproduce the same relative order |

Constructed once per non-selected `m=` section, during the same single pass
`parse_offer` already makes over the offer body — no second pass, no
lookahead.

## Relationship to the spec's Key Entities

| Spec term (`spec.md`) | Concrete type |
|---|---|
| Media section | `OfferedCodec` (the selected one) / `DeclinedMedia` (every other one) |
| Direction | `MediaDirection` |
| Transport profile | `SdpOffer::proto` (raw), checked against the literal `"RTP/AVP"` in `handle_invite` |

## State transition (conceptual — computed once per inbound INVITE)

```
inbound INVITE arrives, offer parsed
        │
        ▼
  offer.proto == "RTP/AVP"? ──No──▶ decline: 488, Warning 305 (Decision 4)
        │ Yes
        ▼
  select_codec_with(offer, ...) finds a usable codec? ──No──▶ decline: 488, Warning 304 (existing, MT-07)
        │ Yes
        ▼
  build_answer_for: negotiated m=audio line (mirrored direction)
                  + one declined m=<kind> 0 <proto> <fmts> line per
                    offer.other_media entry, in original order
```
