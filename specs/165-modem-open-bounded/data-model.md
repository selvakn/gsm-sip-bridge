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
| `finished: bool` (under the map's mutex) | the thread, when the open returns | the caller, on timeout | whether the result has been sent |
| result channel (`mpsc`, one value) | the thread | the caller | the opened port, or the open's error |

## States and transitions

```text
                 caller: map has path?
                 ├─ yes ─────────────────────────────► FAIL "an earlier open of <port> has not returned"   (FR-005)
                 └─ no ──► spawn thread, wait ≤ open_bound(timeout)
                                │
        result in time ◄────────┤
        (Ok → Session → start_worker/sync as today; Err → "failed to open serial …" as today)   (FR-009)
                                │
                     timeout ───┤ caller takes the map's mutex
                                ├─ finished already → try_recv → handled as "result in time"
                                └─ not finished     → insert path → id; FAIL "<port> did not open within N ms"   (FR-003)

thread, when the open returns:
    send(result); if the caller is gone, drop the result (closes the port, releases flock)   (FR-004)
    take the map's mutex; finished = true; if map[path] == id, remove it                      (FR-006)
```

Invariants:

- An entry exists only while its thread has not finished (D4's race is closed by `finished` and the mutex).
- A port reaches a caller only through a result received in time; any other result is dropped by the thread or with the channel.
- A thread removes only its own entry, so a later attempt's entry is never cleared by an earlier thread.
