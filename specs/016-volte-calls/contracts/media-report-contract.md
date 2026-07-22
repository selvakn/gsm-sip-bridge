# Contract: Media Report

**Feature**: `016-volte-calls` | **Satisfies**: FR-011–FR-016, FR-026, FR-028, SC-003, SC-004

Every completed call produces this report. It is the deliverable of US2 — the
evidence an operator uses to decide whether the audio is actually better, and
the only thing standing between "the call worked" and "the call worked *well*".

## Obligations

### It is always produced

A report MUST be produced for **every call that reached the answered state**,
including — especially — calls that failed on audio. A call that connects and
carries nothing is exactly the case the report exists to expose.

### It never reports a silent call as success

An answered call whose `direction_verdict` is anything other than `BothWays`
MUST be reported as a **failure** (FR-016). This is the single most important
rule here: the previous one-way-audio incident on the Wi-Fi path was painful
precisely because a broken call looked like a working one.

### It attributes a fault to a direction

The verdict MUST name which direction failed (FR-015, FR-028):

| Verdict | Meaning | Points at |
|---|---|---|
| `BothWays` | Audio flowed in both directions | — |
| `SendOnly` | We sent; little or nothing came back | The carrier isn't sending, or we can't decode what it sends |
| `ReceiveOnly` | Audio arrived; little or nothing of ours got out | Our transmit path, or the network dropping our uplink |
| `Neither` | Nothing either way | Media never established at all |

`SendOnly` vs `ReceiveOnly` is the distinction that turned the previous
incident from a mystery into a diagnosis. It MUST NOT be collapsed into a
single "no audio" state.

### The verdict is a ratio, not a floor

Derived from received-versus-sent **as a proportion**, per direction, against
a documented threshold defaulting to **10%**.

An absolute floor is wrong here: it either passes a call that received a
handful of packets across thirty seconds, or fails a short call that was fine.
A ratio stays correct regardless of call length, and a quiet answering party
still produces audio frames — so this distinguishes *"nothing is reaching us"*
from *"they said nothing"*, which a loudness measurement could not.

### It reports condition in transit, not just volume

MUST include packet counts, loss derived from sequence gaps, reordering, and
inter-arrival jitter (FR-013).

Volume alone is insufficient and misleading: a clean call and a call that
arrived in bursts with a third missing can yield similar totals. The sequence
numbers needed for this are already on the wire and already parsed — they are
simply discarded today (research R2).

### It names the format actually used

MUST report the negotiated format and whether it is wideband (FR-011).

A quality judgement made on a narrowband fallback is meaningless, so the
report MUST make it obvious when that is what happened.

### It reports how the network treated the call

MUST include the modem's per-context quality class sampled **before, during
and after** the call, and MUST state the change (FR-014).

When the modem declines to report, the result MUST be an explicit
`unavailable` with the reason and what was asked (FR-026). **Silence is not an
acceptable answer**: an implementation that always omits this would satisfy a
weaker requirement while leaving the feature's central question unanswered.

A class-1 entry appearing only during the call is the desired signal; the IMS
connection's idle baseline is class 5 (verified, research R4).

**A confirmed absence is a valid, reportable result**, not a failure of the
report.

## Contract tests

Pure over synthetic packet streams — no hardware, no carrier.

| Test | Assertion |
|---|---|
| Clean bidirectional stream | `BothWays`; zero loss; jitter within tolerance |
| Received far below sent | `SendOnly`, and the call is a failure |
| Sent far below received | `ReceiveOnly`, and the call is a failure |
| Neither direction | `Neither` |
| Received just above threshold | `BothWays` — the boundary is inclusive and documented |
| Short call, proportionally healthy | `BothWays` — proves length-independence |
| Long call, tiny absolute receive | Failure — proves an absolute floor would have passed it |
| Sequence gaps | Loss counted; reordering distinguished from loss |
| Out-of-order arrival | Counted as reordered, not lost |
| Quality class present only during | Reported as preferential handling established |
| Quality class never present | Reported as not established — a result, not an error |
| Modem declines | `unavailable` with a reason naming what was asked |

## Non-goals

- **No comparison against the modem-internal path.** Out of scope by
  clarification; the operator compares by ear. The old path cannot supply most
  of these measurements, so a symmetric comparison would promise a rigour it
  does not have.
- **No opinion on whether the audio "sounds good".** The report supplies
  evidence; the judgement is the operator's, informed also by the recording.
