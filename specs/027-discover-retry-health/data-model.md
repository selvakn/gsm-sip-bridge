# Phase 1 Data Model: Discovery Retry & Missing-Line Health Reporting

This feature adds no new persistent storage or database entities — everything below is either an extension of an existing in-process/on-disk type (`gsm-sip-bridge/src/line/mod.rs`, `gsm-sip-bridge/src/vowifi/discovery.rs`) or a new one shaped exactly like its closest existing sibling (`AlertCategory`/`CategoryAlertConfig` in `alerts/mod.rs` and `config/mod.rs`; `GaugeVec` in `metrics/mod.rs`).

## Extended: `Rejection` / `FailedLine::reason` (`gsm-sip-bridge/src/line/mod.rs`)

Add one variant to the existing `Rejection` enum:

- `Rejection::NotFound` → reason string `"not_found"` — a configured override (`modem_port`, `modem_serial`, or `pcsc_reader`) that matched no probed device on the most recent discovery pass, distinct from `NoAtPort` (device found, but nothing answers AT) and `SimAbsent`/`SimLocked`/`SimUnreadable` (device found, SIM problem).

`FailedLine.card_id` is populated differently for this variant, since a never-discovered device has no USB-derived card id to report: use the override's own configured identifier instead — the `modem_port` path string for a modem pin, or the existing `pcscN` synthetic id (already used for pcsc overflow in `resolve_lines`) for a `pcsc_reader` line. This keeps `FailedLine` identifiable back to the exact `config.toml` entry (FR-006) without inventing a new identity scheme.

## New: per-line retry/discovery state

Not a new persisted type — a transient, in-memory state `supervise::orchestrate`'s retry loop tracks per still-missing configured override between discovery attempts, for the life of one bounded retry window:

| Field | Meaning |
|---|---|
| override identity | The same identifier used in `FailedLine.card_id` above (modem_port path / modem_serial / `pcscN`) |
| first_attempted_at | When this override was first found missing (start of its retry window) |
| attempts | How many discovery passes have been tried for it so far (informational/logging only) |
| outcome | `StillRetrying` \| `Resolved` \| `TerminallyFailed` |

Once an override reaches `Resolved` or `TerminallyFailed`, this state is done — `Resolved` means it's now a normal `ResolvedLine` in the resolution file like any other; `TerminallyFailed` means a `FailedLine{reason: "not_found", ..}` (or whatever rejection was last observed) is written to the resolution file's `failed` list and stays there for the rest of the process's life (per the startup-only clarification — no further retries after the window elapses).

## Extended: `LineResolution` / `LineTableResult` consumers

No shape change to `LineResolution`/`LineTableResult` themselves (`lines: Vec<ResolvedLine>`, `failed: Vec<FailedLine>`) — both already carry exactly the information needed. What changes is who reads `failed`:

- `vowifi::print_status` (`vowifi/mod.rs:1826`) — today ignores `failed` entirely; must print each entry (FR-006/FR-007), and must distinguish `"not_found"` from the SIM-related reasons in its own wording (FR-007) since the existing `reason` strings already carry that distinction.
- `commands::healthcheck::evaluate` (`healthcheck.rs:166`) — today never reads `failed`; must treat a `not_found` entry that has gone terminal (see below) as a health fault (FR-008), while a `not_found` entry still inside its retry window must **not** yet count as unhealthy (that would make ordinary startup churn look like an outage — this needs the "terminal vs. still-retrying" distinction from the in-memory state above to be recorded somewhere `healthcheck` — itself a separate, later `gsm-sip-bridge healthcheck` process invocation, not the same process as the retry loop — can read). The simplest way to make that distinction available across process boundaries is for the resolution file to only gain a `not_found` `FailedLine` entry once the retry window has actually elapsed (i.e., `supervise` doesn't write anything to `failed` for an override still mid-retry) — so "present in `failed` with reason `not_found`" always means *terminal*, and `healthcheck`/`vowifi-status` need no separate retry-window bookkeeping of their own.

## New: `AlertCategory::LineDiscoveryFailed` (`gsm-sip-bridge/src/alerts/mod.rs`)

A new variant alongside `RegistrationLoss`/`TunnelFailure`/`Sms`/`ModuleLifecycle`/`MissedCall`, with the same `"line_discovery_failed"`-style stable string identity those use.

## Extended: `AlertsConfig` (`gsm-sip-bridge/src/config/mod.rs`)

- `AlertsConfig.line_discovery_failed: CategoryAlertConfig` — same shape as `registration_loss`/`tunnel_failure`'s field (`enabled` + optional webhook override), so it composes with the existing `default_webhook_url` fallback and per-category enable/disable an operator already expects.
- No new *threshold* struct (unlike `RegistrationLossThresholds`/`TunnelFailureThresholds`, which measure "unhealthy for N seconds" against a continuously-reported live signal): this category's "threshold" is the retry window itself (R3/R5 in research.md), which is a property of the retry loop, not of a duration derived from repeated `AgentReport`s. The retry window's duration lives with the retry loop's own config (see below), not duplicated into `AlertsConfig`.

## New: retry window configuration

A single new duration setting — where exactly it lives (a new `[vowifi]`/`[discover]` key vs. a constant, and its default value) is an implementation detail for `/speckit-tasks` to place, not fixed here; the data model only requires that *some* bounded, on-the-order-of-minutes duration exists and is readable by `supervise::orchestrate`'s retry loop (per spec Assumptions).

## Revised (implementation-time simplification): no new metric needed

`gsm-sip-bridge/src/alerts/mod.rs`'s `dispatch()` already sets a generic
`CRITICAL_EVENT_ACTIVE{category, module}` gauge (`gsm_sip_bridge_critical_event_active`)
to `1`/`0` for every `AlertCategory` in its `tracks_ongoing_health` list, driven directly
by the `Failure`/`Recovered` events `AlertContext::fire` already dispatches. Adding
`AlertCategory::LineDiscoveryFailed` to that list gets FR-012/SC-006 for free —
`gsm_sip_bridge_critical_event_active{category="line_discovery_failed", module="<id>"}` —
with no bespoke gauge, no new registration, and no second code path to keep in sync
with the alert. The originally-planned standalone `VOWIFI_LINE_DISCOVERY_FAILED`
gauge is dropped in favor of this reuse (Constitution Principle V).
