# Phase 1 Data Model: Bounded modem I/O and stalled-line detection

**Feature**: 039-at-stall-watchdog | **Date**: 2026-08-17

Entities here are in-process state, not persisted records. Nothing in this feature adds
a database table or changes the on-disk store.

## 1. Phase

What a monitored activity is currently doing. Backed by a `u8` discriminant so it can
live in an atomic and be read by the watchdog while the owning thread is blocked.

| Phase | Meaning | Budget | Derivation |
|---|---|---|---|
| `Idle` | Waiting for work | 15s | `IDLE_POLL_INTERVAL` 1s, ×15 slack |
| `Startup` | First registration, incl. PLMN derive | 420s | modem open + full initial registration |
| `GmProbe` | Signalling liveness probe | 30s | one OPTIONS round trip + reconnect |
| `Renewal` | Re-registration | 360s | 30s open + ≤34 APDUs × 5s + 2 × 32s + SA install ≈ 284s, +~25% |
| `InboundCall` | Answering and bridging | 180s | control timeout + ring + bridge setup |
| `Origination` | Placing an outbound leg | 120s | invite 15s + ring 60s + slack |
| `SmsSweep` | Sweeping modem message storage | 90s | open retry ≈1.8s + per-message AT, bounded by sweep size |

**Invariant (test-enforced, FR-033)**: every budget exceeds the summed worst-case of the
operations its phase performs, by ≥20%. A unit test recomputes this from the real
constants so a future timeout bump fails the build rather than silently arming a
false-restart.

## 2. Progress

One per monitored activity. The dispatch loop owns one; the SMS sweep thread owns
another; the VoLTE carrier agent owns a third (FR-032).

| Field | Type | Notes |
|---|---|---|
| `base` | `Instant` | Process-start reference; monotonic (FR-014) |
| `phase` | `AtomicU8` | Current `Phase` |
| `phase_started_ms` | `AtomicU64` | Milliseconds since `base` |
| `busy` | `AtomicBool` | A call is in progress — drives deferral (FR-029) |
| `label` | `&'static str` | Which activity, for the log marker |

Operations: `enter(phase)`, `leave()` (→ `Idle`), `set_busy(bool)`, `snapshot()`, and a
pure `stalled_for(now) -> Option<(Phase, Duration)>`.

`PhaseGuard` is an RAII wrapper returning to `Idle` on drop, so the five early returns in
`on_idle_tick` cannot leave a stale phase armed.

**State transitions**: `Idle → <any working phase> → Idle`. Any phase may be entered from
`Idle`; nested phases are not modelled (the loop is single-threaded within an activity).

## 3. StallVerdict

Produced by the watchdog's pure decision function; the only input to acting.

| Variant | Meaning |
|---|---|
| `Healthy` | Within budget |
| `Suspected { phase, elapsed }` | Over budget, first observation — not yet actionable |
| `Confirmed { phase, elapsed }` | Over budget on two consecutive samples |
| `Deferred { phase, elapsed }` | Confirmed, but a call is in progress and the ceiling is not yet reached (FR-029) |
| `Forced { phase, elapsed }` | Confirmed and the deferral ceiling is exceeded — act regardless |

`Deferred` is reported, never silent (FR-029), so a deferral is distinguishable from an
absence of fault.

## 4. AtChannel state

The lifecycle of one `AtCommander`'s link to its worker thread. Drives FR-003/4/36/37.

| State | Meaning | Behaviour of the next command |
|---|---|---|
| `Healthy` | Normal | Executes with its deadline |
| `Suspect` | A command timed out | Attempt resync first (drain + bare `AT`, short deadline) |
| `Dead` | Resync and reopen both failed | Fail immediately; the line needs recovery (FR-037) |

Transitions: `Healthy --timeout--> Suspect`; `Suspect --resync ok--> Healthy`;
`Suspect --resync fails, reopen ok--> Healthy`; `Suspect --both fail--> Dead`.
`Dead` is terminal for this process — by design, since the abandoned worker still holds
the port (research R2/R4).

## 5. RegistrationStatus (existing — extended)

Adds no field. `health()` becomes `health_at(now)` and newly consults the existing
`expires_at`, which was previously ignored entirely.

## 6. ServiceHealth (existing — extended)

| Field | Change |
|---|---|
| `registration_expired` | **new** — `expires_at` is in the past |

`can_answer()` gains `&& !registration_expired`. `blocked_reason()` inserts
"the registration has expired" between the attachment and registration cases: it is more
specific than "not registered" and outranks downstream symptoms (FR-017).

## 7. FaultIncident (existing — extended)

`supervise::sim_recovery`'s counters, extended for AT stalls.

| Element | Change |
|---|---|
| `AgentExitOutcome::AtStall` | **new** variant, counted against the **same** counter as `CsimFailure` — one physical fault, one remedy (research R7) |
| Give-up state | No longer terminal: becomes slow-retry (FR-030), alerting once per incident (FR-031) |

**Escalation ladder** (existing thresholds retained): repeated stalls → `AT+CFUN=0/1` SIM
reset → repeated resets → give up, alert once, then retry slowly until the line recovers
or a human intervenes.

## 8. Owned-pid registry (new, supervise only)

A set of pids the supervisor spawned, including transient `Command::output()` children,
so the PID-1 reaper can distinguish "my child, leave it" from "an orphan, reap it"
(FR-027, research R10). Decision function is pure: `should_claim(pid, owned) -> bool`.
