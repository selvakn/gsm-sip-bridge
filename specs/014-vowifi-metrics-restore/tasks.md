---
description: "Task list for restoring call and SMS observability under VoWiFi"
---

# Tasks: Restore Call and SMS Observability Under VoWiFi

**Input**: Design documents from `/specs/014-vowifi-metrics-restore/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: Included. The project constitution makes Integration-First Testing and
TDD NON-NEGOTIABLE (`.specify/memory/constitution.md` Principles I & II) —
every task below that changes behavior has a corresponding integration test, no
mocks, real sockets/registry/SQLite.

**Organization**: Phase 2 (Foundational) is the plumbing every story needs:
schema, protocol types, the widened metric surface, the ingest path, and the
agent-side reporter. Phases 3–6 wire that plumbing into Agent A and Agent B, one
user story at a time, in spec priority order.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Maps to spec.md user stories US1–US4

---

## Phase 1: Setup

- [ ] T001 Confirm baseline: `cargo build --workspace` and `cargo test --workspace` pass on `014-vowifi-metrics-restore` before any change, so later failures are attributable to this feature

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Schema, wire protocol, metric surface, and the transport plumbing
that every user story depends on. No agent production code is touched yet.

**⚠️ CRITICAL**: No user story task may start until this phase is green.

- [ ] T002 [P] Add schema v2→v3 migration in `gsm-sip-bridge/src/store/schema.rs`: `SCHEMA_V3_SQL` adds `transport TEXT NOT NULL DEFAULT 'cs' CHECK (transport IN ('cs','vowifi'))` to `calls` and `sms`, adds `idx_calls_transport`/`idx_sms_transport`, drops and recreates `recent_calls`/`recent_sms` with the column; extend the `match version.as_str()` ladder with a `"2"` arm that runs v3 SQL and falls through to `"3"`; bump `SCHEMA_VERSION` to `"3"`
- [ ] T003 [P] Add a `Transport` enum (`Cs`, `Vowifi`; `Display`/`FromStr` to `"cs"`/`"vowifi"`) and a `transport` field to `CallRecord` in `gsm-sip-bridge/src/store/calls.rs`; update its insert/query SQL and mapping
- [ ] T004 [P] Add the same `transport` field to `SmsRecord` in `gsm-sip-bridge/src/store/sms.rs` (reuse the `Transport` type from T003); update its insert/query SQL and mapping
- [ ] T005 Extend `gsm-sip-bridge/tests/test_migration_sql.rs` with a v2→v3 case: open a v2 DB with pre-existing `calls`/`sms` rows, run `init_schema`, assert `meta.schema_version = '3'`, assert every pre-existing row reads back `transport = 'cs'`, assert no row has a NULL/empty transport (FR-011c)
- [ ] T006 Define the observability wire protocol in `gsm-sip-bridge/src/control/protocol.rs`: `ControlCmd::Observe { report: AgentReport }` variant; `AgentReport { agent: AgentKind, module_id: String, state: AgentState, events: Vec<ObservedEvent>, dropped: u64 }`; `AgentKind` (`Ims`, `Sip` → `"ims"`/`"sip"`); `AgentState { active_calls: Option<u32>, registered: Option<bool>, tunnel_up: Option<bool>, pbx_registered: Option<bool> }`; `ObservedEvent` tagged enum (`CallCompleted{status,duration_seconds}`, `PbxLegCompleted{outcome}`, `BridgeFailed{reason}`, `SmsReceived`, `SmsForwarded{outcome}`, `RegistrationAttempt{status}`) with the closed enums `CallStatus`, `BridgeFailureReason`, `RegistrationStatus`, `SmsOutcome` from data-model.md §1 — all `Serialize + Deserialize`, `#[serde(rename_all = "snake_case")]`, matching contracts/observability-protocol.md exactly
- [ ] T007 In `gsm-sip-bridge/src/metrics/mod.rs`: add a `"transport"` label to `CALLS_TOTAL`, `SIP_CALLS_TOTAL`, `CALL_DURATION_SECONDS`, `ACTIVE_CALLS`, `SMS_RECEIVED_TOTAL`, `SMS_FORWARDED_TOTAL`; add seven new metrics — `VOWIFI_REGISTERED` (Gauge), `VOWIFI_REGISTRATIONS_TOTAL` (CounterVec, `status`), `VOWIFI_TUNNEL_UP` (Gauge), `VOWIFI_BRIDGE_FAILURES_TOTAL` (CounterVec, `reason`), `AGENT_UP` (GaugeVec, `agent`), `AGENT_LAST_REPORT_SECONDS` (GaugeVec, `agent`), `OBSERVABILITY_EVENTS_DROPPED_TOTAL` (CounterVec, `agent`) — matching contracts/metrics-inventory.md
- [ ] T008 Update every existing circuit-switched call site to pass `"cs"` as the new trailing label: `gsm-sip-bridge/src/modules/mod.rs` (`CALLS_TOTAL`, `SIP_CALLS_TOTAL`×2, `CALL_DURATION_SECONDS`, `ACTIVE_CALLS`×3), `gsm-sip-bridge/src/sms/mod.rs` (`SMS_FORWARDED_TOTAL`×2) — one commit, so the tree is never half-migrated (Constitution Principle II)
- [ ] T009 [P] Update `gsm-sip-bridge/tests/test_metric_renames.rs` and `gsm-sip-bridge/tests/test_metrics_endpoint.rs` `with_label_values` calls to include the new trailing `"cs"` transport value, matching T007's new arity
- [ ] T010 [P] Add `resolve_module_id_for_port(port: &std::path::Path) -> String` to `gsm-sip-bridge/src/modules/discovery.rs`: walk sysfs from the tty device up to its USB device's `serial` attribute (mirrors `scan_modules`'s existing `read_sysfs_attr` walk) and pass it through the existing `derive_module_id`; fall back to the literal `"vowifi"` with a `tracing::warn!` if resolution fails (research.md §R7)
- [ ] T011 Create `gsm-sip-bridge/src/metrics/ingest.rs`: `apply_report(report: AgentReport)` — applies `state` gauges unconditionally (absolute, latest-wins), applies each `events` entry as a counter increment/histogram observation using T007's metrics with `report.module_id` and `"vowifi"` transport, adds `report.dropped` to `OBSERVABILITY_EVENTS_DROPPED_TOTAL{agent}`, and records `Instant::now()` into a process-wide `AgentLivenessState` (one entry per `AgentKind`, behind a `Mutex`/`OnceLock`) for T013 to read
- [ ] T012 Wire `ControlCmd::Observe` in `gsm-sip-bridge/src/control/server.rs::handle_connection`: match it *before* the existing `cmd_tx.send(...)` call, call `metrics::ingest::apply_report`, reply `ControlResp::ok()` directly — an `Observe` must never reach `CardPool`'s mailbox
- [ ] T013 In `gsm-sip-bridge/src/metrics/server.rs::metrics_handler`, before encoding: for each `AgentKind` in `ingest::AgentLivenessState`, if `now - last_report > 3 * agent_report_interval_seconds`, set `AGENT_UP{agent}` to 0 and zero every gauge that agent owns (`ACTIVE_CALLS{transport="vowifi",...}` for `ims`, `VOWIFI_REGISTERED`, `VOWIFI_TUNNEL_UP`); otherwise set `AGENT_UP{agent}` to 1 and `AGENT_LAST_REPORT_SECONDS{agent}` to the observed age
- [ ] T014 Create `gsm-sip-bridge/src/observability/reporter.rs`: `Reporter` struct wrapping a bounded ring buffer (capacity 1024) fed by an unbounded `mpsc` sender, drained by a background thread that connects to `[control].socket_path`, writes one `Observe` per report using `control::protocol::write_resp`/newline-JSON framing, and reads the response; on connect/write failure the report stays queued and is retried on the next drain tick; on a parse-rejection response the report is discarded (permanent failure); on buffer-full the oldest queued report is dropped and a local counter increments, folded into the next report's `dropped` field; the public API is a non-blocking `report(&self, state: AgentState, events: Vec<ObservedEvent>)` plus an internal heartbeat ticker at `agent_report_interval_seconds`
- [ ] T015 [P] Register the new submodule: add `pub mod reporter;` to `gsm-sip-bridge/src/observability/mod.rs`; add `pub mod ingest;` to `gsm-sip-bridge/src/metrics/mod.rs`
- [ ] T016 [P] Add `[metrics].agent_report_interval_seconds` (default `10`) to `gsm-sip-bridge/src/config/mod.rs`: add to `METRICS_KEYS`, add the field to `MetricsConfig`, parse it in `parse_metrics` with the default, add a matching comment to `config.toml.example`
- [ ] T017 Write `gsm-sip-bridge/tests/test_observability_ingest.rs`: bind a real `UnixListener` control server in a temp dir, send a real `Observe` report (one heartbeat, one `CallCompleted` event) over a real socket, scrape the real `/metrics` handler, and assert the resulting text contains the expected `calls_total{...,transport="vowifi"}` value and `agent_up{agent="ims"} 1` (FR-001–008 infra, contracts/observability-protocol.md)
- [ ] T018 [P] Write `gsm-sip-bridge/tests/test_observability_reporter.rs`: start a `Reporter` pointed at a socket with no listener, enqueue reports past the 1024 bound, assert the oldest are dropped and `dropped` climbs; then start a real listener and assert queued reports flush and daemon-side counters land intact with no reset (FR-019/019a/019b, FR-020)
- [ ] T019 [P] Write `gsm-sip-bridge/tests/test_agent_liveness.rs`: send one report for `agent="ims"`, assert `agent_up{agent="ims"}` is 1 and `active_calls{transport="vowifi"}` reflects the reported state; advance past `3 * agent_report_interval_seconds` with no further reports, scrape again, assert `agent_up{agent="ims"}` is 0 and the owned gauges are zeroed (FR-021/021a/021b, SC-009/010)

