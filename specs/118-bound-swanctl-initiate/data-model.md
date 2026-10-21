# Data Model: Bounded tunnel initiation

All state is in-process and per line unless noted.

## InitiateOperation (new, on `StrongswanEngine`)

`RefCell<Option<Pending>>` where `Pending { handle: ChildHandle, started: Instant }`.

| Transition | Trigger | Effect |
| --- | --- | --- |
| none -> pending | `initiate()` with no live client | spawn tracked client; log `initiate started` |
| pending -> pending | `initiate()` while `is_alive` | skip; log `initiate skipped: previous client still running (Ns)` |
| pending -> none | `initiate()` finds the handle dead | `wait` it, log exit (0 = established or detached at deadline, non-zero = failed), then start a new one |
| pending -> none | `cancel_initiate()` | `reap` the handle |

Invariant: at most one entry per engine, so at most one client per line.

## EstablishTracking (new, owned by the establish loop)

| Field | Meaning |
| --- | --- |
| `half_open: Option<HalfOpen>` | the same record steady state keeps (ike id, activity fingerprint, last activity time) |
| `vici_strikes: u8` | consecutive timed-out probes; reset on any answered probe |

## Establish tick decision (re-initiate ticks only)

| Probe result | Action |
| --- | --- |
| timed out, strikes < 2 | count it; no initiate |
| timed out, strikes = 2 | `restart_if_wedged`, reset strikes |
| no IKE_SA for this connection | `initiate()` |
| `CONNECTING`, activity within 200s | leave to charon |
| `CONNECTING`, idle > 200s | `terminate` + `initiate()` |
| established, no P-CSCF yet | existing: `terminate` + `initiate()` |

## SharedCharon (extended)

Adds `restart_if_wedged(observed_generation) -> bool`; uses the existing `log_generation` as the "someone already restarted it" guard. No new fields.

## Constants (new)

| Name | Value | Constraint |
| --- | --- | --- |
| `INITIATE_DETACH_SECS` | 20 | passed as `--timeout` |
| `INITIATE_HARD_KILL_SECS` | 25 (+2 `-k`) | < 30s cadence |
| `VICI_UNRESPONSIVE_STRIKES` | 2 | consecutive probe timeouts |
