# Phase 1 Data Model: RTCP reporting on the carrier media leg

Entities from `spec.md`'s Key Entities, resolved to concrete shapes against
the current source. Everything here is per-call state on the carrier leg;
nothing persists beyond a call except the metrics derived from it.

## `SendAccounting` — what we have transmitted

New. Shared between whichever relay direction sends toward the carrier and
the RTCP thread. Lock-free, mirroring `media_stats::MediaMeter`'s existing
`Arc<AtomicU64>` design (research Decision 4).

| Field | Type | Written by | Purpose |
| --- | --- | --- | --- |
| `packets` | `AtomicU64` | carrier-bound relay direction | SR sender's packet count |
| `octets` | `AtomicU64` | carrier-bound relay direction | SR sender's octet count — **payload bytes, excluding the RTP header** (RFC 3550 §6.4.1) |
| `ssrc` | `AtomicU32` | carrier-bound relay direction | The identity reports are sent under (FR-002) |
| `ssrc_known` | `AtomicBool` | carrier-bound relay direction | Distinguishes "SSRC is 0" from "nothing sent yet" — `0` is a legal SSRC |
| `last_rtp_timestamp` | `AtomicU32` | carrier-bound relay direction | Anchor for the SR's RTP/NTP timestamp pair |
| `last_timestamp_at` | `Mutex<Option<Instant>>` | carrier-bound relay direction | When that timestamp was observed, so the SR can extrapolate to now |

**Validation rules**

- `packets` and `octets` are monotonically non-decreasing (FR-003). Nothing
  resets them, including on an SSRC change (research Decision 5).
- `ssrc` is published on every send on the pass-through path (it can change
  — FR-002a) and once on first send on the transcoding path (it cannot).
- Until `ssrc_known` is true, no sender report is sent: there is no identity
  to send one under. A leg that has sent nothing at all reports via the
  receiver-report path instead (FR-005).

**Filled differently per relay path** (FR-002b):

| Path | `ssrc` source | Site |
| --- | --- | --- |
| Transcoding | `RtpSender`'s own minted value | `transcode.rs:298` mints it; publish on first send |
| Pass-through | Observed on the forwarded packet | `veth::forward`, where `rtp::SsrcTracker` already parses it |

## `ReceiveQuality` — what we observed

**Reused as-is.** `media_stats::ReceiveTracker` behind an
`Arc<Mutex<...>>`, exactly as `ims/call.rs:445` already wraps it (research
Decision 6). No new type, no new fields.

Already provides everything needed: `received_packets`, `lost_packets`,
`reordered_packets` and RFC 3550 §6.4.1 `jitter`, with sequence-wraparound
handling. Fed by the carrier→veth direction on each received packet; read
by the RTCP thread per report and at teardown.

Its existing `ReceiveStats` snapshot type is what reaches both the report
and the end-of-call reporting.

## `FarEndQuality` — what the far end told us

New. Written by the RTCP thread as reports arrive, read at teardown.

| Field | Type | Purpose |
| --- | --- | --- |
| `reports_received` | `u64` | Zero distinguishes "never reported" from "reported zero loss" (FR-009) |
| `fraction_lost` | `Option<f64>` | Most recent — the far end's loss on what we sent |
| `cumulative_lost` | `Option<u64>` | Most recent |
| `jitter` | `Option<Duration>` | Most recent, converted from RTP timestamp units |
| `round_trip` | `Option<Duration>` | Derived (below); `None` when not yet derivable |

**Validation rules**

- Every `Option` stays `None` until a well-formed report supplies it. No
  field is ever defaulted to zero — FR-009 requires the absence to remain
  visible.
- Discarded packets (wrong source IP, malformed, unrecognised type) never
  write here (FR-010/010a).

**Round-trip derivation** (FR-007): `RTT = now − LSR − DLSR`, where `LSR`
and `DLSR` come from the receiver block and `now` is in the same NTP
middle-32 form. Requires having sent at least one SR, so it is `None` on a
call whose first report arrives before ours goes out. An `LSR` of zero
means the far end had received no SR from us and is not an RTT of zero.

## `RtcpEndpoint` — where RTCP lives for this call

New. Established at answer time, consumed by the RTCP thread.

| Field | Type | Purpose |
| --- | --- | --- |
| `socket` | `UdpSocket` | Local, unconnected (research Decision 7) |
| `local_port` | `u16` | What the answer states, if it states anything |
| `remote` | `SocketAddr` | Where reports are sent |
| `peer_ip` | `IpAddr` | What inbound source IPs are validated against (FR-010a) |
| `declared` | `bool` | True when tier 2 was used and `a=rtcp` must be emitted |

