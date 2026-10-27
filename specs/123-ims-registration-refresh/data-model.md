# Data Model: IMS Registration Refresh

All state is in-memory in the per-line IMS agent. The status snapshot is the only externally visible part.

## Registration (extends the existing `RegisteredSession` + `RegistrationStatus`)

| Field | Type | Notes |
|---|---|---|
| `call_id` | string | existing; the registration Call-ID, kept for the life of the registration |
| `registration_cseq` | u32 | NEW. Next REGISTER CSeq; independent of the shared request counter. Strictly increasing; error on exhaustion. Starts after the initial registration's last CSeq |
| `registration_auth` | optional cached Authorization | NEW. Verbatim header of the last successful AKA response; no Debug impl, never logged |
| `granted` / `registered_at` / `expires_at` | existing | recomputed on every successful registration or refresh |
| `refresh_mode` | `InPlace` or `Fallback` | NEW, status only |
| `consecutive_rejections` | u32 | NEW. Reset to 0 on any accepted refresh or successful fallback |
| `last_refresh` | optional (time, result) | NEW. Result is accepted, rejected(status), timed-out |
| `next_refresh_at` | optional time | NEW, derived from the schedule |

### State transitions

```
Registered --(due, idle or busy)--> Refreshing --200 own contact--> Registered (expiry moved)
Refreshing --423--> Refreshing (one retry with Min-Expires)
Refreshing --503, Retry-After > 128s--> mark P-CSCF, failover via RegistrationRecovery
Refreshing --503, Retry-After <= 128s--> Registered (still valid), retry after Retry-After
Refreshing --408/500/504/403--> Registered (still valid) + force RegistrationRecovery (deferred while busy)
Refreshing --other final error--> Registered (still valid), rejection count + 1, backoff
Refreshing --Timer F--> Registered (still valid), backoff (not a rejection)
Registered --expiry reached with no success--> Failed (reported), refresh retries continue, RegistrationRecovery when idle
rejection count reaches 2 --> next cycle runs RegistrationRecovery (Fallback), then back to InPlace
```

A late or unmatched answer never changes state.

## Refresh transaction (`PendingRegister`, Slice B)

| Field | Notes |
|---|---|
| `cseq`, `branch`, `request` | the sent request, for retransmission |
| `requested_expires` | default expiry, or the `Min-Expires` retry value |
| `deadline` | now + 128 s (Timer F); provisional answers do not extend it |
| `retransmit_at`, `retransmit_interval` | UDP only: 2 s doubling to 16 s |
| `interval_retry` | true after the single 423 retry |

Matching key: Call-ID, CSeq number, method REGISTER, exactly one `branch` in the top Via.

## Maintenance work (`lifecycle.rs`)

| Kind | Deferred while a call or origination is active? |
|---|---|
| `Renewal` (ordinary refresh) | No |
| `RegistrationRecovery` (full re-registration, transport replacement) | Yes |
| `Reattachment` (LTE re-attach) | Yes; subsumes `RegistrationRecovery` |

## Re-authentication attempt (Slice C only)

`ReauthRequest`/`Reauthenticated`: call identity and CSeq window, staged `GmSa` (new client port, same server port), result channel, cancel flag, deadline (600 s work timeout). Drop before activation rolls back only the staged SAs.

Retired sessions: `{ transport, gm, keep, retire_after }`, retained at least 128 s after a switch, extended while an origination is pending.

## Proxy pool (existing)

Reused unchanged: marks, `next_eligible`, backoff ladder, consecutive-failure budget (`MAX_CONSECUTIVE_RENEWAL_FAILURES`).
