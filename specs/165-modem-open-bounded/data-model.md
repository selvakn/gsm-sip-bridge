# Data Model: Bounded Modem Port Open

No persisted data. One piece of in-process state, owned by `modules/at_commander.rs`.

## Outstanding opens

A process-wide `Mutex<Vec<OpenAttempt>>` (`OUTSTANDING_OPENS`): one entry per open that a caller gave up on and that has not returned, each with its port path and id. A handful of entries at most, so a list rather than a map.

- **Key**: the path the open was asked for, as given (no canonicalisation, as today: every caller of a line uses the line's configured port path).
- **Value**: the open's id, from a process-wide counter, so a finishing thread removes only its own entry.
- **Size**: no entry is added for a port that already has one, because no new open of it starts (FR-005). Opens already under way when the port hung may each time out and add their own entry, so a port can have as many entries as it had concurrent opens at that moment (one in the usual case; the SMS sweep and a refresh challenge are the only two that overlap on a line), and that number never grows.

## One open attempt

Shared between the caller and the open's thread:

| Field | Written by | Read by | Meaning |
|---|---|---|---|
| `id: u64` | caller, at start | both | identifies this attempt in the map |
| `OpenSlot::handoff: Mutex<Handoff>` | both, only under its mutex | both | `Waiting`, `Delivered(result)`, `Abandoned`, `Gone` (the opener panicked) or `Taken` |

The caller's timeout decision and the thread's delivery are made under the same `handoff` mutex (revised after review: a channel plus a `finished` flag let a port delivered between the caller's timeout and its decision be dropped on the caller's thread).

## States and transitions

```text
                 caller: map has path?
                 ├─ yes ─────────────────────────────► FAIL "an earlier open of <port> has not returned"   (FR-005)
                 └─ no ──► spawn thread, wait ≤ open_bound(timeout)
                                │
        result in time ◄────────┤
        (Ok → Session → start_worker/sync as today; Err → "failed to open serial …" as today)   (FR-009)
                                │
                     timeout ───┤ the wait returns holding `handoff`
                                ├─ Delivered → handled as "result in time"
                                ├─ Gone      → FAIL "the open of <port> ended without a result"
                                └─ Waiting   → handoff = Abandoned; insert path → id (both under `handoff`);
                                               FAIL "<port> did not open within N ms"   (FR-003)

thread, when the open returns (or its opener panics):
    take `handoff`
    ├─ Waiting   → handoff = Delivered(result) (Gone on a panic); wake the caller
    └─ Abandoned → release `handoff`; drop the result on this thread (closes the port,
                   releases flock)   (FR-004); then remove its own map entry   (FR-006)
```

Invariants:

- An entry exists only while its thread has not finished: the caller adds it under `handoff` before the thread can see `Abandoned`, and the thread removes it only after closing what it got.
- A port reaches a caller only while that caller is still waiting; any other port is closed on the open's thread, never on the caller's.
- A thread removes only its own entry, so a later attempt's entry is never cleared by an earlier thread.
