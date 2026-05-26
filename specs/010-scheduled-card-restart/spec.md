# Feature Specification: Scheduled Card Auto-Restart

**Feature Branch**: `010-scheduled-card-restart`  
**Created**: 2026-05-26  
**Status**: Draft  
**Input**: User description: "build a feature to auto restart the cards (with AT commands) at configurable (config.toml) time. It should happen at the scheduled time (cron like). Default it everynight at 1 AM. Do it for everycard, but one at a time, in order. Start the restart with a random jitter around the configured time. and also use a random jitter for the gap between cards."

## Clarifications

### Session 2026-05-26

- Q: When the scheduler reaches a card's turn and that card has an active SIP call in progress, what should the system do? → A: Defer the card to the end of the cycle's queue and retry after all other cards have been processed; if the call is still active at that point, skip the card for this cycle.
- Q: In which time zone is the cron expression evaluated? → A: System local time only; no `timezone` config field. DST transitions follow host TZ rules (skipped-hour: no run; fall-back hour: single fire).
- Q: If the bridge process is down across a scheduled occurrence, should it run a catch-up cycle on startup? → A: No catch-up. If the bridge starts past the most recent cron occurrence, wait for the next future occurrence; no `catchup_window` config field.
- Q: What happens when a manual `card restart` is issued during an in-progress scheduled cycle? → A: The manual command runs immediately. If the target slot has not yet been processed in the cycle, the scheduler marks it as "already-restarted-by-manual" and skips it when its turn arrives. If the target slot is currently being processed by the scheduler, the manual command is rejected with a precise error.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Nightly Auto-Restart of All Cards (Priority: P1)

A homelab operator runs the GSM-SIP bridge with multiple modem modules. Without any manual action, every night the bridge automatically restarts each modem card in turn during a quiet, off-peak window. Each card receives a fresh modem reset (via AT commands), clearing any accumulated firmware/registration drift, and the bridge resumes normal operation. The next morning the operator finds the bridge healthy and the previous night's restart cycle recorded in the logs.

**Why this priority**: This is the core value of the feature. Modems that run continuously for days are known to drift into degraded states (stuck registration, slow AT responses, dropped re-registration). A nightly preventive restart sharply reduces mid-day failures and operator interventions. Without this, the feature does not exist.

**Independent Test**: With at least two cards connected and the feature enabled with a short test schedule (e.g., set the cron expression to fire in 2 minutes), wait for the trigger and verify that each card is restarted one at a time in ascending slot order, that a randomized gap separates consecutive cards, and that all cards return to ready state with the cycle outcome logged.

**Acceptance Scenarios**:

1. **Given** the bridge is running with three cards in ready state and the schedule is `0 1 * * *` (1 AM nightly) with default jitter, **When** the system clock reaches the next 1 AM tick (plus the random start jitter), **Then** the system begins restarting cards starting with the lowest slot number.
2. **Given** a scheduled cycle has started, **When** the first card completes its restart, **Then** the system waits for a randomized inter-card gap before beginning the second card's restart — no two cards are ever being restarted simultaneously.
3. **Given** a scheduled cycle is in progress, **When** all cards have been processed, **Then** the system logs a cycle-complete summary (cards restarted, cards skipped, total duration) and returns to normal operation.
4. **Given** the bridge has just started and the configured cron time has already passed for today, **When** scheduling is initialized, **Then** the system waits for the next future cron occurrence and does not run a catch-up cycle.
5. **Given** a card is in `Recovering` or `GivenUp` state when its turn comes in a cycle, **When** the scheduler reaches that slot, **Then** the system skips that card, logs the skip reason, and proceeds to the next card without aborting the cycle.
6. **Given** a card has an active SIP call when its turn comes, **When** the scheduler reaches that slot, **Then** the system defers that card to the end of the cycle's queue, logs the deferral with reason, and proceeds to the next card (the active call is not interrupted).
7. **Given** a deferred card's call has ended by the time the scheduler retries it at the end of the cycle, **When** the retry runs, **Then** the card is restarted normally and recorded as a successful scheduled restart for this cycle.
8. **Given** a deferred card still has an active call when the scheduler retries it at the end of the cycle, **When** the retry attempt is made, **Then** the card is skipped for this cycle, the skip is logged with reason, and the cycle completes (the card will be eligible again on the next scheduled cycle).