**Checkpoint**: `cargo test --workspace` green. The wire protocol, registry, and
persistence layers all work end-to-end synthetically. No VoWiFi agent
production code has been touched yet — Phases 3–6 only wire callers into this
plumbing.

---

## Phase 3: User Story 1 - Inbound VoWiFi calls appear on the dashboard (Priority: P1) 🎯 MVP

**Goal**: Inbound VoWiFi calls move the same dashboard call panels
circuit-switched calls already use — call counts by outcome, active calls,
duration distribution — with zero panel edits.

**Independent Test**: Enable VoWiFi, place and answer an inbound call, hang up;
`/metrics` shows one more `calls_total{transport="vowifi",status="answered"}`,
`active_calls{transport="vowifi"}` returns to 0, and the duration lands in
`call_duration_seconds`.

### Implementation for User Story 1

- [ ] T020 [US1] In `gsm-sip-bridge/src/ims/agent.rs::run_inner`, construct one `observability::reporter::Reporter` (agent = `Ims`, module_id via T010's `resolve_module_id_for_port(&config.modem_port)`), started alongside the existing SIP transport setup, and hold it for the lifetime of the agent
- [ ] T021 [US1] Track the in-flight call count in `gsm-sip-bridge/src/ims/agent.rs` and report it as `AgentState.active_calls` on every state transition (increment when an inbound call is accepted for bridging, decrement on `CallEnded`/decline/timeout) via the T020 `Reporter`
- [ ] T022 [US1] At each call's terminal point in `gsm-sip-bridge/src/ims/agent.rs` (answered-and-ended, declined, ring-timeout, caller-cancelled), emit one `ObservedEvent::CallCompleted { status, duration_seconds }` via the T020 `Reporter` — `status = Answered` only when the call actually reached the answered state, `duration_seconds = 0.0` for anything that never answered
- [ ] T023 [US1] Add a periodic heartbeat in `gsm-sip-bridge/src/ims/agent.rs` (or inside `Reporter` itself per T014) that sends the current `AgentState` every `agent_report_interval_seconds` even when `events` is empty, so liveness (T013/T019) has something to key off during idle periods
- [ ] T024 [P] [US1] In `gsm-sip-bridge/src/vowifi/mod.rs`, construct a `Reporter` (agent = `Sip`, module_id resolved the same way) in Agent B's startup, and emit `ObservedEvent::PbxLegCompleted { outcome }` once the PBX-side leg's result is known, mirroring how `modules::mod` counts `SIP_CALLS_TOTAL` today
- [ ] T025 [US1] Write `gsm-sip-bridge/tests/test_vowifi_call_metrics.rs`: drive `ims::agent`'s call-tracking logic directly (construct the same state machine pieces used in T021/T022 against a real `Reporter`/real control socket, without needing a live modem) through an answered call and a declined call, scrape `/metrics`, assert `calls_total{transport="vowifi"}` has one `answered` and one non-answered entry, `active_calls{transport="vowifi"}` is back to 0, and `call_duration_seconds{transport="vowifi"}` has one observation

**Checkpoint**: User Story 1 is independently functional — verify with
quickstart.md §2 "Inbound call".

---

## Phase 4: User Story 2 - Inbound VoWiFi SMS appear on the dashboard and in history (Priority: P1)

**Goal**: SMS received over VoWiFi are counted alongside circuit-switched SMS,
with forwarding outcome visible, and land in the SMS history table.

**Independent Test**: Send an SMS to the SIM's number with VoWiFi enabled;
`sms_received_total{transport="vowifi"}` and
`sms_forwarded_total{transport="vowifi",outcome=...}` increment, and the
message appears in `sms` with `transport='vowifi'`.

### Implementation for User Story 2

- [ ] T026 [US2] Add a `transport: Transport` parameter to `sms::record_and_forward` in `gsm-sip-bridge/src/sms/mod.rs`; use it both for the `SmsRecord` written (T004) and for the `SMS_FORWARDED_TOTAL{...,transport}` label (T007/T008)
- [ ] T027 [US2] Update the circuit-switched call site of `record_and_forward` (in `gsm-sip-bridge/src/modules/mod.rs`) to pass `Transport::Cs`
- [ ] T028 [US2] Update `gsm-sip-bridge/src/vowifi/mod.rs::forward_vowifi_sms` to pass `Transport::Vowifi` to `record_and_forward`, and emit `ObservedEvent::SmsReceived` via the Agent B `Reporter` (T024) at the point the SMS is first relayed from Agent A, and `ObservedEvent::SmsForwarded { outcome }` alongside the existing Discord-forward result
- [ ] T029 [P] [US2] Write `gsm-sip-bridge/tests/test_vowifi_sms_metrics.rs`: call `forward_vowifi_sms`'s underlying logic with a real `Reporter`/socket and a real (temp) store, assert `sms_received_total{transport="vowifi"}` and `sms_forwarded_total{transport="vowifi"}` increment and a `sms` row with `transport='vowifi'` is written

**Checkpoint**: User Stories 1 and 2 both independently functional — verify
with quickstart.md §2 "Inbound SMS".

---

## Phase 5: User Story 3 - VoWiFi calls are in the persisted call history (Priority: P2)

**Goal**: Every inbound VoWiFi call produces a `calls` row with the same fields
circuit-switched calls already carry.

**Independent Test**: Place an inbound VoWiFi call; query `calls` and find a
matching row with correct caller, duration, outcome, and `transport='vowifi'`.

### Implementation for User Story 3

- [ ] T030 [US3] Open a `StoreHandle` in `gsm-sip-bridge/src/ims/agent.rs::run_inner` against `config.sms.db_path` (same pattern `vowifi/mod.rs` already uses for its own store handle — same WAL file, independent connection)
- [ ] T031 [US3] At each call's terminal point in `gsm-sip-bridge/src/ims/agent.rs` (same points as T022), send `StoreCommand::InsertCall` with caller, `started_at`, `duration_seconds`, `status`, `sip_destination` (from `[bridge].sip_destination`), and `transport = Transport::Vowifi`
- [ ] T032 [P] [US3] Write `gsm-sip-bridge/tests/test_vowifi_call_history.rs`: drive an answered call and a missed call through the T031 logic against a real temp-file store, assert both rows exist with `transport='vowifi'` and correct `duration_seconds`/`status`

**Checkpoint**: User Stories 1–3 independently functional — verify with
quickstart.md §2 step 4 (`sqlite3` query).

---

## Phase 6: User Story 4 - VoWiFi-specific health is visible (Priority: P3)

**Goal**: Registration state, tunnel state, and bridge-failure reasons are
visible on the dashboard without reading logs.

**Independent Test**: Break the tunnel or let registration lapse; the
corresponding indicator changes state on the next scrape.

### Implementation for User Story 4

- [ ] T033 [US4] In `gsm-sip-bridge/src/ims/agent.rs`, hook the existing registration-renewal logic to report `AgentState.registered` on every transition and emit `ObservedEvent::RegistrationAttempt { status }` on each attempt (success, auth failure, rejection, timeout — map the existing error paths onto the four closed `RegistrationStatus` values)
- [ ] T034 [US4] In `gsm-sip-bridge/src/ims/agent.rs`, report `AgentState.tunnel_up` derived from whether a P-CSCF address has been read successfully (T for `read_pcscf`) and Agent A's SIP transport to it is currently alive (research.md §R6)
- [ ] T035 [US4] Add a `map_bridge_failure_reason(reason: &str) -> BridgeFailureReason` function in `gsm-sip-bridge/src/ims/agent.rs` that maps the existing free-text `ControlMessage::BridgeFailed`/decline reasons onto the five closed values, defaulting unmapped strings to `BridgeSetupFailed`; call it at the point `BridgeFailed`/ring-timeout/caller-cancelled/PBX-declined outcomes are already handled (same call sites as T022), emitting `ObservedEvent::BridgeFailed { reason }`
- [ ] T036 [P] [US4] Add four additive Grafana panels to `docker/grafana/provisioning/dashboards/gsm-sip-bridge.json`: VoWiFi registration state (`vowifi_registered`), tunnel state (`vowifi_tunnel_up`), bridge failures by reason (`vowifi_bridge_failures_total`), agent liveness (`agent_up`, `agent_last_report_seconds`) — new panel objects only, no edits to existing panel queries
- [ ] T037 [P] [US4] Write `gsm-sip-bridge/tests/test_vowifi_health_metrics.rs`: drive T033–T035's logic through a registration success, a registration failure, and a bridge failure with a known reason; assert `vowifi_registered`, `vowifi_registrations_total{status}`, `vowifi_tunnel_up`, and `vowifi_bridge_failures_total{reason}` all reflect it

**Checkpoint**: All four user stories independently functional — verify with
quickstart.md §2 "Health" and §2 "Failed bridge".

---

## Phase 7: Polish & Cross-Cutting Concerns

- [ ] T038 [P] Update `docs/observability.md`'s metric table: note the new `transport` label on the six widened metrics, add rows for the seven new metrics, and add a short paragraph on module-identity sharing between transports (FR-011a) per contracts/metrics-inventory.md
- [ ] T039 [P] Extend `gsm-sip-bridge/tests/test_metrics_endpoint.rs` with a VoWiFi-disabled case asserting the full metric surface is unchanged in values and panel-relevant series apart from the constant `transport="cs"` dimension (amended SC-006, FR-022)
- [ ] T040 Run `cargo fmt --all`, `make lint`, and `cargo test --workspace`; fix anything surfaced before considering the feature done (CLAUDE.md pre-commit checklist, Constitution Principle II)
- [ ] T041 Walk through quickstart.md §1 by hand (the `nc`/`curl` smoke test) against a locally running daemon to confirm the manual verification path documented for future debugging actually works as written

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: no dependencies
- **Foundational (Phase 2)**: depends on Setup; BLOCKS every user story — schema, protocol, metrics, ingest, and the reporter must all exist before any agent is wired to them
- **User Stories (Phase 3–6)**: all depend on Phase 2 only, not on each other. They touch overlapping files (`ims/agent.rs` in US1/US3/US4, `vowifi/mod.rs` in US1/US2) so within this codebase they are best done sequentially in priority order even though nothing about the *data* couples them
- **Polish (Phase 7)**: depends on whichever stories were completed

### Within Foundational (Phase 2)

T002/T003/T004 (schema + records) are parallel-safe (different files) but must
land before T005 (the migration test) and before T007 (metrics don't need them,
but T011/ingest does via T003/T004's `Transport` type... actually ingest only
needs T006/T007, not the store types — store types are only needed for US2/US3.
Kept in Foundational because the schema is shared infrastructure, not because
Phase 2's later tasks depend on it.)

T006 (protocol types) blocks T011 (ingest) and T014 (reporter), both of which
serialize/deserialize `AgentReport`. T007 blocks T008 (call sites) and T011
(ingest applies to these metrics). T011 blocks T012 (server routing) and T013
(liveness expiry reads what T011 writes). T010 has no dependents inside Phase 2
but every user story needs it.

### Within Each User Story

Reporter construction (e.g., T020) before anything that calls it (T021–T023).
Tests (T025, T029, T032, T037) are written against the logic the preceding
implementation tasks add, and should fail before those tasks land if done
strictly TDD — the constitution requires green-on-commit, not a
red-before-green ceremony, so within a single commit both may land together.

### Parallel Opportunities

- T002, T003, T004 (Foundational, different files)
- T009, T010 (Foundational, independent of the metrics/protocol work each depends on separately)
- T015, T016 (Foundational, trivial/independent)
- T018, T019 (Foundational tests, independent of each other once T011–T014 exist)
- T024 can proceed alongside T020–T023 (different file, `vowifi/mod.rs` vs `ims/agent.rs`)
- T036 (dashboard JSON) is independent of T033–T035 (Rust) and can be done in parallel
- T038, T039 (Polish, different files)

---

## Implementation Strategy

### MVP First (User Story 1 only)

1. Phase 1 (Setup) → Phase 2 (Foundational) — required, no shortcuts
2. Phase 3 (US1) → **STOP and VALIDATE** with quickstart.md §2 "Inbound call"
3. This alone restores the headline symptom: calls visible on the dashboard

### Incremental Delivery

Phase 2 → US1 (calls) → US2 (SMS) → US3 (call history) → US4 (health) → Polish.
US1 and US2 are both P1 and should ship together in practice since they're the
reported regression; US3 and US4 are additive hardening on top.

### Recommended Commit Boundaries

Matches the phase/task grouping above: T002–T005 (schema), T006–T010 (protocol
+ metrics + call-site migration + module-id helper — must land together, this is
the "one commit" from Constitution Principle II), T011–T019 (ingest + reporter +
liveness + their tests), then one commit per user story phase, then Polish.
