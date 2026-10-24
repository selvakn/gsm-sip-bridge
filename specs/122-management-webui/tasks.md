---
description: "Task list for the read-only management web UI"
---

# Tasks: Read-only management web UI

**Input**: Design documents from `specs/122-management-webui/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/ (http-api, config, agent-report, metrics), quickstart.md

**Tests**: Included. The constitution (Principle I, integration-first, and TDD in Development Workflow) requires them, and quickstart.md lists the verification matrix. Within each story, write the test task first and see it fail.

**Organization**: Tasks are grouped by user story. Spec priorities: US1, US2 and US6 are P1, US3 and US4 are P2, US5 is P3. US6 (authentication) comes first because every page sits behind it.

**Before every commit**: `make format && make lint && make test` (CLAUDE.md). Use only synthetic numbers (`+919000000000`, `+919000000001`) in tests and fixtures.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependency on an incomplete task)
- **[Story]**: US1–US6 from spec.md
- Paths are relative to the repository root. The crate is `gsm-sip-bridge/`.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Dependency, module skeleton and asset placeholders.

- [X] T001 Add `subtle = "2.6"` to `[dependencies]` in `gsm-sip-bridge/Cargo.toml` (it's already in `Cargo.lock` transitively; used for constant-time password comparison, research R9), and confirm `make lint` (cargo-deny) still passes
- [X] T002 Create the `webui` module skeleton:
  - `gsm-sip-bridge/src/webui/mod.rs` with submodules `auth`, `api`, `lines`, `queries`, `config_view` and `signal`, each an empty file with a module doc comment citing specs/122;
  - register `pub mod webui;` in `gsm-sip-bridge/src/lib.rs`.
- [X] T003 [P] Create placeholder assets:
  - `gsm-sip-bridge/src/webui/assets/index.html`: doctype, `<link href="/assets/style.css">`, `<script src="/assets/app.js" defer>`, no inline script or style;
  - `gsm-sip-bridge/src/webui/assets/app.js`: empty IIFE;
  - `gsm-sip-bridge/src/webui/assets/style.css`: `:root` colour tokens plus a `prefers-color-scheme: dark` override.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Schema v5, config surface, protocol additions, signal parsing and metrics. Every story builds on these.

**⚠️ CRITICAL**: No user story work begins until this phase is complete.

- [X] T004 Write a failing v4→v5 migration test in `gsm-sip-bridge/tests/test_migration_sql.rs`. It must check that:
  - existing `calls` rows survive with `direction = 'inbound'`;
  - `loss_percent`/`jitter_ms`/`rtt_ms` exist and are NULL;
  - the `events` table exists with `idx_events_at`/`idx_events_module`;
  - `idx_calls_direction` exists;
  - the `recent_calls` view exposes `direction`;
  - `schema_version = '5'`.
- [X] T005 Implement the schema v5 migration in `gsm-sip-bridge/src/store/schema.rs` per data-model.md:
  - bump `SCHEMA_VERSION` to `"5"`;
  - append a migration that does `ALTER TABLE calls ADD COLUMN direction TEXT NOT NULL DEFAULT 'inbound' CHECK (direction IN ('inbound','outbound'))`, adds the three nullable REAL columns, creates `events` plus its indexes, and drops and recreates `recent_calls`.

  Make T004 pass, and keep `gsm-sip-bridge/tests/test_store_concurrent_writers.rs` green.
- [X] T006 Extend `CallRecord` in `gsm-sip-bridge/src/store/calls.rs`:
  - add `direction: CallDirection` (a new enum `Inbound`/`Outbound` with `as_str`);
  - add `loss_percent`, `jitter_ms` and `rtt_ms`, all `Option<f64>`;
  - update `insert_call`;
  - set `Inbound`/`None` at every existing constructor (`gsm-sip-bridge/src/ims/observability.rs` `insert_call_row`, the CS call sites in `gsm-sip-bridge/src/modules/`) so behaviour is unchanged;
  - extend `gsm-sip-bridge/tests/test_store_calls.rs` to round-trip the new fields.
- [X] T007 Add the config surface from contracts/config.md:
  - a `WebuiConfig` (`enabled`, `listen_addr`, `port`, `password: Secret<String>`, `grafana_url`) as `AppConfig.webui` in `gsm-sip-bridge/src/config/mod.rs`, with raw parsing in `gsm-sip-bridge/src/config/raw.rs` (deny unknown fields) and defaults and validation in `gsm-sip-bridge/src/config/build.rs`;
  - `MetricsConfig.signal_poll_interval_seconds` (default 60; `0` disables; values 1–19 are raised to 20 with a warning);
  - `AlertsConfig.event_retention_days` (default 30, at least 1);
  - validation errors for: an empty password when enabled, a non-IP `listen_addr`, a port clash with `[metrics].port`/`[sip].port`/`[sip_server].listen_port`, and a `grafana_url` without an http(s) scheme.

  Tests go in `gsm-sip-bridge/tests/test_config.rs`: defaults when the table is absent, `env:` password resolution, and each validation error message.
- [X] T008 Document every new key in `config.toml.example` (a commented `[webui]` block, plus the new `[metrics]`/`[alerts]` keys) and in the config reference under `docs/`, so `gsm-sip-bridge/tests/test_config_docs.rs` passes. Add a `WEBUI_PASSWORD=` placeholder line to the docker `.env` example under `docker/`.
- [X] T009 Add `load_config_with_snapshot(path) -> BridgeResult<LoadedConfig>` in `gsm-sip-bridge/src/config/mod.rs`. It returns the `AppConfig`, a clone of the `toml::Value` taken **before** `env::resolve_in_place`, the source path and `loaded_at: DateTime<Utc>`. Make `load_config` delegate to it, and add a unit test that the snapshot still contains the literal `env:NAME` string (research R11).
- [X] T010 Add `AgentReport.details: Option<AgentDetails>` in `gsm-sip-bridge/src/control/protocol.rs` per contracts/agent-report.md. It needs:
  - `AgentDetails`;
  - `RegistrationStateWire`, with a `From<ims::RegistrationState>` impl and a `Display` that delegates to the same wording `vowifi-status` prints;
  - `SignalReadingWire`, `ActiveCallInfoWire` and `CallDirectionWire`;
  - serde attributes `skip_serializing_if = "Option::is_none"` and `default`.

  Unit tests: an old report JSON without `details` decodes; `details` round-trips; a report built with more than 4 active calls is truncated to 4 by a `with_active_calls` constructor.
- [X] T011 Keep agent details in ingest, in `gsm-sip-bridge/src/metrics/ingest.rs`:
  - add a `details()` map `Mutex<HashMap<(AgentKind, String), AgentDetails>>`, updated only when the report is not a replay. A `Some` field overwrites; `None` keeps the previous value; `active_call_details` replaces the list.
  - add `pub fn snapshot_lines() -> Vec<AgentSnapshot>`, returning per key the liveness `AgentRecord` fields the UI needs (registered/tunnel/gm/child_sa/uplink/flapping/expires_at/age/up) plus the cloned details.
  - in `gsm-sip-bridge/src/metrics/server.rs` `refresh_agent_liveness`, clear `active_call_details` for stale agents.

  Tests in `gsm-sip-bridge/tests/test_observability_ingest.rs`: merge rules, the replay not overwriting details, and stale clearing.
- [X] T012 [P] Add the series from contracts/metrics.md in `gsm-sip-bridge/src/metrics/mod.rs`: the four `signal_*` gauges plus `signal_last_read_seconds` (labels `module`, `transport`), `events_recorded_total{kind_group}` and `webui_logins_total{outcome}`. Add a helper `set_signal(module, transport, &SignalReading)` that sets known values and **removes** label series for `None` values (FR-023). Test in `gsm-sip-bridge/tests/test_metrics_endpoint.rs` that an unknown RSRP produces no `signal_rsrp_dbm` line.
- [X] T013 [P] Create `gsm-sip-bridge/src/modules/signal.rs` (register it in `gsm-sip-bridge/src/modules/mod.rs`) with:
  - a `SignalReading` struct (data-model.md);
  - `parse_csq(&[String]) -> Option<i32>` (dBm = −113 + 2·rssi; `99` → `None`);
  - `parse_qeng_servingcell(&[String])`, returning RAT plus RSRP/RSRQ/SINR for the LTE form and `None` for the LTE fields on WCDMA/GSM/`SEARCH`/`LIMSRV` or on a parse failure. Reuse the response fixtures from `gsm-sip-bridge/src/volte/pani.rs` tests and keep `pani::parse_serving_cell` untouched;
  - `read_signal(&mut AtCommander) -> SignalReading`, which sends `AT+CSQ` then `AT+QENG="servingcell"` and tolerates either failing.

  Unit tests cover each form.

**Checkpoint**: The foundation compiles and `make test` is green, so story work can start.

---

## Phase 3: User Story 6 — Keep the UI closed to everyone else (Priority: P1) 🎯 MVP gate

**Goal**: An authenticated, off-by-default listener on its own port, with login, logout, rate limiting and security headers, serving the shell.

**Independent Test**: With the UI enabled, every route without credentials and with a wrong password returns no data. Login, logout and a restart behave as specified. With the UI disabled, nothing listens.

- [X] T014 [US6] Write `gsm-sip-bridge/tests/test_webui_auth.rs` against `webui::router(state)` served on `127.0.0.1:0` with `reqwest`. Cover:
  - every `/api/*` route in contracts/http-api.md returns `401` with body exactly `{"error":"unauthenticated"}` when there is no cookie;
  - `GET /` without a cookie returns `303` to `/login`;
  - a wrong password returns `401`;
  - the correct password returns `204` with a `Set-Cookie` containing `HttpOnly`, `SameSite=Strict`, `Path=/` and `Max-Age=604800`, and the cookie then grants `/api/health`;
  - 5 wrong attempts return `429` with `Retry-After`, and the correct password is still refused during the lockout;
  - logout invalidates the cookie;
  - a fresh router instance (simulating a restart) rejects the old cookie;
  - the CSP, `X-Content-Type-Options` and `Referrer-Policy` headers are present;
  - `PUT` returns `405`.
- [X] T015 [US6] Implement `gsm-sip-bridge/src/webui/auth.rs`:
  - `Sessions`: 32 random bytes from `rand`, base64url-encoded, 7-day expiry, pruned lazily on lookup;
  - `RateLimiter`: per-IP, 5 consecutive failures cause a 300s lockout, a success resets, the map is bounded at 1024 entries with the oldest evicted;
  - a constant-time password check with `subtle::ConstantTimeEq`;
  - `POST /api/login` and `POST /api/logout` handlers;
  - `require_session` middleware (`401` JSON for `/api/*`, `303 /login` for HTML);
  - increments of `webui_logins_total{outcome}`.
- [X] T016 [US6] Implement `gsm-sip-bridge/src/webui/mod.rs`:
  - `WebuiState`: sessions, limiter, the `LoadedConfig` snapshot, the Grafana URL, a store-reader placeholder and an `Option<ControlCmdSender>` for CS slots;
  - `pub fn router(state) -> Router` serving the embedded assets (`include_str!`, `Cache-Control: no-cache`, ETag from `option_env!("GIT_SHA")`), `/`, `/login`, the auth routes and an `/api` nest behind the middleware;
  - a layer adding the security headers from contracts/http-api.md, and a 405 fallback;
  - `pub async fn serve(cfg, state)`, which binds `listen_addr:port`.
- [X] T017 [US6] Wire `webui::serve` into `gsm-sip-bridge/src/commands/daemon.rs`:
  - switch to `load_config_with_snapshot`;
  - spawn the server only when `config.webui.enabled`;
  - log a bind error as `error!` without exiting (FR-004);
  - abort it at shutdown alongside `metrics_handle`.

  Add a test in `gsm-sip-bridge/tests/test_cs_disabled.rs` (or a new `gsm-sip-bridge/tests/test_webui_disabled.rs`) that a daemon config without `[webui]` opens no listener on the default port.
- [X] T018 [P] [US6] Build the login flow and app shell in `gsm-sip-bridge/src/webui/assets/index.html`, `app.js` and `style.css`:
  - a password form that posts JSON to `/api/login` and shows distinct messages for 401 and 429 (including the retry time);
  - a logout button;
  - nav for `#/overview`, `#/calls`, `#/sms`, `#/events` and `#/config`;
  - a shared `api(path)` helper that redirects to `/login` on 401;
  - DOM built only with `createElement`/`textContent`;
  - a single-column layout under 640px with no horizontal scroll.

**Checkpoint**: The UI can be enabled and logged into, and serves an empty shell. US6 is done.

---

## Phase 4: User Story 1 — See at a glance whether every line is healthy (Priority: P1)

**Goal**: An Overview page with one card per line, plus a bridge header. Signal is polled for CS and modem-backed IMS lines.

**Independent Test**: Drive real `AgentReport`s for a VoWiFi line and a VoLTE line. `/api/overview` matches what was reported. A child SA going down turns that card `down` with the reason named, and silence past 3× the report interval turns it `stale`.

- [X] T019 [P] [US1] Write `gsm-sip-bridge/tests/test_webui_overview.rs`. Feed reports through `metrics::ingest::apply_report` for a VoWiFi line (`Ims`, `Sip`, `Supervisor`) and a VoLTE line (`Volte`, `VolteSip`) with `details` set. Assert:
  - `/api/overview` line fields (registration wording, expires_at, tunnel/child_sa/gm/uplink, pdn_up null for VoWiFi, msisdn from config `+919000000000`);
  - `child_sa_up=false` gives `health="down"` with `"child_sa_down"` in `health_reasons`;
  - an agent past the staleness threshold gives `stale=true` and `health="stale"`;
  - no agents and CS disabled give `discovery_note="no_lines"`;
  - a `pcsc_reader` line gives `signal=null` and `signal_note="not_available_pcsc"`;
  - the bridge block has version, uptime and `pbx_registered`;
  - `/api/health` has the module and store-queue counters.
- [X] T020 [P] [US1] Implement and unit-test a pure `derive_health(&LineFacts) -> (Health, Vec<&'static str>)` in `gsm-sip-bridge/src/webui/lines.rs`, using the rules in data-model.md: stale, then down (unregistered / tunnel / child SA / PDN), then degraded (Gm down / uplink held / flapping), then ok.
- [X] T021 [US1] Implement `LineView` assembly in `gsm-sip-bridge/src/webui/lines.rs`:
  - group `ingest::snapshot_lines()` by `module_id` across agent kinds;
  - set the transport from the agent kind;
  - take msisdn from `alerts::line_phone_map(&config)`;
  - read `pdn_up` from the `VOLTE_PDN_UP` gauge;
  - list CS lines by sending `ControlCmd::ListSlots` on the daemon's control channel with a 1s timeout (skip them when CS is disabled), with signal from `webui::signal`;
  - merge `active_call_details` from both agents of a line.
- [X] T022 [US1] Implement `GET /api/overview` and `GET /api/health` in `gsm-sip-bridge/src/webui/api.rs`:
  - version from `CARGO_PKG_VERSION` and `GIT_SHA`;
  - uptime (expose a `pub fn uptime()` from `gsm-sip-bridge/src/metrics/server.rs` over `START_TIME`);
  - `pbx_registered` from the `Sip`/`VolteSip` details/state, or the CS `SIP_REGISTERED` gauge, or `null` when no PBX is configured;
  - `grafana_url` (`null` when empty);
  - `active_alerts` from `CRITICAL_EVENT_ACTIVE == 1` series (`since` stays `null` until T048);
  - health from `MODULES_ACTIVE`, `MODULES_FAILED`, `STORE_QUEUE_DEPTH`, `OBSERVABILITY_EVENTS_DROPPED_TOTAL` and `CS_ENABLED`.

  Mount both in `gsm-sip-bridge/src/webui/mod.rs`.
- [X] T023 [US1] Report `details.registration_state`, `registered_at` and `plmn` (`"MCC-MNC"` from the registration config) from the VoWiFi IMS agent's reporter in `gsm-sip-bridge/src/ims/agent/mod.rs`. Extend `gsm-sip-bridge/tests/test_observability_reporter.rs` to assert that they are sent.
- [X] T024 [US1] Do the same for the VoLTE carrier agent's reporter in `gsm-sip-bridge/src/volte/carrier_agent.rs` (or wherever its `Reporter` state is built in `gsm-sip-bridge/src/volte/bridge.rs`), so VoLTE cards show registration wording and carrier.
- [X] T025 [US1] Add the CS signal poll in `gsm-sip-bridge/src/modules/worker.rs` `tick_liveness_probe`:
  - when `signal_poll_interval_seconds > 0`, the interval has elapsed, and `self.call_ctx.is_none()`, call `modules::signal::read_signal(&mut self.at)` instead of the bare `AT` probe;
  - a successful `AT+CSQ` updates `last_at_success` exactly as `AT` does;
  - write the result to `metrics::set_signal(module, "cs", …)` and `webui::signal::update(module_id, reading)`;
  - plumb the interval through the worker setup struct from `config.metrics`.

  Test with the fake-modem harness used by the `gsm-sip-bridge/src/modules/at_worker.rs` tests: there is a poll after the interval, no poll while a call is active, and a `99` RSSI exports no series.
- [X] T026 [US1] Add the IMS/VoLTE signal poll in `gsm-sip-bridge/src/volte/sms.rs` `run_modem_reader`:
  - add a `signal: Option<SignalPoll { slot: Arc<Mutex<Option<SignalReading>>>, every_n_passes: u32 }>` parameter;
  - every `every_n_passes` sweeps (`ceil(interval / MODEM_SWEEP_INTERVAL)`), take `modem_lock`, open **once** through `open_with_retry`, run `read_signal`, release, and store the reading in the slot;
  - do it under the watchdog's existing `SmsSweep` phase guard;
  - update the three callers (`gsm-sip-bridge/src/ims/agent/mod.rs` ~L361, `gsm-sip-bridge/src/volte/bridge.rs` ~L350, `gsm-sip-bridge/src/commands/volte.rs` ~L1209) and have each agent's reporter copy the slot into `details.signal`.

  Unit-test the pass-count scheduling as a pure function. The hardware check is quickstart step 2.
- [X] T027 [US1] In `gsm-sip-bridge/src/metrics/ingest.rs`, set the signal gauges from `details.signal` for `Ims`/`Volte` reports (transport from `transport_label`). In `gsm-sip-bridge/src/metrics/server.rs`, refresh `signal_last_read_seconds` on scrape. Test in `gsm-sip-bridge/tests/test_observability_ingest.rs`.
- [X] T028 [US1] Build the Overview page in `gsm-sip-bridge/src/webui/assets/app.js` (with styles in `style.css`):
  - **Header**: uptime, version, PBX state, active alerts, and a Grafana link when set.
  - **One card per line**:
    - a health badge with its reasons;
    - registration wording, plus a client-side expiry countdown from `expires_at` and time registered since `registered_at`;
    - VoWiFi chips (tunnel, child SA, Gm, uplink) and the VoLTE PDN chip;
    - signal bars from `rssi_dbm`, with RSRP/RSRQ/SINR when present, de-emphasised and labelled "doesn't affect calls" on VoWiFi lines, and "not available"/"not polled yet" notes;
    - the msisdn masked to its last 4 digits with a reveal toggle;
    - active call count and last-report age;
    - a stale banner.
  - **"No lines" empty state**.
  - **Refresh**: every 5s, paused on `document.visibilitychange`.

**Checkpoint**: US1 is demonstrable on its own (after US6).

---

## Phase 5: User Story 2 — Read SMS without Discord or the modem (Priority: P1)

**Goal**: A paged, searchable SMS inbox read only from SQLite, with OTP copy.

**Independent Test**: Seed SMS rows and query `/api/sms` with the filters. Bodies come back verbatim, nothing in the store changes, and the page offers a copy control for a 6-digit code.

- [X] T029 [P] [US2] Write `gsm-sip-bridge/tests/test_webui_sms.rs`:
  - seed SMS rows through `StoreHandle` (two lines; one `forwarding_status='failed'`; one body containing `<script>alert(1)</script>`; one containing `Your OTP is 482913`; senders `+919000000001`);
  - check the line filter, the `q` search over sender and body, paging totals and newest-first order;
  - check the body is returned byte-for-byte;
  - check the row count and every `forwarding_status` are unchanged after the requests;
  - check that an unreadable db path makes `/api/sms` return `503 {"error":"store_unavailable"}` while `/api/overview` still returns 200.
- [X] T030 [US2] Implement `gsm-sip-bridge/src/webui/queries.rs`:
  - a `StoreReader` that opens its own `rusqlite::Connection` with `SQLITE_OPEN_READ_ONLY` and a 5s `busy_timeout`, kept in a `Mutex`, with queries run through `tokio::task::spawn_blocking`;
  - a `Page { page, per_page (default 50, clamp 1..=200) }` helper;
  - `list_sms(filter, page) -> Paged<SmsRow>`, with the `LIKE` pattern bound as a parameter.

  Put the reader in `WebuiState`, replacing the placeholder from T016.
- [X] T031 [US2] Implement `GET /api/sms` in `gsm-sip-bridge/src/webui/api.rs`, mounted behind the session middleware.
- [X] T032 [US2] Build the SMS page in `gsm-sip-bridge/src/webui/assets/app.js`:
  - a line filter, and a debounced (300ms) search box;
  - a paged list showing sender, line, transport, time in the browser timezone, body and a forwarding badge (failed forwarding highlighted);
  - OTP detection per research R10 (a standalone 4–8 digit number, preferring one within 40 chars of otp/code/verification/password/pin);
  - a copy button using `navigator.clipboard.writeText` that copies only the code, with a select-text fallback;
  - `textContent`-only rendering.

**Checkpoint**: US1, US2 and US6 are done, which completes the P1 scope.

---

## Phase 6: User Story 3 — Review calls (Priority: P2)

**Goal**: Call totals and a filterable log in both directions, per-call quality, and live calls.

**Independent Test**: Inbound and outbound calls of each status appear with the right direction, status and transport, the totals match, and an active call shows live quality or "unavailable".

- [X] T033 [P] [US3] Write `gsm-sip-bridge/tests/test_webui_calls.rs`:
  - seed inbound and outbound rows across today, 3 days ago and 10 days ago, over two lines and all statuses;
  - check `/api/calls/summary` today, 7-day, `by_line` and `by_direction` counts and `avg_answered_seconds`;
  - check `/api/calls` filters (line, direction, transport, status, from/to), newest-first order and paging;
  - check that quality NULLs come back as JSON `null`;
  - check that an `ActiveCallInfoWire` in a report appears in the overview line's `active_calls`.
- [X] T034 [US3] Implement `call_summary()` (local-midnight "today" and a 7-day window over `idx_calls_started_at`) and `list_calls(filter, page)` in `gsm-sip-bridge/src/webui/queries.rs`.
- [X] T035 [US3] Implement `GET /api/calls/summary` and `GET /api/calls` in `gsm-sip-bridge/src/webui/api.rs`.
- [X] T036 [US3] Record VoWiFi/VoLTE outbound calls in `gsm-sip-bridge/src/vowifi/mod.rs`:
  - add a pure `outbound_status(outcome) -> Option<CallStatus>` per the data-model.md mapping;
  - note `started_at` when a trusted request is accepted;
  - at each terminal point in the outbound loop, write a `CallRecord { direction: Outbound, caller_id: destination, module_id: line card_id or "" }` through the process's existing `StoreHandle`;
  - for `Placed`, keep the start time in `ActiveOutboundCall` and write the `answered` row with its duration when `service_active_outbound_call` ends the call;
  - don't record untrusted-source 403s.

  Unit-test the mapping, and add an integration case to `gsm-sip-bridge/tests/test_vowifi_call_history.rs`.
- [X] T037 [US3] Record CS outbound calls in `gsm-sip-bridge/src/modules/pool/dispatch.rs` `handle_outbound_request` (refusals → `failed`, unanswered → `missed`) and in its hangup handling (answered + duration), with the same mapping, through the daemon's store sender. Test in `gsm-sip-bridge/tests/test_card_pool.rs`.
- [X] T038 [US3] Store per-call quality: in `gsm-sip-bridge/src/ims/observability.rs`, buffer the last `report_media_quality` values per source and write local loss and jitter (in ms) plus remote RTT (in ms) into the row in `insert_call_row`. Confirm in `gsm-sip-bridge/src/ims/agent/call.rs` that quality is reported before the row is inserted at teardown, and reorder if not. Extend `gsm-sip-bridge/tests/test_vowifi_call_history.rs`.
- [X] T039 [US3] Report live calls in `details.active_call_details`:
  - **Inbound**, from the IMS agent (`gsm-sip-bridge/src/ims/agent/mod.rs`/`call.rs`): remote, `inbound`, start time, answered flag, and the latest loss/jitter from a shared `ReceiveStats` handle the call loop already holds (`gsm-sip-bridge/src/ims/media_stats.rs`).
  - **Outbound**, from `gsm-sip-bridge/src/vowifi/mod.rs`'s reporter.
  - **CS lines**: built in `gsm-sip-bridge/src/webui/lines.rs` from the `ListSlots` slot state, with quality `null`.

  Cap the list at 4 entries.
- [X] T040 [US3] Build the Calls page in `gsm-sip-bridge/src/webui/assets/app.js`:
  - summary tiles for today and 7 days, split by direction;
  - a filter bar (line, direction, transport, status, date range) and a paged table;
  - an "In progress" section showing remote party, direction, running duration and quality, or "unavailable";
  - refresh every 5s while visible.

**Checkpoint**: US3 works on its own.

---

## Phase 7: User Story 4 — Find out what happened while nobody was watching (Priority: P2)

**Goal**: A persisted, deduplicated event timeline from every process, with retention and a page.

**Independent Test**: A registration loss and recovery produce a raise and a resolve with a duration. The rows survive a store reopen, a disabled alert category is still recorded, and old rows are pruned.

- [X] T041 [P] [US4] Write `gsm-sip-bridge/tests/test_webui_events.rs`:
  - with a sink installed on a temp-file store, a `RegistrationLoss` Failure followed by a Recovered through `alerts::dispatch` gives two rows, and the resolve has `raise_id` and `duration_seconds`;
  - a second Failure while open (a retried delivery) adds no row;
  - a Recovered with nothing open adds no row;
  - a category with `enabled=false` is still recorded (FR-051);
  - `Sms`/`MissedCall` are not recorded;
  - `TunnelRecovered{dead_peer: Real}` and `UplinkHoldStarted`/`Ended` through `apply_report` produce `tunnel_recovered`, `dead_peer_loss` and `uplink_hold_*` rows;
  - rows survive a `StoreHandle` reopen;
  - `PruneEvents` deletes only rows older than the retention;
  - `/api/events` filters by line and by a `kind` prefix.
- [X] T042 [US4] Create `gsm-sip-bridge/src/store/events.rs` (registered in `gsm-sip-bridge/src/store/mod.rs`) with:
  - the `EventKind` enum (data-model.md) with `as_str`, plus `Severity` and `EventRecord`;
  - `insert_event`/`prune_events`, and new `StoreCommand::InsertEvent` and `StoreCommand::PruneEvents { older_than }` arms in `writer_loop`;
  - a process-global `install_sink(Sender<StoreCommand>)` and `record(...)`, with an in-process raise/resolve map `(category, module_id) → raise_at`. The map dedups (a Failure while open, or a Recovered with nothing open, is dropped) and computes `duration_seconds = at − raise_at`. The resolve's `raise_id` is filled in the writer thread at insert time with `(SELECT id FROM events WHERE kind = 'alert_raised:<cat>' AND module_id IS ?1 AND at = ?2 ORDER BY id DESC LIMIT 1)`. This is safe because the raise was sent earlier on the same ordered channel to the same writer thread;
  - truncation of `detail` to 500 chars;
  - incrementing `events_recorded_total`;
  - a debug log, without panicking, when no sink is installed;
  - `pub fn open_raises()` for the overview.
- [X] T043 [US4] Call `store::events::record` at the top of `alerts::dispatch` in `gsm-sip-bridge/src/alerts/mod.rs`, before `precheck`, so disabled or undeliverable categories are still recorded. Skip `AlertCategory::Sms` and `AlertCategory::MissedCall`, and map `CriticalEventKind` to `alert_raised:`/`alert_resolved:`.
- [X] T044 [US4] Record non-alert events in `gsm-sip-bridge/src/metrics/ingest.rs` `apply_event`, next to the existing counters:
  - `TunnelRecovered` → `tunnel_recovered` (detail: reason, plus P-CSCFs if any);
  - additionally `dead_peer_loss` when `dead_peer == Some(Real)`;
  - `UplinkHoldStarted` → `uplink_hold_started` (detail: cause);
  - `UplinkHoldEnded` → `uplink_hold_ended` (`duration_seconds = held_secs`, detail: cause and `ended_by`).
- [X] T045 [US4] Record `module_initialized` and `module_init_failed` (with the error text) at the `MODULE_INIT_TOTAL` sites in `gsm-sip-bridge/src/modules/pool/mod.rs`, and `scheduled_restart` at the `SCHEDULED_RESTART_TOTAL` site in `gsm-sip-bridge/src/modules/pool/restart_cycle.rs`.
- [X] T046 [US4] Install the events sink in every writer process:
  - `gsm-sip-bridge/src/commands/daemon.rs` (`store.sender()`);
  - `gsm-sip-bridge/src/ims/agent/mod.rs` (`history_store`);
  - `gsm-sip-bridge/src/vowifi/mod.rs` (its store);
  - `gsm-sip-bridge/src/supervise/orchestrate.rs`, which opens a `StoreHandle` on `config.sms.db_path` next to the `AlertContext` setup. A failure there is logged, and alerts keep working without events.
- [X] T047 [US4] Add retention to `gsm-sip-bridge/src/commands/daemon.rs`: send `StoreCommand::PruneEvents` at startup and every 6h (`tokio::time::interval`), using `config.alerts.event_retention_days`.
- [X] T048 [US4] Implement `list_events(filter, page)` in `gsm-sip-bridge/src/webui/queries.rs` (with an exact or prefix `kind` match) and `GET /api/events` in `gsm-sip-bridge/src/webui/api.rs`. Fill `active_alerts[].since` in `/api/overview` from `store::events::open_raises()`.
- [X] T049 [US4] Build the Events page in `gsm-sip-bridge/src/webui/assets/app.js`:
  - a newest-first timeline with severity styling and line and kind filters;
  - resolutions show "lasted Xm Ys";
  - a health panel fed by `/api/health`.

**Checkpoint**: US4 works on its own.

---

## Phase 8: User Story 5 — Check the configuration the bridge is actually running with (Priority: P3)

**Goal**: A read-only, redacted view of the config document as loaded at startup.

**Independent Test**: With every secret key set to a unique sentinel, and an `env:` value that resolves to a sentinel, no sentinel appears in `/api/config`, while ordinary values do.

- [X] T050 [P] [US5] Write `gsm-sip-bridge/tests/test_webui_config.rs`:
  - load `config.toml.example` with every `Secret<…>`-typed key set to a unique literal sentinel (`SENTINEL-<key>`), plus one key set to `env:WEBUI_TEST_SECRET` with that variable set to another sentinel;
  - assert that `/api/config` contains no sentinel substring;
  - assert that `env:WEBUI_TEST_SECRET` appears literally;
  - assert that non-secret values (e.g. `[metrics].port`) appear unchanged;
  - assert that `note` and `loaded_at` are present.
- [X] T051 [US5] Implement `gsm-sip-bridge/src/webui/config_view.rs`, which converts the pre-resolution `toml::Value` to `serde_json::Value`:
  - keep `env:NAME` strings as written;
  - replace a literal string with `"[REDACTED]"` when its lowercased key contains `password`, `secret`, `token`, `webhook`, `api_key`, `credential` or `auth` (research R11);
  - recurse into tables and arrays of tables.
- [X] T052 [US5] Implement `GET /api/config` in `gsm-sip-bridge/src/webui/api.rs`, and the Config page in `gsm-sip-bridge/src/webui/assets/app.js`: collapsible sections per table, and a banner saying "As loaded at startup; later edits to the file are not shown".

**Checkpoint**: All stories are complete.

---

## Phase 9: Polish & Cross-Cutting Concerns

- [X] T053 [P] Write `gsm-sip-bridge/tests/test_webui_assets.rs`. It fails if:
  - `app.js` contains `innerHTML`, `outerHTML`, `insertAdjacentHTML` or `document.write`;
  - `index.html` contains an inline `<script>` body or a `style=` attribute (CSP);
  - the three assets together exceed 150 KB.
- [X] T054 [P] Add a test in `gsm-sip-bridge/tests/test_metrics_endpoint.rs` that, with `[webui]` absent, the set of metric names on `/metrics` differs from the pre-feature list only by the series in contracts/metrics.md (SC-009).
- [X] T055 [P] Update the documentation:
  - a README section "Web UI": enabling it, the port, the password via `env:`, plain HTTP with a reverse proxy for TLS, and that it is read-only;
  - a `RELEASE_NOTES.md` entry with only the summary in bold;
  - a `CHANGELOG.md` entry.
- [X] T056 Run `make format`, `make lint` and `make test` and fix every finding. Lint covers all test targets.
- [X] T057 Build the image from `git archive HEAD`, not the working tree, because `target-*` dirs bloat the context. Compare `docker images` size against the current `main` image and record the delta, which must be under 1 MB (SC-007), in `specs/122-management-webui/live-verification.md`.
- [ ] T058 Run the live verification in quickstart.md steps 1–8 on the local Vodafone host (CS + VoWiFi) and the Jio Pi (VoWiFi), and record the results in `specs/122-management-webui/live-verification.md`. Step 2 (no EAP-SIM `6F00` during a forced re-registration with signal polling on) is merge-blocking.

---

## Dependencies & Execution Order

### Phase dependencies

- **Setup (Phase 1)** has no dependencies.
- **Foundational (Phase 2)** depends on Setup and blocks every story.
  - T005 needs T004; T006 needs T005.
  - T009 needs T007.
  - T011 needs T010.
  - T012 and T013 are independent of each other and of T004–T011.
- **US6 (Phase 3)** depends on Phase 2. It is the gate for every page, because every route sits behind its middleware.
- **US1, US2, US3, US4 and US5** each depend only on Phase 2 and US6. They don't depend on each other, with these exceptions:
  - T048 (filling `since` in the overview) touches the US1 handler. It is trivial when US1 isn't done yet.
  - T039's CS branch edits `webui/lines.rs` from US1. Do US1 first, or stub the function.
- **Polish (Phase 9)** comes after the stories in scope.

### Within each story

The test task comes first and must fail, then queries/handlers, then producers (agents and pool), then the page in `app.js`. Tasks touching `app.js`, `api.rs` or `queries.rs` are sequential with each other.

### Parallel opportunities

- **Phase 1**: T003 alongside T001/T002.
- **Phase 2**: T012 and T013 alongside the T004→T006 chain and T007→T009.
- **Different stories**: after US6, US2 (SMS), US4's store/producer work (T042–T047) and US5 (T050–T051) touch disjoint files and can proceed in parallel. They meet only at `api.rs`, `queries.rs` and `app.js`, where they must be serialised.
- **Within US1**: T019 and T020 in parallel; T023, T024, T025 and T026 are in disjoint producer files and can run in parallel once T021 exists.
- **Within US3**: T036, T037 and T038 are in disjoint files.

### Parallel example: User Story 1

```text
Task: "T019 Write tests/test_webui_overview.rs"
Task: "T020 derive_health + unit tests in webui/lines.rs"
# after T021:
Task: "T023 IMS agent details in ims/agent/mod.rs"
Task: "T024 VoLTE agent details in volte/carrier_agent.rs"
Task: "T025 CS signal poll in modules/worker.rs"
Task: "T026 IMS/VoLTE signal poll in volte/sms.rs"
```

### Parallel example: User Story 4

```text
Task: "T041 Write tests/test_webui_events.rs"
Task: "T042 store/events.rs"
# after T042:
Task: "T043 hook alerts::dispatch"
Task: "T044 hook ingest apply_event"
Task: "T045 hook modules/pool"
```

---

## Implementation Strategy

### MVP first

1. Phase 1 and Phase 2 (foundation).
2. Phase 3, US6: authenticated shell. **Stop and validate** with `test_webui_auth.rs`.
3. Phase 4, US1: Overview. **Stop and validate** against `vowifi-status` on the Pi. This is the smallest release worth shipping.

### Incremental delivery

1. Add US2 (SMS). That completes P1, so release.
2. Add US3 (Calls, including outbound history) and US4 (Events). Both are P2 and can ship separately.
3. Add US5 (Config, P3).
4. Polish, including the merge-blocking live check T058.

Each story is a sequence of small, green commits (Constitution II/III). One task is typically one commit, and a test task may share a commit with its implementation when the test alone would leave `make test` red.

---

## Notes

- `[P]` means different files and no dependency on an incomplete task.
- Never commit real MSISDNs, IMSIs or ICCIDs. Fixtures use `+919000000000`-style placeholders.
- AT traffic changes (T025, T026) must not add a new port user or a second lock on top of `serialport`'s own.
- Don't narrow `make lint`'s scope. A warning in a test target fails the build.