---

### User Story 2 - Operator Configures Schedule and Jitter (Priority: P2)

An operator wants to move the nightly restart to a different time (for example, 3 AM instead of 1 AM), broaden the start jitter window, or temporarily disable the feature during a maintenance window. They edit the bridge's TOML configuration file, restart the bridge, and the new schedule takes effect immediately — without any code change.

**Why this priority**: Different deployments have different quiet windows, peering schedules, and risk tolerances. Without the ability to tune the schedule and jitter, the feature would only fit one operator's environment. Configurability is required for the feature to be broadly useful, but the default works for most users — so this is P2.

**Independent Test**: Edit the `[scheduled_restart]` section of `config.toml` to set a different cron expression (e.g., `*/5 * * * *` for every 5 minutes during testing) and adjusted jitter values, restart the bridge, and verify that the next cycle fires at the new time and uses the new jitter parameters as reflected in startup logs.

**Acceptance Scenarios**:

1. **Given** the operator sets `enabled = false` under `[scheduled_restart]` in `config.toml`, **When** the bridge starts, **Then** no scheduled restart cycle ever fires and the startup log clearly states that scheduled restart is disabled.
2. **Given** the operator changes the cron expression to a new valid value, **When** the bridge starts, **Then** the startup log shows the next scheduled occurrence in human-readable form (e.g., `next scheduled restart: 2026-05-27 03:00 +/- 10m`) and the cycle fires at that time.
3. **Given** the operator sets a malformed or invalid cron expression, **When** the bridge starts, **Then** the bridge logs a clear configuration error pointing at the offending field, leaves scheduled restart disabled, and continues normal operation (the rest of the bridge stays up).
4. **Given** the operator increases the start jitter from default ±10 minutes to ±30 minutes, **When** several scheduled cycles run over multiple nights, **Then** observed start times are distributed across the wider window.
5. **Given** the operator omits the `[scheduled_restart]` section entirely from `config.toml`, **When** the bridge starts, **Then** the feature is enabled with built-in defaults (1 AM nightly, ±10m start jitter, 30s ± 15s inter-card gap) and the defaults appear in the startup log.

---

### User Story 3 - Visibility into Scheduled Restart Activity (Priority: P3)

An operator returns in the morning and wants to confirm that the previous night's restart cycle ran cleanly, see which cards were restarted, how long each took, and whether any were skipped. They can answer all of these questions from the bridge's structured logs and (where available) metrics — without running any extra diagnostics.

**Why this priority**: Operators need confidence that an automated, unattended activity actually ran. Silent successes are nearly as bad as silent failures because they erode trust over time. Visibility is essential for the feature to be production-grade, but the feature is still functionally useful (P1) before this story is delivered — so this is P3.

**Independent Test**: After at least one scheduled cycle has completed, grep the bridge's log file for the cycle identifier and verify it contains: a cycle-start entry, one entry per card with slot/start-time/duration/outcome, and a cycle-complete summary. If metrics are emitted, scrape the metrics endpoint and verify a per-card scheduled-restart counter has incremented.

**Acceptance Scenarios**:

1. **Given** a scheduled cycle has just completed, **When** the operator inspects the structured log output, **Then** every card's restart outcome (success, failure, skipped-with-reason) is recorded with slot, scheduled time, actual start time, and duration.
2. **Given** the system emits Prometheus-style metrics (per the existing observability feature), **When** a scheduled cycle completes, **Then** counters for `scheduled_restart_total`, `scheduled_restart_success_total`, `scheduled_restart_skipped_total`, and `scheduled_restart_failed_total` are incremented appropriately, labeled by slot.
3. **Given** a scheduled cycle is currently in progress, **When** the operator inspects logs in real time, **Then** they can see which card is currently being restarted and which cards remain in the queue.

---

### Edge Cases

