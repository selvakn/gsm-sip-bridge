# Phase 1 Data Model: siptest

Entities are in-memory only; nothing is persisted but WAV recordings and a JSON
report sidecar per call. Types shown are indicative Rust, not final signatures.

---

## Account

Static configuration; the identity siptest registers with.

| Field | Type | Notes |
|---|---|---|
| `aor` | `String` | `sip:{username}@{realm}` |
| `username` | `String` | Must not equal the bridge's current `ring_aor` unless overridden (FR-005) |
| `password` | `Secret<String>` | Reuses `gsm_sip_bridge::config::secret::Secret`; never in `Debug` output (FR-034) |
| `realm` | `String` | Must match the registrar's configured realm |
| `registrar` | `SocketAddr` | |
| `expires_secs` | `u32` | Requested lifetime; the registrar may clamp |

**Validation**: `username` non-empty; `realm` non-empty; `expires_secs` within
the registrar's min/max, adopting `Min-Expires` on `423`.

---

## Registration

The live binding. Exactly one exists.

| Field | Type | Notes |
|---|---|---|
| `state` | `RegistrationState` | |
| `contact` | `String` | Advertised URI; must carry the routable LAN address (FR-004) |
| `local_addr` | `SocketAddr` | The one socket everything is sent from (R2) |
| `expires_at` | `Option<Instant>` | |
| `renews_at` | `Option<Instant>` | `min(expires/2, expires - 30s)`, floor 30 s |
| `call_id` | `String` | Stable for the registration's lifetime |
| `cseq` | `u32` | Monotonic |
| `nonce` | `Option<String>` | Cached, to pre-authorise refreshes |
| `nc` | `u32` | Monotonic per nonce; the registrar replay-tracks it |
| `last_response` | `Option<(u16, String)>` | |
| `consecutive_failures` | `u32` | Exposed in status (FR-003) |

### State transitions

```
Unregistered ──REGISTER──► Challenged ──REGISTER+auth──► Registered
     ▲                          │                            │
     │                          │ 401 (2nd time)             │ renew timer
     │                          ▼                            ▼
     └──────── backoff ──── Failed ◄──── failure ──── Refreshing
                                                            │
                                          Expires:0 on shutdown
                                                            ▼
                                                      Unregistered
```

Rules: `401` with `stale=true` adopts the new nonce and resets `nc` to 1. A
second `401` on an already-authorised REGISTER is a hard failure, never a retry
loop. Backoff 2/4/8/16/30 s, capped.

---

## Call

One dialog. At most `max_concurrent` (default 1) exist at once.

| Field | Type | Notes |
|---|---|---|
| `id` | `CallId` | Stable, agent-facing, e.g. `c-8` |
| `direction` | `Outbound \| Inbound` | |
| `state` | `CallState` | |
| `peer` | `String` | Destination dialled, or caller identity |
| `peer_uri` | `String` | Remote target after any redirect |
| `caller_id` | `CallerId` | Inbound only; see below |
| `local_tag` / `remote_tag` | `String` | |
| `call_id_hdr` / `cseq` | `String` / `u32` | |
| `remote_target` | `SocketAddr` | Where in-dialog requests go — `:5072` after redirect |
| `rtp_local` / `rtp_remote` | `SocketAddr` | |
| `codec` | `CodecProfile` | |
| `timestamps` | `CallTimestamps` | invite/180/200/ack/first-RTP/end |
| `end_reason` | `EndReason` | |
| `report` | `Option<CallReport>` | Populated on termination |

### CallerId

Three fields kept **separate** (FR-015), because an agent testing caller-ID
propagation needs to see them disagree:

| Field | Source header |
|---|---|
| `from` | `From` — rewritten by the bridge |
| `p_asserted_identity` | `P-Asserted-Identity` |
| `x_gsm_caller_id` | `X-GSM-Caller-ID` |

### Outbound state transitions

