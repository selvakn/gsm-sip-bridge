# Phase 1 Data Model: session-refresh state

This feature adds no persistent storage. It adds one small, pure state
machine (`agent::session_refresh`) plus one new field on the existing
`ActiveCall` (`agent/call.rs`), following the same shape `agent::ping`'s
`PingState` already established for a structurally identical problem
(track something we sent, wait for its response, act on a deadline).

## Refresher (new entity — RFC 4028 §7.1's `refresher` parameter, parsed)

```rust
enum Refresher { Uac, Uas }
```

`Uac` means this bridge holds the refresh duty; `Uas` means the carrier
does. Parsed from the `Session-Expires` header's `refresher=` parameter
(research.md Decision 2); defaults to `Uac` when the parameter is absent
but `Session-Expires` itself is present (the one defensive default this
feature adds beyond RFC 4028's literal text).

## SessionRefreshState (new entity — one per `ActiveCall`, when applicable)

| Field | Type | Role |
|---|---|---|
| `interval` | `Duration` | The negotiated session interval — the `Session-Expires` delta-seconds, fixed for the call's lifetime (research.md Decision 4) |
| `refresher` | `Refresher` | Which side performs refreshes |
| `phase` | `RefreshPhase` | The mutable part — where in the refresh cycle this call currently is |

`ActiveCall.session_refresh: Option<SessionRefreshState>` — `None` exactly
when the outbound call's `200 OK` carried no `Session-Expires` (today's
behavior, unchanged: no obligation, nothing this feature does ever runs for
that call).

## RefreshPhase (new entity — the state `verdict()` acts on)

```rust
enum RefreshPhase {
    /// refresher == Uac: no refresh in flight; send one once `due_at` passes.
    WaitingToSend { due_at: Instant },
    /// refresher == Uac: a refresh was sent; waiting for its response.
    /// `sent_at`/`first_cseq` are fixed at the first attempt (the overall
    /// ceiling and the low end of the outstanding range); `latest_cseq`/
    /// `last_attempt_at`/`attempts` track up to `MAX_SESSION_REFRESH_ATTEMPTS`
    /// bounded resends in between, at `SESSION_REFRESH_RETRY_INTERVAL`
    /// apart — a single lost datagram must not end an otherwise-healthy
    /// call (PR #74 review). A response naming *any* `CSeq` in
    /// `first_cseq..=latest_cseq` settles the cycle, not just the latest —
    /// a late response to an earlier attempt is still a real, valid answer
    /// (PR #74 review, second pass).
    AwaitingResponse {
        first_cseq: u32,
        latest_cseq: u32,
        sent_at: Instant,
        last_attempt_at: Instant,
        attempts: u8,
    },
    /// refresher == Uac: the sent refresh's response was a failure (non-2xx,
    /// or the `send()` call itself failed) — resolved to fatal on the very
    /// next `verdict()` check, one dispatch-loop tick later. A distinct
    /// variant rather than back-dating `sent_at` past the timeout, so the
    /// "why" survives into any future debug logging.
    Failed,
    /// refresher == Uas: waiting for the carrier's own in-dialog refresh
    /// before `deadline` (RFC 4028 §10: `min(32s, interval/3)` before the
    /// session would otherwise expire).
    WaitingForPeer { deadline: Instant },
}
```

## RefreshVerdict (new entity — computed, not stored; mirrors `PingVerdict`)

```rust
enum RefreshVerdict {
    /// Nothing to do this tick.
    Idle,
    /// refresher == Uac, `due_at` has passed: send a refresh now.
    SendNow,
    /// The refresh cycle has failed — end the call. Covers both
    /// `AwaitingResponse` past its timeout and `Failed`, and refresher ==
    /// Uas's `WaitingForPeer` past `deadline`. One verdict for the one
    /// action both cases require (research.md Decision 5), even though the
    /// two causes are logged distinctly.
    Overdue,
}
```

`SessionRefreshState::verdict(&self, now: Instant) -> RefreshVerdict` is
pure — takes `now` as a parameter so tests never sleep, exactly like
`PingState::verdict`.

## Mutators (mirror `PingState::on_sent`/`on_response`)

- `on_sent(&mut self, cseq: u32, now: Instant)` — from `WaitingToSend`:
  `AwaitingResponse { first_cseq: cseq, latest_cseq: cseq, sent_at: now, last_attempt_at: now, attempts: 1 }`
  (first attempt). From `AwaitingResponse` (a bounded resend): keeps the
  original `sent_at`/`first_cseq`, bumps `attempts`, updates
  `last_attempt_at` and `latest_cseq` — the overall ceiling never moves,
  and every earlier attempt stays a valid answer, just because a retry
  went out.
