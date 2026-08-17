# Phase 1 Contracts: observable surfaces

**Feature**: 039-at-stall-watchdog | **Date**: 2026-08-17

This feature exposes no network API. Its contracts are the four surfaces other things
depend on: the status CLI, the metrics endpoint, the log markers the supervisor greps,
and the process exit code. Each is consumed by something outside the changing code, so
each is pinned here.

## C1. `vowifi-status` / `volte-status` CLI output

Existing fields keep their names and meaning. Changed and added lines:

```text
Line 0 (card ec20-11):
  VoWiFi registration (Agent A):
    state: Registered
    registered_at: 1786891429
    expires_at: 1786895029
    expires_in: 2841s                      # NEW - derived, negative and marked when lapsed
    gm_connection: up
    last_failure: none
    can_answer: true
    blocked_reason: -                      # NEW value possible: see below
```

When lapsed:

```text
    expires_in: -8412s (LAPSED)
    can_answer: false
    blocked_reason: the registration has expired
```

**`blocked_reason` priority order** (first match wins) — the expiry case is new and is
inserted second:

1. `the network attachment is down`
2. `the registration has expired`   ← **NEW**
3. `not registered`
4. `the carrier signaling connection is down`
5. `the PBX registration is down`
6. `a call is already in progress`

**Compatibility**: additive. Existing consumers parsing the old fields are unaffected.

## C2. Prometheus metrics

Existing gauges keep their names. Added:

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `gsm_sip_bridge_vowifi_registration_expires_in_seconds` | gauge | `module` | Seconds until the registration lapses; **negative once lapsed** |
| `gsm_sip_bridge_agent_dispatch_stall_seconds` | gauge | `agent`, `module` | How long the activity has been over-budget; 0 when healthy |

**Changed behaviour of existing gauges** (no rename):

- `gsm_sip_bridge_agent_up{agent="ims"}` — now reflects *dispatch-loop progress*. A
  stalled agent stops heartbeating, its report goes stale, and this reads 0. Previously
  it read 1 for the entire 2h45m outage.
- `gsm_sip_bridge_vowifi_registered` — zeroed for a stale agent by the existing
  staleness path, which now actually triggers.

**Wire format note**: the agent reports the **absolute** expiry timestamp, and the
daemon computes the countdown at scrape time. This matches the existing
`agent_up`/`last_report_seconds` pattern and avoids a report every second.

## C3. Log markers (consumed by the supervisor)

The supervisor greps the agent's redirected stderr after an exit to classify the
failure. Existing marker for CSIM failure is unchanged; one marker is added.

**AT stall marker** — must be stable, single-line-greppable, and emitted exactly once
before exit:

```text
watchdog: the dispatch loop has made no progress
```

Emitted with structured fields: `activity`, `phase`, `stalled_secs`, `budget_secs`,
`last_at_command`. `last_at_command` is the single most valuable diagnostic — its
absence is what made the original incident take live forensics to diagnose.

**Deferral marker** (FR-029, not a failure):

```text
watchdog: recovery deferred while a call is in progress
```

**Guarantee**: logging is synchronous to stderr (no non-blocking writer), so the marker
is durable before the process exits.

## C4. Process exit code

| Code | Meaning |
|---|---|
| `70` | Watchdog-confirmed stall; the supervisor should restart this line |

Any exit causes a restart today, so this is advisory for humans and for classification;
the supervisor's decision is driven by the log marker (C3), matching how CSIM failures
are already classified.

## C5. Configuration

One setting added (FR-034). Budgets and deadlines are deliberately **not** configurable
(FR-033).

| Setting | Default | Meaning |
|---|---|---|
| `[vowifi].watchdog_recovery_enabled` | `true` | When false, stalls are still detected, reported and visible in all health surfaces (FR-035), but the process is not exited — for preserving a wedged line for diagnosis |

## C6. `AtCommander` internal API

Not a public contract, but pinned because its stability is what keeps this change
reviewable: **the public method set and signatures do not change.** `open`,
`open_with_timeout`, `from_stream`, `send_command`, `read_line_raw`, `query_imsi`,
`query_imei`, `transmit_apdu`, `reboot`, `radio_restart` all keep their existing
signatures and error types, so the 30+ call sites are untouched.

New error text is reused where callers already match on it: a timeout still produces
`"AT command timeout"`, which `sim_recovery`'s existing greps and callers' retry logic
already handle.