- **Daylight-saving / time-change transitions**: The schedule MUST fire based on system local time. On a "spring forward" night, if the configured time falls inside the skipped hour, the cycle does not run that night; on a "fall back" night, it runs at the first occurrence and does not double-fire.
- **Process restart mid-cycle**: If the bridge process is restarted while a scheduled cycle is in progress, the in-progress cycle is abandoned (no resumption); the next future cron occurrence is the next time a cycle will run.
- **Scheduled time arrives while a previous cycle is still running**: The new trigger is dropped with a warning log entry; only one scheduled cycle may be active at a time.
- **All cards are unhealthy at trigger time**: The cycle starts, skips every card with reasons, logs a cycle-complete summary with zero successful restarts, and the system continues running.
- **A single card's restart hangs or exceeds expected duration**: The restart inherits the same per-card timeout used by the existing manual restart path; on timeout, the card is marked failed for this cycle and the scheduler proceeds to the next card.
- **A card's restart fails during the cycle**: The failure is logged for that card; the cycle continues with the remaining cards.
- **System time jumps forward (NTP correction)** past the configured time: The cron evaluator treats this as a normal scheduled tick and fires once; it does not fire repeatedly to "catch up" on missed historical occurrences.
- **System time jumps backward (NTP correction)** to before a recently-fired cycle: A second cycle MUST NOT fire for the same logical occurrence; the scheduler tracks the last-fired timestamp to suppress duplicates within a reasonable window.
- **Hot-plug arrives during a cycle**: A newly-detected card is not added to the in-progress cycle; it will be eligible starting from the next cycle.
- **Manual `card restart` issued during a cycle**: If the slot has not yet been processed, the manual restart runs and the scheduler skips that slot when its turn arrives (logged as `already-restarted-by-manual`). If the slot is currently being processed by the scheduler, the manual command is rejected with a clear error. If the slot has already been processed earlier in the cycle, the manual command runs normally.
- **`GivenUp` card is restarted by the cycle and succeeds**: The card's give-up state is reset (consistent with manual `card restart`), allowing it to re-enter normal operation.

## Requirements *(mandatory)*

### Functional Requirements

**Scheduling**

- **FR-001**: The bridge MUST support a `[scheduled_restart]` section in the TOML configuration file with the following fields: `enabled` (bool), `cron` (string, 5-field cron expression), `start_jitter_seconds` (non-negative integer), `inter_card_gap_seconds` (non-negative integer), `inter_card_gap_jitter_seconds` (non-negative integer).
- **FR-002**: When the `[scheduled_restart]` section is omitted entirely, the bridge MUST apply built-in defaults: `enabled = true`, `cron = "0 1 * * *"` (1 AM nightly), `start_jitter_seconds = 600` (±10 minutes), `inter_card_gap_seconds = 30`, `inter_card_gap_jitter_seconds = 15`.
- **FR-003**: When `enabled = false`, the bridge MUST never trigger a scheduled cycle, and MUST log clearly at startup that the feature is disabled.
- **FR-004**: The bridge MUST validate the cron expression at startup; on an invalid expression, it MUST log a configuration error with the offending value, keep the rest of the bridge running, and leave scheduled restart disabled for that process lifetime.
- **FR-005**: The scheduler MUST fire each cron occurrence exactly once per logical tick, even across small system-time perturbations (e.g., NTP corrections within ±60 seconds of the scheduled time).
- **FR-005a**: The cron expression MUST be evaluated in the host's system local time. The configuration schema MUST NOT include a `timezone` field. On DST "spring forward" nights, if the scheduled time falls inside the skipped hour, no cycle runs that night. On DST "fall back" nights, the cycle fires exactly once at the first occurrence of the scheduled wall-clock time.
- **FR-006**: For each scheduled cron tick, the actual cycle start time MUST be offset from the cron tick by a uniform random value in the range `[-start_jitter_seconds, +start_jitter_seconds]`.

**Cycle Execution**

