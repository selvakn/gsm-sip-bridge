# Research: Discord Alerts for Critical Events

All items below were resolved by reading the current codebase (post-rebase onto
`main`, which brought in `specs/021-entrypoint-supervise-rust`'s `supervise`
module) rather than external research — this feature extends existing,
already-decided patterns rather than introducing new ones.

## R1: Where does each alert category's signal already live?

**Decision**: Reuse four different existing signal sources rather than
building new detection logic:

| Category | Signal source | Concurrency world |
|---|---|---|
| SMS incoming (existing) | `sms::record_and_forward` | async (tokio) |
| Missed call | `modules::mod::record_call_end(..., "missed")` (GSM/circuit-switched) writing `CallStatus::Missed` to the `calls` table | async (tokio) |
| Module lifecycle failure — SIM/discovery (GSM) | `modules::discovery::SimStatus::{Absent,Unreadable}` | async (tokio) |
| Module lifecycle failure — AT worker unresponsive | per-module last-successful-AT-command timestamp (new, in the same async card loop) | async (tokio) |
| Module lifecycle failure — SIM/discovery (VoWiFi/VoLTE) | `supervise::sim_recovery::Action::GiveUpForThisIncident`, computed in `supervise::orchestrate`'s per-line agent-supervision thread | sync (`std::thread`) |
| IMS/SIP registration loss | `AgentState.registered: Option<bool>`, already reported by each VoWiFi/VoLTE line agent via `AgentReport` and ingested by `metrics::ingest` | async (tokio, central daemon) |
| VoWiFi tunnel failure | `AgentState.tunnel_up: Option<bool>`, same `AgentReport`/`metrics::ingest` pipe | async (tokio, central daemon) |

**Rationale**: `control::protocol::AgentState` already carries `registered:
Option<bool>` and `tunnel_up: Option<bool>` — every VoWiFi/VoLTE line agent
already reports these at `[metrics].agent_report_interval_seconds` cadence
(default 10s), and `metrics::server::refresh_agent_liveness` already derives
`AGENT_UP`/`VOWIFI_REGISTERED` gauges from the same reports. This means
registration-loss and tunnel-failure detection do **not** need to reach into
`supervise`'s synchronous per-line threads at all — they can be computed
entirely in the existing central-daemon ingest pipeline, the same place SMS
alerting and the missed-call/module-lifecycle (GSM) categories already run.
Only the SIM-recovery-exhausted case is genuinely inside `supervise`'s
synchronous world, because CSIM-failure counting only happens inside the
per-line agent-supervision thread in `supervise::orchestrate`.

**Alternatives considered**: Building a new health-polling loop inside
`supervise` for registration/tunnel status was rejected — it would duplicate
the reporting pipe that already exists and cross a process boundary
(`supervise::orchestrate` supervises the agent *process*; it does not see
inside it) for no benefit.

## R2: The `GiveUpForThisIncident` signal exists but is not acted on

**Finding**: `supervise::sim_recovery::Action::GiveUpForThisIncident` is a
real enum variant (`sim_recovery.rs:54`), reached when
`IncidentCounters::observe` sees `MAX_SIM_RESETS` (5) already used this
incident. However, at the only call site
(`supervise::orchestrate.rs:868`, inside the per-line `vowifi-ims-agent`
supervision loop), the result of `observe()` is matched only against
`Action::ResetSim`; `GiveUpForThisIncident` (and the "no action needed" case)
fall through silently — the loop just sleeps 5s and retries forever, exactly
as if recovery had not been attempted. Nothing observable happens when
recovery is exhausted today.

**Decision**: This feature adds the missing `match` arm at that call site: on
`GiveUpForThisIncident`, emit a `tracing::error!` (module id + incident
context) and invoke the alert dispatcher (R3) for the module-lifecycle-failure
category. This is in-scope wiring required by FR-004, not an unrelated fix —
without it, FR-004 has no signal to alert on for the VoWiFi/VoLTE SIM path.

