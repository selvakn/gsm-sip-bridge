# Phase 1 Data Model: Gm Connection Liveness

**Feature**: `028-gm-tcp-reconnect` · **Date**: 2026-08-07

Nothing here is persisted. All state is either on `dispatch_loop`'s stack, in
the shared `RegistrationStatus` mutex the status listener reads, or in
`metrics::ingest`'s existing per-module record. That is deliberate — FR-017
(no stale state across a re-registration or restart) is satisfied structurally
rather than by cleanup code.

---

## `GmConnectionState` — the reported health of one line's Gm association

New enum in `ims::mod`, alongside `RegistrationState`.

| Variant | Meaning | Entered when |
|---|---|---|
| `Up` | Last liveness probe round-tripped. | A ping response matched, including the confirming ping after a reconnect (R7). |
| `Reconnecting { since: SystemTime, attempts: u32 }` | A drop was detected; repair is in progress. | A ping verdict came back dead, or the listener's `is_alive` flipped. |
| `Failed { since: SystemTime }` | Repair has been escalated to re-registration and that is also failing. | `attempts >= MAX_RECONNECT_ATTEMPTS` and the forced renewal did not succeed. |

**Transitions**

```
                 ping ok / confirming ping ok
        ┌──────────────────────────────────────────────┐
        │                                              │
        ▼                                              │
      [Up] ──ping dead / listener dead──▶ [Reconnecting]│
        ▲                                     │        │
        │                                     │ attempts >= MAX
        │                                     ▼
        └────────forced re-registration ok───[Failed]
                                              │
                                              └─ keeps retrying on backoff
                                                 (FR-010b: never terminal)
```

Notes:
- There is no `Unknown`. Before the first ping the connection is `Up` — the
  registration that just completed *is* a successful round trip, so treating it
  as unknown would report a false degradation on every startup.
- `Failed` is not terminal. FR-010b requires continued retry on backoff so a
  line can self-heal when the network recovers.
- `Reconnecting.attempts` counts *consecutive* failures. Any successful
  confirming ping resets it to zero, which is what makes FR-015's
  one-alert-per-episode boundary well-defined.

---

## `PingState` — the in-flight liveness transaction

Private to `ims::agent`, lives on `dispatch_loop`'s stack.

| Field | Type | Purpose |
|---|---|---|
| `last_sent` | `Option<Instant>` | When the last ping went out. Drives the 120s interval. |
| `pending` | `Option<PendingPing>` | The unanswered ping, if any. |

`PendingPing`:

| Field | Type | Purpose |
|---|---|---|
| `cseq` | `u32` | Correlation key. Matched against the numeric part of the response's `CSeq` header (R1). |
| `sent_at` | `Instant` | Deadline base for `PING_RESPONSE_TIMEOUT`. |

**Validation rules**
- At most one ping may be pending at a time. A new ping is never sent while
  `pending.is_some()`; the outstanding one must first resolve or time out.
- `pending` MUST be cleared whenever `*session` is replaced (R11) — a CSeq from
  the old session can never be answered on the new one, and leaving it would
  score a spurious failure ~10s after a *successful* renewal.
- A response whose CSeq does not match `pending.cseq` is ignored, not treated
  as an answer. Late responses from a previous ping must not revive a
  connection that has since been scored dead.

---

## `PingVerdict` — the pure decision step

The extracted, directly unit-testable function (R12). Input: `&PingState`,
`now: Instant`, `call_in_progress: bool`. No I/O.

| Verdict | Condition |
|---|---|
| `Idle` | A call is in progress (FR-006/R10 — a call proves liveness by itself), or the interval has not elapsed and nothing is pending. |
| `Send` | No ping pending and `last_sent` is `None` or older than `PING_INTERVAL`. |
| `Await` | A ping is pending and within `PING_RESPONSE_TIMEOUT`. |
| `Dead` | A ping is pending and older than `PING_RESPONSE_TIMEOUT`. |

A send error at the call site is folded into `Dead` by the caller (FR-022);
the function itself never sees I/O.

---

## Constants

Added to `ims::agent`, beside `RENEWAL_HEADROOM` / `RETRY_INITIAL_BACKOFF`.

| Name | Value | Rationale |
|---|---|---|
| `PING_INTERVAL` | `120s` | Spec clarification Q2. Worst-case dead-line duration ~130s; ~30 exchanges/line/hour (R10). |
| `PING_RESPONSE_TIMEOUT` | `10s` | Generous against a P-CSCF's normal response time, 12× inside the ping period (R2). |
| `MAX_RECONNECT_ATTEMPTS` | `3` | Three failed transport rebuilds is strong evidence the layer underneath is the problem, so escalate (R6). At the existing backoff this is ~35s before escalation. |

`GM_CONNECTION_ALERT_THRESHOLD` lives with the other alert thresholds in
`metrics::ingest`, not here — see contracts/metrics.md.

---

## Extensions to existing types

### `ims::RegistrationStatus` (the shared status snapshot)

| New field | Type | Notes |
|---|---|---|
| `gm_connection` | `GmConnectionState` | Defaults to `Up`. In-memory only, like `attached`/`busy`/`pbx_registered` — `render_status` persists only the original four fields, so a status read from disk carries the default. |

### `ims::lifecycle::ServiceHealth`

| New field | Type | Notes |
|---|---|---|
| `gm_connection_up` | `bool` | Folded into `can_answer()` (R9). In `blocked_reason()` it is ordered **after** `attached` and `registered` (both are underneath it, so reporting the symptom over the cause would mislead) and **before** `pbx_registered`. Message: `"the carrier signaling connection is down"`. |

### `ims::session::Inbound`

No new fields. `_server` is replaced wholesale by `restart_gm_server`, mirroring
how `restart_client_reader` replaces the client reader thread.

### `sip_client::GmServer`

| New field | Type | Notes |
|---|---|---|
| `alive` | `Arc<AtomicBool>` | Initialised `true`; cleared by the accept loop on its fatal-exit path. Read via `is_alive()`. Distinct from the existing `stop` flag, which is an *instruction to* the loop rather than a *report from* it — conflating them would make a deliberate shutdown indistinguishable from a crash. |

### `control::protocol::AgentState`

| New field | Type | Notes |
|---|---|---|
| `gm_connection_up` | `Option<bool>` | `#[serde(skip_serializing_if = "Option::is_none", default)]`, matching every sibling. `None` from an older peer means "not reported", which the ingest side must treat as "no change", never as "down". |

### `control::protocol::RegistrationStatusReply`

| New field | Type | Notes |
|---|---|---|
| `gm_connection` | `String` | `#[serde(default)]` for wire compatibility with an older peer. Rendered form of `GmConnectionState`: `"up"`, `"reconnecting since <ts> (attempt N)"`, `"failed since <ts>"`. A `String` rather than a typed enum for the same reason `state` already is one — the CLI prints it verbatim. |
