# Phase 0 Research: Card Phone Number and Instance Identity in Alerts

No `NEEDS CLARIFICATION` markers remained after `/speckit-clarify`. The two
product decisions (phone source; host label) were made with the user; this
document records the supporting technical findings from the codebase exploration.

## R1 — Where alerts are rendered

**Decision**: Add both new fields in the two existing embed builders in
`gsm-sip-bridge/src/alerts/discord.rs` — `forward_sms` (SMS) and `send_alert`
(critical events). Both already build a `fields` array and a `footer`, and both
call the shared `post_with_retry`.

**Rationale**: Single choke point per notification shape; no new sender.

**Alternatives considered**: A post-processing enrichment wrapper — rejected as
unnecessary indirection (Constitution V).

## R2 — Hostname retrieval

**Decision**: `libc::gethostname` (via the already-present `libc = "0.2"` dep),
wrapped in `alerts::system_hostname() -> String` with a `"unknown"` fallback.

**Rationale**: No new crate; works in every split process (all on the same host).
Config `instance_name` overrides it when set.

**Alternatives considered**: `hostname`/`gethostname`/`whoami` crates (new
dependency, no benefit); reading `/etc/hostname`/`$HOSTNAME` (less reliable than
the syscall). Rejected.

## R3 — Phone-number sources (uneven across transports)

**Decision**:
- VoLTE: reuse existing `[[volte.line]].msisdn` (`config/mod.rs`,
  `volte/discovery.rs` `ResolvedLine.msisdn`).
- VoWiFi: add `[[vowifi.line]].msisdn` (mirrors VoLTE through `raw.rs` +
  `build.rs`); `VowifiLineOverride` has no number today.
- CS: no per-card config table exists → use the live `AT+CNUM` value already
  cached in `SlotState.phone_number` (`modules/slot.rs:43`,
  `modules/pool/mod.rs:613`), and cache it in the worker for worker-emitted alerts.
- Unresolved → render literal `unknown` (clarification 2026-08-11).

**Rationale**: Reuses established config; CS SIM read is the only per-card auto
source; matches the user's "config + AT+CNUM fallback" decision.

**Alternatives considered**: Adding a CS `[[card]]` config table — rejected as
scope creep (cards are auto-discovered; no existing table). Querying `AT+CNUM`
from VoWiFi/VoLTE agents — rejected (contended AT channel risk; config is the
intended source there).

## R4 — Cross-process phone resolution for daemon-detected categories

**Decision**: Build a `unit_id → msisdn` map from the resolved VoLTE/VoWiFi lines
at `metrics::ingest::init_alerts` (stored beside the existing
`ALERTS_CONFIG`/`ALERTS_CLIENT` `OnceLock`s) and look it up by `unit_id` in
`dispatch_transition`. `supervise::orchestrate` resolves the same way from the
`resolution.lines` already in scope for `LineDiscoveryFailed`.

**Rationale**: `RegistrationLoss`/`TunnelFailure`/`GmConnectionLost` are detected
in the daemon from `AgentReport`s that carry only `module_id`/`unit_id`. Config is
loaded in every process, so a config-derived map is the only value consistent
across the split-process boundary.

**Alternatives considered**: Threading the number in the `AgentReport` protocol —
rejected (protocol change; the number is static config, not live agent state).
An in-process global registry populated at runtime — rejected (does not cross the
process boundary the daemon evaluates on).

## R5 — Testing approach

**Decision**: Extend the existing alert tests — `wiremock` to capture the POST
body and assert the `Phone` field + `footer` instance; `insta` snapshots where
already used. Unit-test `instance_label` (config vs hostname fallback) and the
`unit_id → msisdn` resolver as pure functions.

**Rationale**: Integration-first (Constitution I) with the suite's existing tools;
no new mocks.

**Alternatives considered**: None needed.