```
Idle → Inviting ──302──► Redirected ──ACK──► Inviting2 → Ringing → Answered
                                                  │                    │
                                          484/503/403/400        duration / BYE
                                                  ▼                    ▼
                                                Ended ◄──────── Terminating
```

### Inbound state transitions

```
Idle → Offered ──100,180──► Ringing ──200──► Answered ──ACK──► Established
                               │                                    │
                          CANCEL │                                  │ BYE
                               ▼                                    ▼
                             Ended ◄──────────────────────────── Ended
```

`Answered → Established` retransmits the 200 OK on a T1 ladder (500 ms
doubling, abandon at 64×T1) until the ACK arrives (FR-014).

### EndReason

`DurationElapsed` · `LocalHangup` · `RemoteBye` · `CallerCancelled` ·
`RingTimeout` · `Rejected(u16)` · `Failed(String)`

`CallerCancelled` is deliberately distinct from `Failed` — a carrier CANCEL
mid-ring is not a fault (FR-016).

---

## CallReport

The verdict bundle. Serialised to JSON, rendered to text, written as a sidecar.

### Signalling

`invite_at`, `redirect_302: Option<{contact, port}>`, `invite_to_180_ms`,
`invite_to_200_ms`, `answer_to_first_rtp_ms`, `final_status`.

The three delays here are the ones genuinely one-way and genuinely measurable
(R9). `answer_to_first_rtp_ms` is the one that catches broken media paths.

### Media counters

`sent_packets`, `sent_samples`, `received_packets`, `received_samples`,
`lost_packets`, `loss_percent`, `reordered_packets`, `jitter_ms` — from
`ims::media_stats::{ReceiveTracker, ReceiveStats}`.

### Verdicts — three orthogonal axes, never collapsed (FR-019, FR-021, FR-022)

| Axis | Type | Answers |
|---|---|---|
| `packets` | `DirectionVerdict` — `BothWays \| SendOnly \| ReceiveOnly \| Neither` | Did anything arrive? |
| `rx_audio` | `Silent \| NoiseOnly \| ToneDetected{symbol_error_pct} \| SpeechOrOther` | Was it *ours*? |
| `loopback` | `Confirmed{rtt} \| NotConfirmed` | Did our signal come back? |

Reusing `media_stats::verdict(sent, received, threshold)` unchanged — a ratio
against `max(sent, received)`, so it is call-length independent. Passed
**packets**, not samples (R6).

### Level profile

`peak_dbfs`, `mean_dbfs`, `noise_floor_dbfs`, `silent_frame_pct`. Reported
whether or not the tone was detected, so "nothing arrived" and "something
arrived that was not ours" stay distinguishable (FR-022).

### Tone

`plan`, `tx_symbols_sent`, `rx_symbols_detected`, `rx_symbol_error_pct`,
`detected`, `first_detected_ms_after_answer`,
`round_trip_delay_ms: Option<{min, median, max, samples}>`.

`None` when the signal never returned — reported as unmeasured, never as a
failure (FR-024).

### Overall

`success: bool`, evaluated against `require`:

| `require` | Success when |
|---|---|
| `signalling` | The call was answered |
| `packets` *(default)* | Answered **and** `packets == BothWays` |
| `tone-loopback` | The above **and** `loopback == Confirmed` |

Drives the process exit code (FR-032) — an answered-but-silent call is a
failure.

---

## CodecProfile

The G.722 trap encoded as data (R5). Wrong-field use is silent and corrupts the
measurement, so each consumer takes a named field.

| Field | PCMU | G.722 | Consumed by |
|---|---|---|---|
| `pt` | 0 | 9 | SDP |
| `rtpmap` | `PCMU/8000` | `G722/8000` | SDP |
| `rtp_clock_hz` | 8000 | **8000** | RTP timestamps, `ReceiveTracker` jitter |
| `audio_hz` | 8000 | **16000** | `WavWriter`, Goertzel bins, level meter |
| `samples_per_frame` | 160 | **320** | Tone generator |
| `ts_increment` | 160 | **160** | RTP timestamp step |
| `bytes_per_frame` | 160 | 160 | Framing |

