# Contract: Live Status Query

**Feature**: `017-volte-inbound-bridge` | **Satisfies**: FR-014–FR-018, FR-030–FR-033, SC-009, SC-012, SC-013

## It is a live query, not a published snapshot

The operator asks the **running service**, which answers from its own current
state (FR-033). The feature-015 registration loop publishes a status file
instead; that is adequate for "am I registered" and cannot answer "is a call in
progress **right now**", which US3 explicitly requires.

It MUST use the same channel and message shapes the Wi-Fi calling path already
uses, so one status command and one vocabulary cover both (FR-018).

## What it answers

| Field | Requirement |
|---|---|
| Registration state | FR-014 — in the shared vocabulary, not a second one |
| Call in progress, right now | FR-014 — the reason a snapshot will not do |
| Registration's remaining lifetime | FR-014 |
| Attachment state | So "registered but unreachable" is distinguishable |
| **Can it answer a call** | SC-009 — derived, and load-bearing |
| Recent call outcomes | FR-015 — enough to tell a normal call from a failed one |

**`can_answer` must never be optimistic.** Exclusive card assignment removes the
circuit-switched fallback, so if this is wrong the operator believes calls are
being taken while they are silently missed.

## Failure attribution

A failed call MUST name the stage it reached (FR-016), distinguishing at
minimum: the calling side, the telephone-system side, and the audio path. A bare
"call failed" violates this contract — the point is that an operator can act
without reproducing the failure.

## Measurements

### Call activity is shared

Calls MUST be reported through the **existing** call measurements, tagged as
this path (FR-030). Those measurements already carry a `transport` dimension, so
this adds a value rather than a metric — existing dashboards keep working
without being rebuilt (FR-032).

**What to watch**: adding a label *value* is additive for queries, because label
matching is by subset. But a panel that **groups by** transport will split into
two series. That is a visual change rather than a broken query, and FR-032 asks
for it to be verified rather than assumed — the cost of being wrong is silently
broken production dashboards.

### Registration and attachment are not shared

These stay on measurements distinct from the other paths' (FR-031), because an
operator troubleshooting needs to know **which** registration is down, not that
one of them is. The previous feature established this and it stands.

## Contract tests

| Test | Assertion |
|---|---|
| Query while idle | Registered, no call, `can_answer` true |
| Query during a call | Call reported as in progress — the live-query requirement |
| Query while unregistered | `can_answer` false |
| Query while registered but unattached | `can_answer` false |
| Failed call | Stage named, in the shared vocabulary |
| Call reported successful | Never one that carried no audio |
| Call metric emitted | Under the existing measurement, tagged as this path |
| Registration metric emitted | Distinct from the other paths' |