- **FR-007**: A scheduled cycle MUST iterate over all currently-known cards in ascending slot order and attempt to restart each one in turn.
- **FR-008**: Within a cycle, at most one card MUST be undergoing a restart at any given time; the next card's restart MUST NOT begin until the previous card's restart has completed (succeeded, failed, or timed out).
- **FR-009**: After each per-card restart completes, before starting the next card, the scheduler MUST wait for `inter_card_gap_seconds` plus a uniform random value in `[-inter_card_gap_jitter_seconds, +inter_card_gap_jitter_seconds]` (clamped at zero).
- **FR-010**: The per-card restart action MUST use the same teardown-and-reinitialize sequence (via AT commands on the modem) as the manual `card restart` CLI subcommand from feature 009; behavior, timeouts, and state transitions MUST be identical.
- **FR-011**: When a card's turn arrives, the scheduler MUST skip it and proceed to the next when that card is in any non-ready lifecycle state (`Initializing`, `Recovering`, `GivenUp`); the skip reason MUST be logged.
- **FR-011a**: When a card's turn arrives and the card has an active SIP call, the scheduler MUST defer that card to the end of the current cycle's queue (not skip immediately) and proceed to the next card. The deferral MUST be logged with slot, cycle identifier, and reason.
- **FR-011b**: After all non-deferred cards have been processed in a cycle, the scheduler MUST retry each deferred card once, in the order it was deferred, applying the same inter-card gap (with jitter) between retries. If a deferred card still has an active call at its retry moment, it MUST be skipped for this cycle and a skip entry logged; otherwise it MUST be restarted using the standard per-card restart action.
- **FR-012**: A scheduled restart that completes successfully on a card whose previous state was `GivenUp` MUST reset that card's give-up state (consistent with manual restart behavior in feature 009).
- **FR-013**: A failed per-card restart inside a cycle MUST NOT abort the cycle; remaining cards MUST still be attempted in order.
- **FR-014**: If a previous scheduled cycle is still in progress when the next cron tick fires (including the post-tick jitter offset), the scheduler MUST drop the new trigger, log a warning identifying both the in-progress and dropped cycles, and continue.
- **FR-014a**: A manual `card restart` command (from feature 009) issued while a scheduled cycle is in progress MUST be handled as follows: (1) if the target slot has not yet been processed in the current cycle, the manual restart proceeds immediately and the scheduler MUST record that slot as `skipped-already-restarted-by-manual` when its turn arrives in the cycle; (2) if the target slot is currently being restarted by the scheduler, the manual command MUST be rejected with a clear error message identifying the in-progress cycle and slot, and the CLI MUST exit with a non-zero status code; (3) if the target slot has already been processed earlier in the cycle, the manual command MUST proceed normally.
- **FR-015**: If the bridge starts after the most recent past cron occurrence, the scheduler MUST NOT run a catch-up cycle; it MUST wait for the next future occurrence. The configuration schema MUST NOT include a catch-up window field.

**Observability**

- **FR-016**: At startup, the bridge MUST log the scheduled-restart configuration (enabled flag, cron expression, jitter values, and the next computed occurrence in human-readable local time).
- **FR-017**: The scheduler MUST emit a cycle-start log entry containing a unique cycle identifier, the scheduled cron tick time, the actual (jittered) start time, and the list of slot identifiers to be processed.
- **FR-018**: For each card-attempt inside a cycle (including deferred retries), the scheduler MUST emit a structured log entry containing slot, cycle identifier, attempt type (`initial` or `deferred-retry`), action outcome (`success`, `failed`, `deferred`, `skipped`), reason (for `deferred`, `skipped`, or `failed`), and elapsed duration.
- **FR-019**: At cycle end, the scheduler MUST emit a cycle-complete log entry with totals: cards processed, succeeded, failed, deferred-then-recovered (succeeded on retry), skipped, and total cycle duration.
- **FR-020**: Each per-card scheduled restart outcome MUST be reflected in the bridge's metrics surface (per the existing observability feature) as counters labeled by slot and outcome.

### Key Entities

