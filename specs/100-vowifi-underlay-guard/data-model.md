# Data model: VoWiFi underlay guard and the half-open IKE_SA fix

Every rule here is a time, never a number of ticks: the loop ticks every 30s normally and every 5s around a hold, so "the next tick" means a different span depending on when it is asked.

Everything here is in-memory and per line, except `EpdgHeard` (below), owned by the strongSwan steady-state loop (`supervise/orchestrate.rs`) or its tick (`supervise/line_supervisor.rs`). The only things that leave the process are:
- the `AgentState.uplink` field and two events (contracts/observability.md);
- the per-line `/tmp/pcscf-N.uplink` file (contracts/status-file.md).

Nothing persists across a supervisor restart (spec Edge Cases).

## UnderlayVerdict

What the guard currently believes about the path from this host to one line's ePDG.

| Variant | Meaning | Entered when | Left when |
|---|---|---|---|
| `Up` | Nothing suggests the path is down. The default. | Start; any exit below. | — |
| `NoRoute` | The host has no usable route (or no source address) to the ePDG. | 2 consecutive `ip route get` failures ≥10s apart (FR-002, research R3), **or** 1 failure on a tick that is about to take a reachability-inferred recovery. | The first successful `ip route get` with a `src` (FR-006). |
| `NoResponse` | The route is fine, but the ePDG has stopped answering. | Since a log mark: ≥2 retransmits of an outstanding request, or an on-demand DPD unanswered for one tick, **and** no evidence of life (research R4). | Any evidence of life after the mark: a parsed IKE message from the ePDG, a younger inbound ESP age, or a grown tunnel RX counter (FR-006). |

`NoRoute` takes precedence when both hold: local state is checked first and is the cheaper explanation.

Wire form (`UplinkState`, contracts/observability.md): `up` / `no_route` / `no_response`.

## UnderlayView (per-tick input)

Built by the loop at the top of each tick and passed into `tick_steady_state_with_pool`. The tick reads it and never mutates it.

| Field | Type | Notes |
|---|---|---|
| `enabled` | bool | `[vowifi].underlay_guard`. When false, `verdict` is always `Up`, `route_ok` is always true, and `evidence_of_life` is always true (R12). |
| `verdict` | `UnderlayVerdict` | As above. |
| `route_ok` | bool | This tick's `ip route get` result, for the one-failure-plus-pending-recovery rule. |
| `evidence_of_life_since(mark)` | fn → bool | Wraps the R4 parser for the pending liveness check. |

`UnderlayView::disabled()` is the guard-off value.

## UnderlayTracker (loop-owned, per line)

| Field | Type | Purpose |
|---|---|---|
| `route_failures` | `u32` + `first_failure_at: Instant` | Debounce for FR-002. |
| `hold` | `Option<Hold>` | The current hold, if any. |
| `pending_check` | `Option<PendingCheck>` | An on-demand DPD in flight (FR-004). |
| `evidence_mark` + `silence_since` | `LogMark` + `Instant` | Where silence is counted from. Moved on whenever the line is up with nothing outstanding, **or the ePDG is heard from by any means** (a parsed message, a grown RX counter, another line on the ePDG), so old retransmits never count against a path that has since answered. |
| `heard` | `Option<Heard>` | The latest this line heard from the ePDG, as the window `(after, by]` one tick can place it in. `heard_recently` in the view is `by` within 30s (`RECENT_LIFE`). |
| `held_initiate_sent` | bool | FR-010a: whether this hold already issued its one initiation since the IKE_SA vanished. Reset when an IKE_SA is listed again. |

### Hold

| Field | Type | Notes |
|---|---|---|
| `cause` | `NoRoute` \| `NoResponse` | The current cause. It can change while the path stays down: that is the same hold, reported as `ended_by = cause_changed` for the old cause and a start for the new one. |
| `started` / `started_at` | `Instant` + `SystemTime` | When the hold began, whatever its causes since: `Instant` for the 15-minute bound, `SystemTime` for the status file. |
| `cause_since` | `Instant` | When the current cause began; `held_secs` is measured from here. |
| `skipped` | `Vec<DegradeReason>`, deduplicated | Recoveries held back under the current cause, for the line that ends it. |
| `marks_from` | `SystemTime` | When the hold ends because the path returned, this becomes `tracking.negotiated_at`, so agent P-CSCF marks set during the hold don't count (FR-011; 097's `exhausted_pool` already ignores marks older than `negotiated_at`). A hold ended by `max_hold` keeps them for the ordinary pass. |

### PendingCheck

