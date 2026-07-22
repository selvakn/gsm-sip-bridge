# Data Model: Voice Calls over the Host-Side LTE Registration

**Feature**: `016-volte-calls` | **Date**: 2026-07-22

All state is in-memory and scoped to a single call. No new persisted schema:
the diagnostic command produces a report and two audio files, and the existing
store already records call history.

Entities marked **(exists)** are already in the tree and are extended rather
than replaced — which is what keeps FR-019 and FR-020 simultaneously true.

---

## CallAttempt **(exists — `ims::call::CallConfig` / `CallOutcome`)**

One outbound call.

| Field | Type | Notes |
|---|---|---|
| `callee` | E.164 string | Dialled as a telephone number, not a resolvable address |
| `ring_timeout` | duration | **(exists)** How long to wait for an answer |
| `call_duration` | duration | **(exists)** How long to hold the call once answered (FR-027) |
| `record_path` | path | **(exists)** Where the far end's audio is written |
| `record_sent_path` | optional path | **(exists)** Where our outgoing audio is written |
| `audio_source` | `SpeechSynthetic` \| `File(path)` \| `Tone` | **NEW** (FR-025) |
| `end_reason` | `DurationElapsed` \| `FarEndHungUp` \| `OperatorInterrupted` \| `AttachmentLost` | **NEW** (FR-005) |

### Validation rules

- `callee` must be E.164. A bare address without the telephone-number marker
  reaches a terminating application server that never rings the callee — this
  is recorded in `call.rs` as observed behaviour, not theory.
- `call_duration` must be long enough for a quality judgement; the default
  must satisfy SC-006's 30 seconds.
- `audio_source = File` must reference a readable audio file; **it must never
  default to anything under `samples/`** (research R3 — real recordings of
  real people).

### Call state transitions

```
Idle ──dial──> Attempting ──network accepts──> Ringing ──answered──> InCall
   │                │                             │                    │
   │                └── rejected ──> Rejected      └── no answer ──> Unanswered
   │                                                                   │
   └────────────────────── Ended (carries end_reason) ◄────────────────┘
```

`InCall → Ended` is the only transition that produces a `MediaReport`. Every
other terminal state produces a stage-attributed failure instead (FR-017).

---

## NegotiatedAudioFormat **(exists — `ims::sdp::NegotiatedCodec`)**

What both ends agreed to use.

| Field | Notes |
|---|---|
| `name` | The format actually in use — reported per FR-011 |
| `bandwidth_class` | Wideband or narrowband; decides whether a quality judgement is meaningful at all (Gate C2) |
| `offered` | Everything offered, so a refusal can name it (FR-009) |

**Validation**: if the wideband format was unavailable in the build, that must
be known *before* dialling and reported (FR-010), not discovered from a
rejection.

---

## MediaReport **NEW**

What actually happened to the audio. The core of US2.

| Field | Type | Notes |
|---|---|---|
| `sent_samples` | count | **(exists)** Outgoing audio volume |
| `received_samples` | count | **(exists)** Incoming audio volume |
| `sent_packets` | count | **NEW** |
| `received_packets` | count | **NEW** |
| `lost_packets` | count | **NEW** — derived from sequence gaps |
| `reordered_packets` | count | **NEW** — sequence arriving out of order |
| `jitter` | duration | **NEW** — inter-arrival variance |
| `direction_verdict` | `BothWays` \| `SendOnly` \| `ReceiveOnly` \| `Neither` | **NEW** (FR-015, FR-028) |

### Validation rules

- `direction_verdict` is derived by comparing received against sent **as a
  ratio**, per direction, against a documented threshold defaulting to 10%
  (FR-016). Not an absolute floor: a call that received 200ms of a 30-second
  exchange must not read as success.
- The ratio is evaluated **per direction independently**, so the verdict names
  which side failed (FR-028).
- A verdict other than `BothWays` on an answered call makes the call a
  **failure**, never a success (FR-016).

---

## QosObservation **NEW**

The modem's view of how the network is treating the connection. Sampled three
times per call.

| Field | Type | Notes |
|---|---|---|
| `phase` | `Before` \| `During` \| `After` | (FR-014) |
| `contexts` | list of `(context_id, quality_class)` | Read from the modem |
| `available` | bool | False when the modem declines to report (FR-026) |
| `unavailable_reason` | optional string | What was asked and what came back |

### Interpretation

| Observation | Meaning |
|---|---|
| A **class-1** entry present only in `During` | The network gave the call conversational-voice treatment — the desired outcome |
| No class-1 entry in any phase | The audio is being carried as ordinary data. **A valid finding**, and the one that would mean the quality gain may not materialise |
| `available = false` | Must be reported explicitly with the reason (FR-026); never silently omitted |

The IMS connection itself sits at **class 5** at idle (verified, research R4),
so the baseline is known and a class-1 entry is a clear, falsifiable signal.

---

## CallRecording **(exists)**

| Field | Notes |
|---|---|
| `far_end_path` | The received audio, playable (FR-008) |
| `sent_path` | Our outgoing audio, recorded separately so a defect can be attributed to a direction |

Deliberately **not** mixed into one file: a single mixed recording makes it
impossible to tell which side a fault came from — the precise diagnostic
mistake the prior one-way-audio incident turned on.

---

## VoicePathSelection **NEW**

Per card, which cellular voice path is used (FR-023, FR-024).

| Value | Meaning |
|---|---|
| `ModemInternal` | **Default.** Today's behaviour, unchanged |
| `HostSide` | The bridge controls signalling and media |

**Validation**: absence means `ModemInternal`. This is what makes the feature
safe to merge — no existing deployment changes behaviour unless it asks to.

---

## Relationships

```
CallAttempt ──produces──> MediaReport ──contains──> direction_verdict
     │                          ▲
     │                          │ derived from
     │                    RTP sequence + arrival times
     │
     ├──negotiates──> NegotiatedAudioFormat
     ├──samples─────> QosObservation  (Before / During / After)
     └──writes──────> CallRecording   (far end + sent, separately)

VoicePathSelection ──decides──> whether a card uses CallAttempt at all
```

A `CallAttempt` that reaches `InCall` must produce all four: a media report, a
negotiated format, three QoS observations, and two recordings. Anything less
is an incomplete result and should be reported as such rather than as a pass.