**Alternatives considered**: Redefining "exhausted" as a duration threshold
(matching R4's approach for tunnel/registration) was rejected for this one
case, since a precise, already-modeled "gave up" signal exists — using it is
simpler than inventing a parallel timer for the one category where a
discrete exhaustion signal already exists in the type system.

## R3: Bridging a synchronous call site to the async Discord client

**Decision**: Reuse the exact pattern already documented in
`sms::record_and_forward`'s doc comment: a purely synchronous caller
(`vowifi::mod`'s accept loop, per that comment) builds its own small
dedicated `tokio::runtime::Runtime` once and spawns onto it, rather than the
async caller's `Handle::current()` used by `modules::mod`. `supervise::
orchestrate::run` is exactly such a synchronous caller (no ambient Tokio
context — `Runtime::run` is invoked straight from `main.rs` as a blocking
subcommand). It will build one `Runtime` at startup (alongside loading
`AlertsConfig`), wrap it in an `Arc` next to the existing `runner`/`started`/
`shutting_down` state already threaded into every per-line supervision
thread, and call `.spawn(...)` on it wherever an alert needs to fire.

**Rationale**: This is not a second concurrency model for equivalent work
(which `specs/021-entrypoint-supervise-rust`'s plan correctly forbids for
long-lived supervisory loops) — it is a single, rare, fire-and-forget leaf
call (a Discord POST), and the codebase already has a named, precedented way
to make that same kind of call from synchronous code. Reusing it keeps one
`DiscordClient`/alert-payload implementation shared by every category.

**Alternatives considered**: Enabling reqwest's `blocking` feature and
calling it directly from the sync thread was rejected — it would introduce a
second HTTP-client code path (blocking vs. async) for what should be one
`DiscordClient` shared by all five categories (SMS + 4 new), duplicating
retry/backoff/payload logic that already exists and is already tested
(`sms::discord::DiscordClient`).

## R4: Duration-based "recovery exhausted" thresholds, and how to test them

**Decision**: For the two categories with no discrete "gave up" signal
(VoWiFi tunnel failure, IMS/SIP registration loss — both sit behind
unbounded auto-restart loops per the spec's Clarifications Q8/Q9), track a
plain `unhealthy_since: Option<Instant>` per (category, module/line id),
set the first time the signal is observed unhealthy and cleared the moment
it is observed healthy again. An event fires once
`unhealthy_since.elapsed() >= threshold` (5 minutes, configurable). This is
the same shape `metrics::ingest::evaluate_liveness` already uses for agent
staleness (`record.last_report.elapsed() <= staleness_threshold`), just
keyed on health instead of report recency.

**Testability**: Following the same precedent, tests construct
`unhealthy_since` in the past directly (`Instant::now() -
Duration::from_secs(301)`) rather than sleeping — this is real `Instant`
arithmetic, not a mock, satisfying Constitution Principle I without a 5-minute
test run.

**Alternatives considered**: A background timer/ticker per line was
rejected in favor of evaluating on each incoming `AgentReport` (registration/
tunnel) or on each supervision-loop iteration (SIM), mirroring
`metrics::server`'s existing "evaluated on every scrape, not on a timer"
design for agent liveness — one fewer moving part (Constitution V).

## R5: No new SQLite table for the four new categories

**Decision**: Only the existing SMS category keeps its DB persistence
(`sms` table, unchanged). The four new categories are logged via
`tracing::warn!`/`tracing::error!` (structured, with category/module/line
fields) and counted via new Prometheus counters (R6) — no new table, no
schema migration.

**Rationale**: The spec's own Assumptions rule this out explicitly:
"Historical alert data... is not required beyond what is already logged/
persisted for SMS; this feature does not require a new UI." FR-011/FR-012
only require every event to be logged and reflected in metrics, which
structured tracing + Prometheus already satisfy — a new table would be
unused complexity (Constitution V).

## R6: New Prometheus metrics

**Decision**: Add two new metric families in `metrics/mod.rs`, following the
existing `SMS_FORWARDED_TOTAL`-style `CounterVec` convention:

- `gsm_sip_bridge_critical_alerts_total{category, outcome}` — counter,
  incremented once per alert *decision* (`outcome` ∈
  `sent|suppressed|skipped|failed`; "suppressed" covers a
  still-continuously-unhealthy tick that intentionally does not re-alert
  per FR-013).
- `gsm_sip_bridge_critical_event_active{category, module}` — gauge, 1 while
  a category is in its alerted (post-threshold) unhealthy state for that
  module/line, 0 once recovered. Mirrors the existing `AGENT_UP` gauge shape
  so Grafana panels can reuse the same query patterns.

**Alternatives considered**: A single high-cardinality metric keyed by raw
event description was rejected — categories and outcomes are both small,
closed enums (consistent with `control::protocol::ObservedEvent`'s existing
"closed enum, not free string" convention for bounded label cardinality).

## R7: Config schema shape

**Decision**: New `[alerts]` top-level section, plus one `[alerts.<category>]`
sub-table per category (`sms`, `module_lifecycle`, `registration_loss`,
`tunnel_failure`, `missed_call`), following the existing `[sms]`/
`[scheduled_restart]` nested-table precedent in `config/mod.rs`. `[alerts]`
itself carries the shared default `discord_webhook_url` (a `Secret<String>`,
matching `SmsConfig::discord_webhook_url`); each `[alerts.<category>]` table
carries `enabled: bool` and an optional `discord_webhook_url` override, plus
category-specific tunables where the spec calls for them
(`at_worker_unresponsive_sec = 60`, `tunnel_unhealthy_sec = 300`,
`registration_unhealthy_sec = 300`). The existing `[sms].discord_webhook_url`
/`[sms].enabled` keys are kept for backward compatibility and now feed
`[alerts.sms]`'s defaults if `[alerts.sms]` is absent, satisfying FR-001's
"bring existing behavior under the same mechanism" without breaking existing
deployments' `config.toml` files.

**Alternatives considered**: Flattening everything into `[alerts]` with
prefixed keys (`module_lifecycle_enabled`, `module_lifecycle_webhook_url`,
...) was rejected as harder to read/maintain than one sub-table per category,
and inconsistent with how every other multi-attribute section in this config
(`[scheduled_restart]`, `[resilience]`) is already structured.