| Field | Type | Notes |
|---|---|---|
| `mark` | `LogMark` | Taken immediately before `swanctl --dpd`. |
| `rx` | `Option<u64>` | The tunnel RX counter read on the same tick, before the DPD went out. Growth past it answers the check; growth before it does not. |
| `sent` | `Instant` | When the sending tick began. Until `sent + 30s` (`LIVENESS_WAIT`) an unanswered check keeps the recovery `Deferred` without a second DPD; after it, it holds the line. |
| `deferred` | `DegradeReason` | The recovery waiting on the answer. |

## EpdgHeard (shared, on `SharedCharon`)

FR-001: lines sharing an ePDG share the evidence their verdicts rest on. Each line records the `Heard` window of what it heard on each tick, keyed by ePDG address and line; each line reads the latest of the *other* lines' records (its own it reads more precisely from the log). Another line's word counts as life for the silence count and for a pending check only if its window starts after the silence began or the check was sent, so it never answers for traffic from before. Holds, pending checks and strikes stay per line, because recovery does.

## Additions to SteadyTracking (tick-owned)

| Field | Type | Purpose |
|---|---|---|
| `half_open` | `Option<HalfOpen>` | The current half-open IKE_SA, if any (R6). |

### HalfOpen

| Field | Type | Notes |
|---|---|---|
| `ike_id` | `u32` | `#N` from the `swanctl --list-sas` header. A different `#N` replaces the record. |
| `activity` / `active_at` | fingerprint / time | The last IKE-traffic fingerprint seen and when it last changed; a different fingerprint on a tick resets `active_at`. |
| `waiting_logged` | bool | "waiting for negotiation" is logged once per `#N` (US3 scenario 1). |

Charon's IKE activity is a fingerprint (the count of `<conn|N>` IKE lines and the last of them), compared between ticks, never a parsed time: the log is stamped in the container's local time, which the Jio Pi found out is not UTC (R6). `HalfOpen` carries the last fingerprint and `active_at`, the time it last changed (or the `#N` was first listed); idle time is measured from there.

## New enum variants

| Enum | New variant | Wire label |
|---|---|---|
| `SteadyStateHealth` | `IkeSaConnecting { ike_id: u32 }` | — (internal) |
| `DegradeReason` | `IkeSaNegotiationStuck` | `TunnelRecoveryReason::IkeSaNegotiationStuck` → `ike_sa_negotiation_stuck` |
| `SteadyOutcome` | `Held { cause, skipped: DegradeReason }` | — |
| `SteadyOutcome` | `Deferred { reason: DegradeReason }` | — |
| `SteadyOutcome` | `WaitingForNegotiation` | — |
| `DeadPeerVerdict` | `Uplink` | `uplink` |
| `AgentState` | `uplink: Option<UplinkState>` | `"uplink":"no_route"` |
| `ObservedEvent` | `UplinkHoldStarted { cause }`, `UplinkHoldEnded { cause, held_secs, ended_by }` | contracts/observability.md |

`recovery_restarts_agent(IkeSaNegotiationStuck)` and `recovery_renegotiates(IkeSaNegotiationStuck)` are both `true`, the same as `ChildSaStuck`.

## Reachability-inferred vs structural

| Kind | Reasons | Held? |
|---|---|---|
| Reachability-inferred (FR-007) | `PcscfUnreachable`, `PcscfPoolExhausted`, `IkeSaLost` (re-initiate), `ChildSaMissing`, `ChildSaStuck`, `ChildSaRekeyStuck`, `IkeSaNegotiationStuck` | Yes |
| Structural (FR-009) | `ProcessDied`, `ViciBroken`, `TunVanished`, `TunUnavailable`, `DefaultRouteMissing` | Never |
| Neither | `PcscfChanged` (a refresh, not a recovery) | Never |

`fn is_reachability_inferred(DegradeReason) -> bool` is exhaustive with no wildcard arm, like the existing `From<DegradeReason>`, so a new reason does not compile until it is classified.

## Per-tick decision

Evaluated in this order; the first matching row wins. "Would take R" means the existing tick logic, unchanged, would issue recovery R this tick.