- `is_awaiting_response(&self, cseq: u32) -> bool` — whether `cseq` falls
  in `first_cseq..=latest_cseq` while `phase` is `AwaitingResponse`; how
  `handle_carrier_response` decides a response belongs to this refresh at
  all before handing it to `on_response`.
- `on_response(&mut self, cseq: u32, status: u16, now: Instant)` — only
  acts if `is_awaiting_response(cseq)` (a response to a cycle already
  resolved, or not yet started, is ignored, same discipline
  `PingState::on_response` applies to its single pending value — widened
  here to a range since more than one attempt can be outstanding at once,
  PR #74 review second pass) and `status >= 200` (a provisional like
  `100 Trying` is not a verdict on the transaction — PR #74 review);
  `status` 2xx → `WaitingToSend { due_at: now + interval/2 }` (armed for
  the *next* cycle); any other final response → `Failed`.
- `on_send_failed(&mut self)` — `AwaitingResponse`/`WaitingToSend` →
  `Failed` directly, for when the `send()` call itself errors (no
  transaction was ever created to time out).
- `on_peer_refresh(&mut self, now: Instant)` — (refresher == `Uas` only)
  `WaitingForPeer` → `WaitingForPeer { deadline: now + interval -
  min(32s, interval/3) }`, re-armed for the next cycle.

## ActiveCall (existing entity, extended)

| Field | Type | Role |
|---|---|---|
| `control`, `ctrl_rx`, `stop`, `call_id`, `to_tag`, `dialog`, `caller`, `answered_at`, `answered_instant`, `meter`, `lifecycle`, `answered_invite`, `rtcp` | (existing) | Unchanged |
| **`session_refresh`** | `Option<SessionRefreshState>` (new) | Set once, at call creation, from the outbound `200 OK`'s `Session-Expires`; `None` for every inbound-answered call (this feature is UAC-leg-only, FR-011) and for an outbound call whose `200 OK` carried no `Session-Expires` |

## DialogInfo (existing entity, extended)

| Field | Type | Role |
|---|---|---|
| `remote_target`, `route_headers`, `from`, `to`, `local_addr`, `use_tcp`, `cseq` | (existing) | Unchanged |

New method: `build_update_for(&mut self, call_id: &str, session_expires:
&str) -> String` — increments `self.cseq` (so a later `build_bye_for`
always picks a strictly higher `CSeq`, RFC 3261 §12.2.1.1) and builds a
body-less `UPDATE` via a new `sip_client::build_update`, mirroring
`build_bye_for`/`build_bye` exactly except for the method name and the
added `Supported: timer`/`Session-Expires` headers (RFC 4028 §7.1: a
request using the extension must carry `Supported: timer`).

## EndedBy / reason (existing entities, extended)

| New variant/const | Value | Role |
|---|---|---|
| `EndedBy::SessionTimerExpired` (`ims/lifecycle.rs`) | `as_str()` = `"session_timer_expired"` | FR-012's distinct signal — picked up automatically by `report_answered_call_ended`'s existing log line and `gsm_sip_bridge_calls_total` |
| `reason::SESSION_TIMER_EXPIRED` (`vowifi/control.rs`) | `"session_timer_expired"` | The `ControlMessage::CallEnded` reason Agent B receives |

## Relationship to the spec's Key Entities

| Spec term (`spec.md`) | Concrete type |
|---|---|
| Session Refresh State | `SessionRefreshState` |

## Control flow (conceptual — one `dispatch_loop` tick, active call present)

```
tick begins
    │
    ▼
handle_pbx_hangup / handle_attachment_loss ──(call ended)──▶ continue
    │ (call still active)
    ▼
handle_session_refresh:
    call.session_refresh? ──None──▶ nothing to do
        │ Some(refresh)
        ▼
    refresh.verdict(now)
        │
        ├─ Idle ─────────▶ nothing to do
        │
        ├─ SendNow (refresher==Uac) ─▶ build+send UPDATE
        │       send Ok  ─▶ refresh.on_sent(cseq, now)
        │       send Err ─▶ refresh.on_send_failed() (Overdue next tick)
        │
        └─ Overdue ──▶ end the call: EndedBy::SessionTimerExpired,
                        report_answered_call_ended, hangup_carrier
                        (reason::SESSION_TIMER_EXPIRED) ──▶ continue
    │
    ▼
(later, on inbound.rx)
  response to our own UPDATE arrives ──▶ handle_carrier_response's new
      branch: on_response(cseq, status, now)
  carrier's own UPDATE arrives (refresher==Uas) ──▶ new dispatch_loop
      UPDATE arm: body-less + matches active dialog + Uas ⇒ accept (200 OK),
      refresh.on_peer_refresh(now); anything else ⇒ today's unchanged
      unserved_method_response decline
```