- **Restart Schedule**: The configuration that drives the scheduler. Attributes: enabled flag, cron expression, start jitter range (seconds), inter-card gap base (seconds), inter-card gap jitter range (seconds). Read once at startup from `config.toml` and held in memory for the process lifetime.
- **Scheduled Cycle**: A single end-to-end execution triggered by one cron occurrence. Attributes: cycle identifier, scheduled cron-tick time, actual jittered start time, ordered list of slots to process, deferred-retry queue of slots that had an active call on their first attempt, per-card outcomes, cycle-end time. Exists only in memory and in logs.
- **Scheduled Restart Event**: One card-level outcome within a cycle. Attributes: cycle identifier, slot, planned position in the cycle, attempt type (`initial` or `deferred-retry`), actual restart start time, restart duration, outcome (`success` / `failed` / `deferred` / `skipped`), failure / defer / skip reason.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: With default configuration, all healthy connected cards are automatically restarted exactly once per 24-hour period inside the configured jitter window, with zero operator action.
- **SC-002**: For a host with up to 8 cards (the supported maximum from prior features), a full scheduled cycle completes within 10 minutes from the actual cycle start time, under normal modem response conditions.
- **SC-003**: No two cards are observed to be mid-restart at the same instant during any scheduled cycle — exactly one card transitions through its restart sequence at a time.
- **SC-004**: Over any 7 consecutive nightly cycles with default jitter, the observed start times of the first card span a range consistent with the configured ±10 minute jitter (i.e., not all firing at the exact same minute).
- **SC-005**: 100% of scheduled cycles produce a complete log trail — one cycle-start entry, one per-card outcome entry per processed card, and one cycle-complete summary — verifiable by log inspection.
- **SC-006**: Operators can disable the feature with a single configuration change (`enabled = false`) and confirm via startup logs that no cycle will fire during that process lifetime.
- **SC-007**: A misconfigured cron expression does not prevent the bridge from starting; the bridge remains fully operational with scheduled restart disabled and a clear error logged.
- **SC-008**: Calls in progress on cards not yet processed in the current cycle are not interrupted by the scheduler — an active call on slot N has zero impact on cards M ≠ N being restarted.
- **SC-009**: After a card's give-up state is reset by a successful scheduled restart, the card is fully usable again (registered, able to bridge calls) without any operator action.

## Assumptions

- This feature builds on top of feature `009-gsm-resiliency-cli`, which already provides the per-card teardown-and-reinit sequence used by the manual `card restart` subcommand. The scheduler reuses that exact mechanism; it does not introduce a new restart code path.
- "config.toml" refers to the bridge's existing TOML configuration file; this feature adds a new `[scheduled_restart]` section to it and does not introduce a separate config file.
- "Cron-like" is interpreted as standard 5-field cron syntax (`minute hour day-of-month month day-of-week`) using system local time. Seconds-precision and extended cron features (e.g., `@reboot`, year fields) are out of scope for v1.
- Default schedule `0 1 * * *` corresponds to 1 AM in the host's local time zone, matching the user's stated default.
- Default start jitter is ±10 minutes (`start_jitter_seconds = 600`), and default inter-card gap is 30 seconds base with ±15 seconds jitter — values chosen to produce a clearly randomized but predictable window for a homelab with up to 8 cards.
- "One at a time, in order" is interpreted as ascending slot index (slot 0, then slot 1, etc.), matching the slot ordering established in feature 004-multi-card-support and persisted in feature 009.
- When a card has an active SIP call at the moment its turn arrives in a cycle, the scheduler defers that card to the end of the cycle's queue rather than force-dropping the call; if the call is still active at the deferred-retry moment, the card is skipped for this cycle and will be eligible again on the next nightly cycle. Rationale: the value of an off-peak restart is convenience; interrupting a live call is more disruptive than waiting one more night, but short calls that end mid-cycle should not cost the card its restart for the night.
- Scheduled restart is independent of and complementary to the auto-recovery feature from 009. Auto-recovery still handles unexpected failures throughout the day; the scheduled restart is a preventive measure during the quiet window.
- Cards added via hot-plug after a cycle has already begun are not added to that in-flight cycle; they are picked up on the next cycle.
- The bridge process is assumed to remain continuously running across the scheduled time. Missed schedules due to process downtime are not retroactively executed.
- At most 8 modem modules are supported per host (inherited constraint from features 004 and 008).
- Metrics emission depends on the existing observability surface from feature 005; if that feature is disabled or absent, only the structured logs (FR-017 through FR-019) are required.