| # | Condition | Outcome |
|---|---|---|
| 1 | Process dead / vici broken / tunnel interface vanished | Structural recovery, as today (FR-009). |
| 2 | Namespace default route missing | Route repair, as today (FR-009). |
| 3 | IKE_SA half-open, with IKE activity in the last 200s | `WaitingForNegotiation`: no action; log once per `#N` (FR-015). |
| 4 | IKE_SA half-open, no IKE activity for >200s, verdict `Up` | Terminate + re-initiate, as `IkeSaNegotiationStuck` (FR-016). |
| 5 | Verdict `NoRoute` or `NoResponse` and would take R (reachability-inferred) | `Held { cause, skipped: R }`, issue nothing (FR-007/008). Exception, FR-010a: R is the `IkeSaLost` re-initiate and `held_initiate_sent` is false → issue one `--initiate`, no agent restart, and set the flag. |
| 6 | Verdict `Up`, would take R, `PendingCheck` exists | Evidence of life since it was sent → take R. None, and its 30s are not up → `Deferred { reason: R }`, no second DPD. None after 30s → verdict becomes `NoResponse` and row 5 applies. |
| 7 | Verdict `Up`, would take R, route check failed this tick (not yet debounced) | Verdict becomes `NoRoute` now and row 5 applies (the one-failure-plus-pending-recovery rule). |
| 8 | Verdict `Up`, would take R, R ≠ `IkeSaLost`, the IKE_SA is `ESTABLISHED`, and nothing proves the ePDG alive in the last 30s (no parsed IKE message, inbound ESP age > 30s) | Trigger `swanctl --dpd`, create `PendingCheck`, `Deferred { reason: R }` (FR-004). |
| 9 | Otherwise | Existing behaviour, unchanged. |

Row 7 comes first because a DPD can't be sent without a route. Row 8 excludes `IkeSaLost`: with no IKE_SA there is nothing to send a DPD on. That case is covered by the retransmit evidence of the re-initiate itself (R4) and by FR-010a.

With the guard off, the verdict is always `Up` and evidence of life always true, so rows 5–8 never fire. Rows 3–4 (the half-open fix) still apply.

## Hold lifecycle

```
            route_get fails ×2 (≥10s apart), or ×1 with a pending recovery
   ┌────┐ ─────────────────────────────────────────────────────────────▶ ┌────────────┐
   │ Up │                                                                │ held:      │
   │    │ ◀──── route_get ok (with src) ──────────────────────────────── │ no_route   │
   │    │                                                                └────────────┘
   │    │  retransmits ≥2 / DPD unanswered, no life                      ┌────────────┐
   │    │ ─────────────────────────────────────────────────────────────▶ │ held:      │
   │    │ ◀──── any evidence of life ─────────────────────────────────── │ no_response│
   └────┘                                                                └────────────┘
      ▲            held for 15 minutes (either cause)                          │
      └────────── ended_by=max_hold: one ordinary recovery pass; ◀────────────┘
                  a new hold may start on fresh evidence (FR-012)
```

On every transition into a hold:
1. `UplinkHoldStarted` event;
2. state report `uplink=<cause>`;
3. write the status file;
4. one log line with the cause and evidence (FR-020).

On every transition out of a hold:
1. `UplinkHoldEnded` event;
2. state report `uplink=up`;
3. write the status file;
4. one log line with the duration, `ended_by` and the skipped reasons;
5. if it ended because the path returned (`restored`), set `tracking.negotiated_at = now` (FR-011); at `max_hold` the marks stay for the ordinary pass (FR-012);
6. reset `held_initiate_sent`;

A change of cause while the path stays down (*no response* → *no route*) is not a transition out: it logs and reports the end of the old cause (`ended_by = cause_changed`) and the start of the new one, keeps the hold's start for the 15-minute bound, and does none of steps 5–7.
7. if no IKE_SA is established or in flight, issue one `--initiate` (FR-013). The unheld tick that follows does this itself: a missing IKE_SA is `IkeSaLost` and re-initiated. A half-open IKE_SA left from a *no route* hold is terminated and re-initiated on that tick (`UnderlayView.restored`), reported as `IkeSaNegotiationStuck`; after a *no response* hold it is left alone, since it has just answered.

On shutdown while held: `UplinkHoldEnded { ended_by: shutdown }`, so the held-seconds counter is complete.

## Validation rules (from the spec)

- The debounce is 2 failures, ≥10s apart (FR-002). At the 30s tick that's two consecutive ticks.
- A liveness check costs at most one `swanctl --dpd` per pending recovery (FR-004). A second suspicion while `PendingCheck` exists reuses it.
- A hold lasts at most 15 minutes continuously, whatever its causes (FR-012).
- A liveness check is given 30s to be answered, measured from when it was sent, however often the loop ticks (FR-004).
- The tick's strike counters (`pcscf_unreachable`, `child_sa_missing`) are `Streak`s: a count and the time it covers. P-CSCF unreachable needs ≥2 checks covering 90s; a missing CHILD_SA is re-initiated once on the first miss and rebuilt once ≥2 checks cover 60s, with no further re-initiates in between. Both are reset on every tick the path is down (rows 5 and 7), so a failure seen during an outage never counts after it; they are earned only with the path up.
- At most one held initiation per IKE_SA disappearance (FR-010a).
- Half-open inactivity bound: 200s (FR-015).