---

## SignalPlan

| Field | Default | Notes |
|---|---|---|
| `plan` | `grid8` | `grid8 \| dtmf \| single \| silence` |
| `low_tones` | 600, 750, 900, 1050 Hz | |
| `high_tones` | 1300, 1500, 1700, 1900 Hz | Non-harmonic, ≥150 Hz apart, clear of the bridge's 400 Hz ringback |
| `symbol_ms` | 100 | 5 windows of 20 ms; ≥3 must agree |
| `frame_symbols` | 16 | One low + one high = 16 symbols, cycling |
| `level_dbfs` | −12.0 | Above carrier noise gates, below limiters |

A symbol's recovered index identifies its transmit time, which is what makes
round-trip delay measurable at all. Deliberately not DTMF frequencies, which a
carrier may regenerate out-of-band (R6). `silence` is a real diagnostic: send
nothing and see whether the far end's audio still arrives.

---

## SafetyPolicy

Guards outbound dialling. Outbound calls cost money and ring real people, and
an agent can retry in a loop (FR-006a, FR-006b).

| Field | Type | Default | Notes |
|---|---|---|---|
| `allowed_destinations` | `Vec<String>` | *(empty)* | Exact numbers or trailing-`*` prefixes. **Empty denies everything** — fail closed, never open |
| `min_call_interval_secs` | `u32` | 10 | Since the previous outbound *attempt*, not its completion |
| `max_calls_per_hour` | `u32` | 20 | Sliding window over attempt timestamps |

Applies to outbound only; inbound calls are never rate-limited. Both checks run
**before any signalling leaves the host**, so a refusal is locally attributable
and distinguishable from a bridge rejection.

`CallCounter` holds the sliding window: a bounded `VecDeque<Instant>` of
attempt times, pruned on each attempt.

---

## RetentionPolicy

Bounds disk and memory for a daemon left running indefinitely (FR-025a).

| Field | Type | Default |
|---|---|---|
| `max_calls_retained` | `usize` | 50 |

The `CallRegistry` is an insertion-ordered map. When a completed call would
take it past the cap, the oldest is evicted: its two WAV files and JSON sidecar
are deleted and its record dropped. An active call is never evicted.

Evicted ids are remembered in a small bounded set so a request for one reports
**gone** rather than **not found** — the agent needs to tell "you asked too
late" apart from "that id never existed".

Call ids carry a per-run prefix so sidecars left on disk by an earlier run
cannot be mistaken for this run's.

---

## InboundPolicy

Runtime-mutable without restart (FR-013).

| Field | Default |
|---|---|
| `mode` | `answer` (`answer \| reject \| manual`) |
| `answer_delay_ms` | 2000 |
| `reject_status` | 486 |
| `duration_secs` | 30 |

---

## Event

The ordered, replayable log an agent polls (FR-029).

| Field | Type |
|---|---|
| `seq` | `u64` — monotonic, gap-detectable |
| `at` | RFC 3339 timestamp |
| `kind` | see below |
| payload | kind-specific |

Kinds: `registration_state` · `incoming_call` (carries all three caller-ID
fields) · `call_state` · `media_first_packet` · `tone_detected` · `call_ended`
(carries the verdicts) · `warning` · `error`.

Held in a bounded ring buffer. `GET /events?since=N` returns everything with
`seq > N`, or blocks up to `timeout_ms` and returns `[]`.

---

## Snapshot

What `GET /status` returns: `registration`, `local` (sip addr, RTP range),
`bridge` (registrar and observed outbound target), `active_call`,
`inbound_policy`, `counters`, `event_seq`.

`local.sip_addr` is present specifically so an unroutable advertised contact —
the most likely first-run failure — is visible rather than silent (R10).