**Local port**, by the three-tier strategy (research Decision 1):

| Tier | Local port | Answer |
| --- | --- | --- |
| 1 | `rtp_port + 1` | Unchanged — no `a=rtcp` emitted |
| 2 | any ephemeral | `a=rtcp:<port>` added (RFC 3605) |
| 3 | none — no endpoint exists | Unchanged; warning + metric (FR-017/017a/017b) |

**Remote port**: the offer's `a=rtcp` value when it names one (FR-015),
otherwise `remote_rtp.port() + 1`. A malformed or unusable value falls back
to the convention rather than failing the call (FR-016).

**State**: created once at answer time, immutable for the call's duration.
Tier 3 is represented by the whole endpoint being absent (`Option`), so
every downstream consumer must handle "no RTCP on this call" — which is
what makes FR-017 hard to violate by accident.

## `SdpOffer.rtcp` — the offer's stated RTCP port

New field on the existing `sdp::SdpOffer` (`sdp.rs:316`), joining
`maxptime`, `direction`, `proto` and `other_media` from batches 4 and 5.

| Field | Type | Notes |
| --- | --- | --- |
| `rtcp` | `Option<u16>` | The audio section's `a=rtcp` port, `None` when absent |

**Validation**: parsed permissively, in keeping with the module's
established posture (`proto` is "captured but not validated here — the
caller decides"). A port that does not parse, or is zero, yields `None` and
therefore the convention (FR-016). Only the port form is read; RFC 3605's
optional address form is not — the peer address is already known from the
media negotiation, and honouring a *different* address for RTCP would
contradict FR-010a's own trust boundary.

## `ReportSchedule` — when the next report is due

New, private to the RTCP thread. No shared state, no member count, no
reconsideration (FR-004b, research Decision 9).

| Field | Type | Purpose |
| --- | --- | --- |
| `bandwidth_bps` | `u32` | From the declared `b=RS:` value |
| `mean_packet_size` | running mean, bytes | Of the compound packets actually sent |
| `next_due` | `Instant` | Randomised deadline |

**Transitions**: on each send, recompute the base interval as
`mean_packet_size × 8 ÷ bandwidth_bps`, randomise within ±50%, and set
`next_due`. Checked on each read-timeout wakeup — the read timeout is the
clock, so no timer exists (research Decision 3).

## `ObservedEvent::MediaQuality` — the metrics carrier

New variant on the existing control-protocol enum
(`src/control/protocol.rs:167`), the only route from Agent A to Prometheus
(research Decision 8).

| Field | Type | Constraint |
| --- | --- | --- |
| `source` | closed enum: `Local` \| `Remote` | Whose view — ours or the far end's |
| `loss_percent` | `f64` | |
| `jitter_seconds` | `f64` | |
| `round_trip_seconds` | `Option<f64>` | Absent when not derivable |

**Validation**: label-bearing fields MUST be closed Rust enums, never
strings, and MUST NOT carry per-call values (FR-008b). This is the
protocol's own stated design rule, not a new constraint — see
`ObservedEvent`'s doc comment.

A second variant or a dedicated counter covers FR-017a's "RTCP
unavailable" signal, so a bridge silently running without RTCP is visible
as a metric and not only as a log line.

## Relationships

```text
answer time (agent/inbound.rs)
  SdpOffer.rtcp ──┐
                  ├──> RtcpEndpoint (Option: absent = tier 3)
  RTP port ───────┘
                            │
        ┌───────────────────┼────────────────────┐
        │                   │                    │
  carrier-bound        carrier→veth         RTCP thread
  relay direction      relay direction      (owns socket)
        │                   │                    │
        v                   v                    │
  SendAccounting     ReceiveTracker              │
  (Arc, atomics)     (Arc<Mutex>)                │
        └───────────────────┴───────> read ──────┤
                                                 │
                                          FarEndQuality
                                          (Arc<Mutex>)
                                                 │
                       teardown (call.rs) <──────┘
                                │
                ┌───────────────┴───────────────┐
                v                               v
      "call media verdict" log         ObservedEvent::MediaQuality
      (FR-008)                         (FR-008a) ──> metrics/ingest
```

`ActiveCall` (`agent/call.rs:58`) gains one optional field holding the
handles teardown needs — alongside the `meter: MediaMeter` it already
carries for the same purpose. `Option` because of tier 3: a call can
legitimately have no RTCP at all.
