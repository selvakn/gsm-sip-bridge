---
description: "Task list for the gsm-sip-bridge v5.0.0 Rust rewrite"
---

# Tasks: Rust Rewrite (gsm-sip-bridge v5.0.0)

**Input**: Design documents from `/specs/008-rust-rewrite/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: Test tasks ARE included. Constitution Principle I (Integration-First Testing) and the Development Workflow's TDD default both require it. Each implementation task is preceded by an integration test task scoped to the same behaviour. Tests are written first and expected to fail before the matching implementation lands (per Constitution: "Write a failing test that defines the desired behavior. Implement the minimum code to make the test pass.").

**Organization**: Tasks are grouped by user story (US1..US5 from spec.md). Each story is independently testable and produces a green checkpoint. The MVP is US1 (P1) — once Setup + Foundational + US1 are done, the bridge delivers core value.

## Format: `[ID] [P?] [Story?] Description`

- **[P]**: Can run in parallel (different files, no dependencies on incomplete tasks)
- **[Story]**: Maps task to user story (US1..US5); omitted in Setup, Foundational, and Polish phases

## Path Conventions

This is a 3-crate Cargo workspace under the existing repo root `audio-echo/`:

- `pjsua-sys/` — bindgen FFI output
- `pjsua-safe/` — safe Rust wrappers around `pjsua-sys`
- `gsm-sip-bridge/` — the binary crate (zero `unsafe`)

All paths below are relative to the repo root.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Prepare the Rust workspace alongside the existing v4.1.x C++ source, then cut over.

- [x] T001 Create the Cargo workspace at the repo root: `Cargo.toml` listing members `pjsua-sys`, `pjsua-safe`, `gsm-sip-bridge`; create empty crate skeletons (each with `Cargo.toml` and a placeholder `src/lib.rs` or `src/main.rs`); add `rust-toolchain.toml` pinning a recent stable; add `Cargo.lock` to git
- [x] T002 [P] Replace the repo-root `Makefile` with one that wraps `cargo`. Required targets: `build`, `test`, `run`, `clean`, `lint`, `format`, `dev`, `dev-gsm`, `dev-sip`, `docker-build`, `coverage`, `help`. Each target gets a one-line description visible from `make help` (Constitution Principle IV)
- [x] T003 [P] Add `deny.toml` at the repo root configuring `cargo-deny` to flag advisory CVEs and licenses outside of `MIT/Apache-2.0/BSD-*/ISC/Unicode-DFS-2016`
- [x] T004 [P] Update `.github/workflows/` CI definitions to invoke `make test` and `make lint` on push and pull request; remove CMake/C++ build steps; preserve the publish workflow's release-notes extraction
- [x] T005 [P] Update `.gitignore` for Rust artefacts (`target/`, `Cargo.lock` rules already in place from T001), and remove C++ build artefact ignores (`build/`)
- [x] T006 Delete the v4.1.x C++ source tree and its build wiring in a single commit: remove `CMakeLists.txt`, `src/` (all .cpp/.h files), `tests/integration/` (all .cpp test files), and `build/`. Preserve `etc/`, `screenshots/`, `config.ini.example` (kept for migration-guide reference). Their content remains accessible at git tag `v4.1.1`
- [x] T007 [P] Create stub `README.md` for v5.0.0 referencing this plan and the upcoming migration guide; the full rewrite lands at T101 in Polish

**Checkpoint**: `make build` and `make test` succeed against three empty Rust crates.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Plumbing every user story relies on — config, CLI, runtime, logging+redaction, persisted store skeleton, metrics registry, error types, test fixtures.

**⚠️ CRITICAL**: No user story work begins until this phase is complete.

### Foundational tests (write first, expect failures)

- [x] T008 [P] Test fixture: `gsm-sip-bridge/tests/common/mod.rs` exposing `temp_store()`, `wiremock_server()`, `null_alsa_device()` helpers (no real assertions yet; just makes the rest compile)
- [x] T009 [P] Test fixture: `gsm-sip-bridge/tests/common/pty.rs` — `socat`-driven PTY pair where the test side scripts AT command responses; exposes `PtyHarness::expect("AT+CSQ").reply("+CSQ: 22,99\r\nOK\r\n")`
- [x] T010 [P] Test fixture: `gsm-sip-bridge/tests/common/pbx.rs` — localhost SIP loopback PBX harness on an ephemeral UDP port; supports scripted responses (`200 OK`, `486 Busy`, `408 Timeout`, `503 Service Unavailable`)
- [x] T011 [P] Test: `gsm-sip-bridge/tests/test_config.rs` — load full and minimal `config.toml` fixtures; verify env-reference resolution (`env:VAR`); verify failure modes from contracts/config.toml.schema.md (missing field, out-of-range, unset env var, unknown key warns)
- [x] T012 [P] Test: `gsm-sip-bridge/tests/test_logging.rs::redaction` — boot the redaction layer with a known fake `sip.password = "secret123"`; emit a verbose log including the value via `tracing::error!(?config)`; capture all log output; assert `secret123` is absent and `[REDACTED]` is present
- [x] T013 [P] Test: `gsm-sip-bridge/tests/test_cli.rs` — parse representative argv arrays; verify `--config` required; verify `-s`/`-a` paired-or-neither rule; verify exit-code-2 path on unknown flag

### Foundational implementation

- [x] T014 [P] Crate-wide error types in `gsm-sip-bridge/src/error.rs` — `BridgeError` enum (Config, Sip, Audio, Store, Metrics, Discovery, Sms variants); implements `std::error::Error` + `From` impls for crate-internal sources
- [x] T015 [P] Logging in `gsm-sip-bridge/src/observability/logging.rs` — initialise `tracing-subscriber` with env filter, plus a `RedactionLayer` that scans field names for `password`, `webhook_url`, `auth.*`, `secret`, `token` and replaces values with `[REDACTED]`; verbose mode flips the global filter to `debug,gsm_sip_bridge=trace,pjsua_safe=debug`
- [x] T016 [P] `Secret<T>` newtype in `gsm-sip-bridge/src/config/secret.rs` — `Debug`/`Display` return `[REDACTED]`; `expose_secret(&self) -> &T` for explicit access
- [x] T017 [P] Config loader in `gsm-sip-bridge/src/config/mod.rs` — serde structs matching `contracts/config.toml.schema.md`; resolve `env:VAR_NAME` references at load; validate ranges; emit `WARN` on unknown keys; secret-bearing fields use `Secret<String>`
- [x] T018 [P] CLI parser in `gsm-sip-bridge/src/cli.rs` — `clap` v4 derive matching `contracts/cli.md`; reject any attempt to add a secret-bearing flag (compile-time review item)
- [x] T019 [P] ModemManager detection in `gsm-sip-bridge/src/observability/modemmanager.rs` — checks `systemctl is-active ModemManager` (best-effort) and presence of `/run/dbus/system_bus_socket` to detect; logs `WARN` with the documented remedy when active
- [x] T020 Tokio runtime bootstrap in `gsm-sip-bridge/src/runtime.rs` — multi-thread runtime construction; SIGTERM/SIGINT handler that triggers graceful-shutdown signal channel; 10-second grace period for in-flight DB writes and Discord posts before forced exit (depends on T014)
- [x] T021 [P] Persisted store schema in `gsm-sip-bridge/src/store/schema.rs` — runs the SQL from `contracts/db.schema.sql` on first init; checks/inserts `meta('schema_version', '1')`; refuses to start if a foreign schema_version is present
- [x] T022 Persisted store writer thread in `gsm-sip-bridge/src/store/mod.rs` — single dedicated `std::thread` owns the `rusqlite::Connection`; receives work items over `crossbeam::channel`; emits `gsm_sip_bridge_store_writes_total` and `gsm_sip_bridge_store_queue_depth` (depends on T014, T021, T024)
- [x] T023 [P] Empty per-table CRUD skeletons in `gsm-sip-bridge/src/store/calls.rs` and `gsm-sip-bridge/src/store/sms.rs` — `insert_call(&mut Conn, CallRecord)` and `insert_sms(&mut Conn, SmsRecord)` plus `update_sms_forwarding(...)` signatures; bodies implemented in their respective user-story phases
- [x] T024 [P] Metrics registry skeleton in `gsm-sip-bridge/src/metrics/mod.rs` — global `Registry`, `lazy_static`/`once_cell` registration helpers, `build_info` gauge with version+git_sha+pjsip_version+rust_version labels; metrics defined per `contracts/metrics.md` are *registered* here even though their values are produced from individual subsystems

**Checkpoint**: `make test` runs T011..T013 against the foundational implementation; all three pass. The bridge can load config, parse CLI, init logging with redaction, but does no real work yet.

---

## Phase 3: User Story 1 — Bridge GSM Calls to SIP With Identical Behavior to v4.1.x (Priority: P1) 🎯 MVP

**Goal**: Auto-answer GSM calls on every connected EC20 module, dial SIP, bridge audio bidirectionally, handle multi-card and module failure/recovery, hit ≤200 ms p95 mouth-to-ear latency.

**Independent Test**: With one or more EC20 modules connected and a SIP PBX configured, dial a GSM number landing on a connected module. Verify auto-answer, beep during dial, SIP INVITE with correct DID, bidirectional audio after SIP answer, clean termination either side, simultaneous calls across modules without cross-talk.

### pjsua-sys (FFI)

- [x] T025 [US1] `pjsua-sys/build.rs` — `pkg-config` probe asserting `libpjproject >= 2.14`; bindgen invocation generating `bindings.rs` from `pjsua.h` and `pjmedia/sound_port.h`; emit `cargo:rustc-link-lib` directives for `pjsua`, `pjsip`, `pjmedia`, `pj`, `pjlib-util`, plus the SSL/SRTP/UUID transitive deps
- [x] T026 [US1] `pjsua-sys/src/lib.rs` — `include!(concat!(env!("OUT_DIR"), "/bindings.rs"));` plus a build smoke test compiling against the FFI symbols

### pjsua-safe (safe wrappers — every `unsafe` block carries `// SAFETY: ...` per FR-080)

- [x] T027 [P] [US1] Test: `pjsua-safe/tests/smoke.rs` — boot an `Endpoint`, configure a UDP transport, register an `Account` against the localhost SIP loopback (from T010), assert registration succeeds within 5 s
- [x] T028 [P] [US1] `pjsua-safe/src/error.rs` — `PjsipError` enum mapping every `pj_status_t` we care about; `pj_status_to_str` helper
- [x] T029 [P] [US1] `pjsua-safe/src/log_bridge.rs` — `pjsua_logging_config` callback that forwards every PJSIP log line to `tracing` under target `sip`
- [x] T030 [US1] `pjsua-safe/src/endpoint.rs` — `Endpoint::create(EndpointConfig)` wrapping `pjsua_create` + `pjsua_init` + `pjsua_transport_create` (UDP/TCP/TLS) + `pjsua_start`; `Drop` impl calls `pjsua_destroy`; honours `tls_verify=strict|skip` from R-03 (depends on T028, T029)
- [x] T031 [US1] `pjsua-safe/src/account.rs` — `Account::register(&Endpoint, AccountConfig)` returning a typed handle; expose registration callbacks via a `RegistrationListener` trait (depends on T030)
- [x] T032 [US1] `pjsua-safe/src/call.rs` — outbound `Endpoint::make_call(account, dest_uri)` returning a `Call`; `Call::hangup`, `Call::conf_slot()`; expose call state via `CallStateListener` (depends on T031)
- [x] T033 [US1] `pjsua-safe/src/audio_media_port.rs` — implement `pjmedia_port` with a custom frame callback; safe trait `AudioMediaPort { fn read_frame(&mut self, buf: &mut [i16]); fn write_frame(&mut self, buf: &[i16]); }`; expose `register_to_conf_bridge(&Endpoint) -> SlotId` (depends on T030)

### USB / Serial / AT / Audio (gsm-sip-bridge crate, zero `unsafe`)

- [x] T034 [P] [US1] Test: `gsm-sip-bridge/tests/test_discovery.rs` — fake the USB scan via a trait + injectable test impl; verify vendor/product matching (`0x2c7c:0x0125`), stable ID derivation (`ec20-` + uppercase last 6 hex of USB serial), AT-port-vs-audio-device mapping
- [x] T035 [P] [US1] Test: `gsm-sip-bridge/tests/test_at_commander.rs` — drive the AT commander against a PTY (T009); verify happy paths (CSQ, COPS, CMGR, CMGD, CHUP, ATA), error paths (`+CME ERROR`), and timeout
- [x] T036 [P] [US1] Test: `gsm-sip-bridge/tests/test_beep_generator.rs` — verify the 400 Hz sine fills a buffer with the expected amplitude/period; verify it produces silence after a stop signal
- [x] T037 [P] [US1] Test: `gsm-sip-bridge/tests/test_audio_pipeline.rs` — connect the SPSC ring buffer between an in-memory producer (simulating ALSA capture) and consumer (simulating PJSIP read); verify zero-loss steady state and that overrun is counted in metrics
- [x] T038 [P] [US1] `gsm-sip-bridge/src/modules/discovery.rs` — `rusb`-based scan; vendor/product match; serial-number derivation; AT-port vs ALSA card mapping; supports the single-card override path (matching `--serial` to a discovered device)
- [x] T039 [P] [US1] `gsm-sip-bridge/src/modules/at_commander.rs` — `serialport`-based AT commander; per-command timeout; line-oriented `OK`/`ERROR`/`+CME ERROR`/`+CMTI`/`RING` parsing; trace logging under target `at`
- [x] T040 [P] [US1] `gsm-sip-bridge/src/modules/beep.rs` — pure-Rust 400 Hz sine generator; produces 16-bit signed mono frames at 8 kHz
- [x] T041 [US1] `gsm-sip-bridge/src/modules/audio_pipeline.rs` — per-module ALSA capture and playback threads; `crossbeam_queue::ArrayQueue` for SPSC frame transfer; counts underrun/overrun into `gsm_sip_bridge_audio_errors_total` (depends on T024)
- [x] T042 [US1] `gsm-sip-bridge/src/sip/alsa_media_port.rs` — implements `pjsua_safe::AudioMediaPort` reading from the playback ring and writing to the capture ring; bridges to the conference slot returned by `Call::conf_slot()` (depends on T033, T041)

### Bridge logic (multi-card pool, RING handling, beep, retry)

- [x] T043 [P] [US1] Test: `gsm-sip-bridge/tests/test_card_pool.rs` — start CardPool with 3 PTY-backed modules, two of which fail init; assert pool starts with 1 functional, retries every 30 s (advance time via test clock), and rejoins recovered modules
- [x] T044 [P] [US1] Test: `gsm-sip-bridge/tests/test_sip_registration.rs` — SipBridge registers against the loopback PBX (T010); registration loss + recovery emits the expected metric increments
- [x] T045 [US1] Test: `gsm-sip-bridge/tests/test_bridge_call.rs` — full GSM↔SIP bridge: PTY scripts a RING + CLIP, the bridge plays beep, INVITEs the loopback PBX, PBX sends `200 OK`, audio frames flow in both directions for 5 seconds, GSM-side hangup tears down SIP cleanly. Covers US1 acceptance scenarios 1, 3, 4, 7
- [x] T046 [US1] Test: `gsm-sip-bridge/tests/test_bridge_call.rs::sip_unreachable` — same setup with PBX scripted to ignore INVITE; assert dial-timeout produces an error indication to GSM caller and the call is recorded with `status=failed` (covers US1 scenario 7)
- [x] T047 [US1] Test: `gsm-sip-bridge/tests/test_end_to_end.rs::three_cards_concurrent` — three PTY-backed modules each receive a RING simultaneously; assert three independent SIP INVITEs, three audio bridges, and one teardown does not affect the others (covers US1 scenario 2)
- [x] T048 [US1] `gsm-sip-bridge/src/modules/card.rs` — `CardInstance` state machine: Idle → Ringing → Answering → Bridged → Cleanup; emits `gsm_sip_bridge_calls_total{status=incoming|answered|missed}` and `gsm_sip_bridge_active_calls{module}` (depends on T039, T040, T041, T042)
- [x] T049 [US1] `gsm-sip-bridge/src/modules/mod.rs` — `CardPool`: detection at startup, dispatch RING events to the matching `CardInstance`, retry-thread for failed modules at the configured interval, emits `gsm_sip_bridge_module_init_total`, `_module_retries_total`, `_modules_active`, `_modules_failed` (depends on T038, T048)
- [x] T050 [US1] `gsm-sip-bridge/src/sip/mod.rs` — `SipBridge`: owns the `Endpoint` + `Account`; on RING from a CardInstance, computes the SIP destination URI (DID passthrough vs fixed `[bridge].sip_destination`), starts a `Call`, watches for state changes, swaps the AudioMediaPort in/out at answer/teardown; emits `gsm_sip_bridge_sip_calls_total`, `_sip_registrations_total`, `_sip_registered`, `_call_duration_seconds` (depends on T031, T032, T042)
- [x] T051 [US1] `gsm-sip-bridge/src/main.rs` — wire CLI + config + runtime + logging + CardPool + SipBridge + metrics writer-thread; honour single-card override; log the v4.1.x-style startup summary; on graceful shutdown, hang up active calls and unregister SIP (depends on all the above)
- [x] T052 [P] [US1] `gsm-sip-bridge/src/bin/gsm_echo.rs` — debug bin: single-card GSM-only audio loopback (capture → playback through SPSC ring; no SIP)
- [x] T053 [P] [US1] `gsm-sip-bridge/src/bin/sip_echo.rs` — debug bin: single-card SIP-only audio loopback (PJSIP echo to the registered account; no GSM)
- [x] T054 [P] [US1] Test: `gsm-sip-bridge/tests/test_card_pool.rs::failed_recovery` — module fails at init, succeeds on retry, joins active pool without process restart (US1 scenarios 5, 6)

**Checkpoint**: With one or more EC20 modules attached (or PTY-backed in tests), the bridge auto-answers, dials SIP, bridges audio, handles multi-card. MVP delivered. Tests covering US1 scenarios 1–7 pass; scenario 8 (latency p95) is validated in Phase 8.

**Hardware Smoke Test (Phase 3)**: Deploy to real hardware with at least one EC20 module. Verify manually:
1. PJSIP enumerates the correct ALSA device (check logs for `sound device set` with matching card name)
2. SIP registration succeeds (logs show `registered` state, not "not yet wired")
3. Incoming GSM call is answered and a SIP INVITE is sent (verify with PBX logs or `tcpdump`)
4. Audio flows bidirectionally (speak into GSM phone, hear on SIP side and vice versa)
5. GSM hangup propagates to SIP; SIP hangup propagates to GSM (AT+CHUP issued)
6. Ringback tone is audible to GSM caller while SIP is dialing

---

## Phase 4: User Story 2 — Capture Incoming SMS Reliably and Forward to Discord (Priority: P2)

**Goal**: SMS arrives → persist to store → delete from SIM → post Discord webhook (if configured). Never disrupt active calls.

**Independent Test**: Send an SMS to the SIM in a connected module; verify Discord embed within seconds, row in the SMS store, SIM cleared. Verify failure path with unreachable webhook still persists and clears SIM.

### Tests for US2

- [x] T055 [P] [US2] Test: `gsm-sip-bridge/tests/test_sms_reader.rs` — script `+CMTI` notification on PTY, then `+CMGR` response with PDU-encoded SMS; assert decoder returns expected sender/body; assert `+CMGD` delete is sent after persistence; cover concatenated SMS reassembly across two CMGR responses
- [x] T056 [P] [US2] Test: `gsm-sip-bridge/tests/test_sms_discord.rs` — drive the Discord client against `wiremock`: assert the JSON body shape from `contracts/discord-webhook.md`; assert retry/backoff against scripted 429 (with and without Retry-After) and 503; assert UTF-8 emoji body survives round-trip; assert 4097-char body is truncated to 4090 + `…`; assert URL never appears in captured logs
- [x] T057 [US2] Test: `gsm-sip-bridge/tests/test_sms_handler.rs` — full SMS path: PTY scripts an arrival, store row written with `forwarding_status=pending`, `wiremock` returns 200, status transitions to `sent`, SIM `+CMGD` was sent in correct order. Covers US2 scenarios 1, 4
- [x] T058 [US2] Test: `gsm-sip-bridge/tests/test_sms_handler.rs::during_call` — a bridged call is active on a module when an SMS arrives on the same module; assert audio frames continue without disruption and SMS still completes (US2 scenario 2)
- [x] T059 [US2] Test: `gsm-sip-bridge/tests/test_sms_handler.rs::discord_unreachable` — `wiremock` returns 503 on every attempt; assert row reaches `forwarding_status=failed` after 3 retries, SIM is still cleared, bridge keeps operating (US2 scenario 3)

### Implementation for US2

- [x] T060 [P] [US2] `gsm-sip-bridge/src/sms/reader.rs` — listens for `+CMTI` URC on the AT commander; reads via `+CMGR`; decodes PDU mode; reassembles concatenated SMS; deletes via `+CMGD` AFTER persistence (FR-031 ordering)
- [x] T061 [P] [US2] `gsm-sip-bridge/src/sms/discord.rs` — `reqwest` client with `rustls-tls`; embed payload builder per `contracts/discord-webhook.md`; retry loop honouring `Retry-After`; total time budget 30 s; never logs the URL
- [x] T062 [US2] `gsm-sip-bridge/src/sms/mod.rs` — `SmsHandler`: per-module tokio task pulling SMS events from the AT commander; persists via the store writer thread (T022) before spawning an independent forward task; emits `gsm_sip_bridge_sms_received_total`, `_sms_forwarded_total{outcome}`, `_sms_db_writes_total{outcome}` (depends on T060, T061, T022, T023)
- [x] T063 [US2] Wire `SmsHandler` into `main.rs`: one handler instance receiving `module_id` + AT commander handle from each `CardInstance`; respects `[sms].enabled = false` (skip path) and empty `discord_webhook_url` (skipped status)
- [x] T064 [US2] Implement `gsm-sip-bridge/src/store/sms.rs::insert_sms` and `update_sms_forwarding` against the schema in `contracts/db.schema.sql`

**Checkpoint**: SMS path works end-to-end. US1 (calls) is unaffected. Tests covering US2 scenarios 1–4 pass.

**Hardware Smoke Test (Phase 4)**: With a real EC20 module and active call bridging:
1. Send an SMS to the SIM — verify Discord embed appears with correct sender/body/timestamp
2. Send an SMS during an active call — verify call audio is not disrupted
3. Kill network to Discord — verify SMS is persisted locally with `failed` status and SIM is cleared

---

## Phase 5: User Story 3 — Observe System Health Through Metrics and a Dashboard (Priority: P2)

**Goal**: Expose `/metrics` in Prometheus exposition format with all metrics from `contracts/metrics.md`, ship a Grafana dashboard, ensure scraping never disrupts call processing.

**Independent Test**: With the bridge running, fetch `/metrics` and verify documented metric names appear with plausible values; the included Grafana dashboard renders without missing-data warnings; eight concurrent calls + tight scrape loop produce no audio underruns.

### Tests for US3

- [x] T065 [P] [US3] Test: `gsm-sip-bridge/tests/test_metrics_endpoint.rs` — start the bridge with a single PTY-backed module; complete one call; assert every metric from `contracts/metrics.md` appears in the `/metrics` body with the expected type (counter/gauge/histogram), and that `gsm_sip_bridge_build_info` carries non-empty version/git_sha/pjsip_version/rust_version labels
- [x] T066 [P] [US3] Test: `gsm-sip-bridge/tests/test_metrics.rs::scrape_under_load` — 8 PTY-backed modules with active calls; spawn a tight scrape loop hitting `/metrics` every 10 ms for 30 s; assert no audio underruns reported in the audio_errors counter and every scrape returns within 1 s (validates FR-051 + SC-010 partially)
- [x] T067 [P] [US3] Test: `tests/test_metric_renames.rs` — load `contracts/metrics.md` and assert every v4.1.x metric in the rename table has a corresponding v5.0.0 metric registered in the running bridge

### Implementation for US3

- [x] T068 [P] [US3] `gsm-sip-bridge/src/metrics/server.rs` — `axum` server bound to `[metrics].port` (or `METRICS_PORT` env var if set); single route `GET /metrics` returning `prometheus::TextEncoder` output with `Content-Type: text/plain; version=0.0.4`
- [x] T069 [US3] Wire `metrics::server` into the runtime as a long-running tokio task launched from `main.rs`; bind error is fatal and produces exit code 1
- [x] T070 [P] [US3] Process collector: enable the `prometheus::process_collector::ProcessCollector` so `process_*` and runtime metrics are exposed (recorded in research.md "open items")
- [x] T071 [P] [US3] `docker/grafana/provisioning/dashboards/gsm-sip-bridge.json` — full dashboard JSON with panels for system overview, GSM and SIP call rates, active calls per module, call duration percentiles (p50/p95/p99), SIP registration timeline, module health and retry counts, audio and SIP error rates, SMS forwarding outcomes (FR-052)
- [x] T072 [P] [US3] `docker/grafana/provisioning/datasources/prometheus.yml` — Prometheus datasource provisioning pointing at `prometheus:9090`
- [x] T073 [P] [US3] `docker/grafana/provisioning/dashboards/dashboard.yml` — provisioning manifest pointing at the JSON file
- [x] T074 [P] [US3] `docker/prometheus.yml` — scrape config: 15 s interval, single target `gsm-sip-bridge:9091`

**Checkpoint**: Metrics endpoint works under load; Grafana renders. Tests for US3 scenarios 1–3 pass.

---

## Phase 6: User Story 4 — Persist Calls and SMS for After-the-Fact Inspection (Priority: P2)

**Goal**: Every call and SMS is durably persisted; data survives restarts; operators can inspect via a SQL CLI.

**Independent Test**: Run the bridge, complete one answered call and one missed call, receive one SMS; stop the bridge; verify rows on disk; restart; verify rows still readable and new activity appends correctly.

### Tests for US4

- [x] T075 [P] [US4] Test: `gsm-sip-bridge/tests/test_store_calls.rs` — write `answered`/`missed`/`failed` call records via the writer thread; query each by status, by module_id, by started_at range; verify the `recent_calls` view returns newest-first
- [x] T076 [P] [US4] Test: `gsm-sip-bridge/tests/test_store_sms.rs` — insert pending SMS; transition to `sent` (with `discord_status_code=200`, non-null `forwarded_at`), `failed` (with `discord_status_code=503`), `skipped` (no Discord call); verify all three terminal states are persisted correctly
- [x] T077 [P] [US4] Test: `gsm-sip-bridge/tests/test_store_restart.rs` — write rows; close connection; reopen; assert all rows readable and the `meta(schema_version)` row is preserved
- [x] T078 [P] [US4] Test: `gsm-sip-bridge/tests/test_store_concurrent_writers.rs` — submit 1000 mixed call+sms writes concurrently from many tokio tasks; assert all 1000 land in the store with no `SQLITE_BUSY` errors (validates the single-writer-thread invariant)

### Implementation for US4

- [x] T079 [US4] Complete `gsm-sip-bridge/src/store/calls.rs::insert_call` (skeleton from T023) — full implementation including the index-friendly status enum encoding
- [x] T080 [P] [US4] `docker/docker-compose.yml` — add `sqlite-web` service exposing port 8088, mounted read-only on the bridge's `db_path` volume
- [x] T081 [P] [US4] `docs/operations.md` — operator-facing pages: how to query the store via `sqlite3` CLI; copy-pasteable manual prune SQL (FR-042); WAL/checkpoint guidance; backup recipe

**Checkpoint**: Persistence and inspection work end-to-end. Tests for US4 scenarios 1–3 pass.

---

## Phase 7: User Story 5 — Migrate From v4.1.x in a Single Documented Step (Priority: P3)

**Goal**: A complete, audit-able migration document covering configuration, database, metrics, CLI, and Docker Compose. No migration CLI ships (R-15 / spec Q5 = Option A).

**Independent Test**: Take a working v4.1.x deployment (config.ini, populated sms.db, Grafana dashboard, running Docker Compose stack). Follow the migration guide entirely by hand. Verify the new release starts and the operator can see both pre- and post-migration records in a single store.

### Tests for US5

- [x] T082 [P] [US5] Test: `gsm-sip-bridge/tests/test_migration_guide.rs` — parse `docs/migrating-from-v4.1.x.md`; assert every metric in the v4.1.x→v5.0.0 rename table from `contracts/metrics.md` appears in the doc's "Metrics rename mapping" section; assert every `[sip]`/`[bridge]`/`[sms]` INI key from `config.ini.example` has a corresponding TOML row in the doc's "Configuration" table
- [x] T083 [P] [US5] Test: `gsm-sip-bridge/tests/test_migration_sql.rs` — fixture: a v4.1.x-shaped sms.db file with sample rows; run the doc's copy-pasteable SQL against it; assert the resulting store.db conforms to the v5.0.0 schema and contains all original rows (US5 scenario 1)
- [x] T084 [P] [US5] Test: same `test_migration_sql.rs::idempotent` — running the SQL twice produces no errors and no duplicates (operator may re-run)
- [x] T085 [P] [US5] Test: `tests/test_migration_sql.rs::nondestructive` — assert the original `sms.db` file is byte-identical after migration runs (US5 scenario 3)

### Implementation for US5

- [x] T086 [US5] `docs/migrating-from-v4.1.x.md` — full guide with sections: Overview / Configuration mapping (INI→TOML side-by-side, `env:` reference syntax explained) / Database conversion (copy-pasteable SQL: CREATE new tables, INSERT…SELECT from old, INDEX, VACUUM) / Metrics rename mapping / CLI flag mapping / Docker Compose new file shown verbatim / Roll-back procedure (US5 acceptance scenarios 1, 2, 3)

**Checkpoint**: Migration guide is auditable and the SQL works against a v4.1.x fixture.

---

## Phase 8: Polish & Cross-Cutting Concerns

**Purpose**: Validate the success criteria, lock down quality gates, finalise operator-facing artefacts.

### Performance & success-criteria validation

- [x] T087 [P] Establish v4.1.x baseline: build the v4.1.1 binary (`git checkout v4.1.1 && make build`), run a 30-minute three-call load on the documented test rig, capture latency / CPU / RSS; record numbers in `docs/baselines/v4.1.x.md` (informs SC-003 and SC-004)
- [x] T088 [P] Test: `gsm-sip-bridge/tests/test_end_to_end.rs::latency_p95` — measure mouth-to-ear one-way latency across many talk-spurts using the loopback rig; assert p95 ≤ 200 ms (validates SC-003 and US1 scenario 8)
- [x] T089 [P] Test: `gsm-sip-bridge/tests/test_end_to_end.rs::eight_card_stress` — eight PTY-backed modules + eight concurrent loopback PBX calls held for ≥5 minutes; assert no audio underrun on any module and the latency target still holds (validates SC-010 and FR-014)
- [x] T090 [P] Test: `gsm-sip-bridge/tests/test_end_to_end.rs::cpu_memory_baseline` — under the same load as T087, assert steady-state CPU and RSS are no worse than the recorded baseline (validates SC-004)

### `unsafe` audit

- [x] T091 [P] `tools/count-unsafe.sh` — script that counts `unsafe` blocks per crate (excluding generated `bindings.rs`); exits non-zero if `gsm-sip-bridge/src/**` contains any `unsafe`; reports the ratio for `pjsua-safe` against SC-009's 5% threshold
- [x] T092 Wire `tools/count-unsafe.sh` into `make lint` so CI fails on regressions
- [x] T093 [P] `make coverage` integration: `cargo llvm-cov --workspace --all-features --lcov --output-path lcov.info`; CI uploads to coverage service; threshold ≥90% lines (Constitution Principle I)
- [x] T094 [P] CI step: assert every `unsafe` block in `pjsua-safe/src/**` has a `// SAFETY:` comment on the same or preceding line (FR-080)

### Docker, deployment, and operator artefacts

- [x] T095 [P] `docker/Dockerfile` — multi-stage build: stage 1 builds PJSIP from a pinned source tarball; stage 2 builds the Rust workspace using the PJSIP build; stage 3 runtime image with just the binary, the Grafana JSON, the udev rule, and `libpjproject.so.2`
- [x] T096 [P] `docker/docker-compose.yml` — bridge + Prometheus + Grafana + sqlite-web, host network mode, `restart: unless-stopped`, volumes for `db_path` and config, `env_file:` for secrets
- [x] T097 [P] `etc/99-ec20-gsm-sip-bridge.rules` — udev rule preserved from v4.1.x; verify still functional with the new binary name
- [x] T098 [P] `docs/configuration.md` — operator-facing reference cross-linking `contracts/config.toml.schema.md`; example `config.toml` for common deployment shapes (single-card, multi-card, TLS PBX, SMS-disabled, fixed extension vs DID passthrough)

### Documentation finalisation

- [x] T099 [P] Final pass on `docs/operations.md` (started in T081): runbook entries for "module shows FAILED at startup", "SIP registration failing", "Discord forwarding failing", "metrics endpoint returns 5xx", "store.db corrupt"
- [x] T100 [P] `docs/baselines/v4.1.x.md` (from T087) committed and linked from quickstart.md
- [x] T101 Rewrite `README.md` for v5.0.0: prereqs (Rust toolchain), quick-start (clone → make build → make test → make run), 12-feature overview (matches v4.1.x README sections), Docker Compose section, configuration excerpt, links to specs/008-rust-rewrite/spec.md and migration guide. Supersedes the placeholder from T007

### Quality gates

- [x] T102 Cross-validate parity (SC-001): for each of specs `001-gsm-audio-echo` through `006-sms-discord-forward`, walk every acceptance scenario and confirm a matching test or runbook check exists in this implementation; record the cross-walk in `specs/008-rust-rewrite/parity-check.md`
- [x] T103 Final `make lint && make test && make coverage` clean run on a fresh checkout; fix any drift; confirm coverage ≥90%
- [x] T104 Run `quickstart.md` end-to-end on a fresh Linux VM; time it; if >60 minutes, fix the slow steps until it lands under 60 (validates SC-008)
- [x] T105 Final commit: bump version to `5.0.0` across `Cargo.toml` files, tag `v5.0.0-rc1`, push for ultrareview / CI

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies — start immediately.
- **Foundational (Phase 2)**: Depends on Setup. Blocks all user stories.
- **User Story 1 (Phase 3, P1)**: Depends on Foundational. The MVP. No dependencies on other user stories.
- **User Story 2 (Phase 4, P2)**: Depends on Foundational + a working `CardInstance`+`AT commander` from US1 (T039, T048). Implementation work in Phase 4 reuses these.
- **User Story 3 (Phase 5, P2)**: Depends on Foundational + the metric registrations that subsystems emit in their own phases (US1 emits call/SIP/audio/module metrics in Phase 3; US2 emits SMS metrics in Phase 4). The metrics *server* is in this phase; the metric *values* exist regardless.
- **User Story 4 (Phase 6, P2)**: Depends on Foundational + writes from US1 (calls) and US2 (sms). Most of the heavy lifting is foundational (T021..T024); this phase adds tests and operator-facing docs.
- **User Story 5 (Phase 7, P3)**: Depends on the schema in `contracts/db.schema.sql` and the metric set in `contracts/metrics.md`. Can start any time after Foundational; final validation requires US3 (metric names finalised) and US4 (schema finalised).
- **Polish (Phase 8)**: Depends on all desired user stories.

### User Story Dependencies (cross-story)

- **US1 (P1)**: independent.
- **US2 (P2)**: needs the AT commander + CardInstance from US1; SMS handler attaches per module.
- **US3 (P2)**: needs metrics registered by US1 + US2; the *server* itself is independent.
- **US4 (P2)**: schema + writer thread are foundational; only the operator-facing docs and tests are in this phase. Mostly independent.
- **US5 (P3)**: depends only on the contracts being stable, not on running code.

### Within Each User Story

- Tests are written FIRST and expected to fail (Constitution TDD default).
- For each phase: foundational helpers → leaf modules (e.g., AT commander before CardInstance) → composition (CardPool ties them together) → main.rs wiring.
- Story complete = its checkpoint section's tests pass green.

### Parallel Opportunities

- All [P]-marked tasks within Setup and Foundational run in parallel.
- pjsua-safe modules (T028, T029, T030, T031, T032, T033) have a small sub-dependency chain but five of the six sub-tasks share the [P] property when applied to different files at the same level of the chain (e.g., T028 + T029 + T033 can run in parallel; T030 must come before T031/T032).
- Test tasks within a story (e.g., T034..T037, T055..T057, T065..T067, T075..T078, T082..T085) are all [P] — different files, no shared mutable state.
- Docker/Grafana provisioning tasks (T071..T074, T080, T095..T097) are all [P] — different files.

---

## Parallel Example: User Story 1 sub-batches

```bash
# After T020 (runtime) is done, kick off these in parallel:
Task: T028  # pjsua-safe/src/error.rs
Task: T029  # pjsua-safe/src/log_bridge.rs
Task: T034  # tests/test_discovery.rs
Task: T035  # tests/test_at_commander.rs
Task: T036  # tests/test_beep_generator.rs
Task: T037  # tests/test_audio_pipeline.rs
Task: T040  # src/modules/beep.rs
Task: T038  # src/modules/discovery.rs
Task: T039  # src/modules/at_commander.rs

# Then sequentially because of cross-file dependencies:
Task: T030  # endpoint.rs (depends on T028, T029)
Task: T031  # account.rs (depends on T030)
Task: T032  # call.rs (depends on T031)
Task: T033  # audio_media_port.rs (depends on T030)
Task: T041  # audio_pipeline.rs (depends on T024)
Task: T042  # alsa_media_port.rs (depends on T033, T041)
Task: T048  # card.rs (depends on T039, T040, T041, T042)
Task: T049  # modules/mod.rs (depends on T038, T048)
Task: T050  # sip/mod.rs (depends on T031, T032, T042)
Task: T051  # main.rs (final wiring)
```

---

## Implementation Strategy

### MVP First (US1 only)

1. Phase 1 Setup — workspace, Makefile, CI, delete C++ source.
2. Phase 2 Foundational — config, CLI, runtime, logging+redaction, store skeleton, metrics registry, test fixtures.
3. Phase 3 User Story 1 — pjsua-sys/safe + bridge logic + multi-card pool + audio pipeline.
4. **STOP and VALIDATE**: bridge GSM↔SIP works against real or PTY-backed modules. Latency check (T088) can run here.
5. This is a usable v5.0.0-alpha for early adopters — no SMS, no metrics endpoint, no migration tooling.

### Incremental Delivery

1. MVP (US1) → first internal alpha.
2. Add US3 metrics → ops can observe what's happening (high-leverage next).
3. Add US2 SMS → feature parity for the second-most-used feature.
4. Add US4 persistence-as-product (operator docs + sqlite-web) → "calls and SMS are inspectable" story is whole.
5. Add US5 migration guide → external operators can upgrade.
6. Phase 8 polish → ship v5.0.0.

### Parallel Team Strategy

Once Foundational completes:

- Developer A: US1 (the bulk; ~30 tasks).
- Developer B: US3 metrics server + Grafana dashboard (T065..T074) — independent of US1/US2 implementation.
- Developer C: US5 migration guide (T082..T086) — independent of running code; needs only finalised contracts.

US2 (SMS) and US4 (persistence polish) start after US1's CardInstance is stable.

---

## Notes

- Every commit MUST pass `make test && make lint` (Constitution Principle II — Green-on-Commit).
- Each task is sized for one commit (Constitution Principle III — Frequent Atomic Commits).
- Tests written first, fail, then turn green (Constitution Development Workflow — TDD default).
- `[P]` = different files, no incomplete dependency.
- `[Story]` label maps tasks to user stories (omitted in Setup, Foundational, Polish).
- Verify the failing-test discipline by running `cargo test <test_name>` immediately after writing each test task.
- Avoid: vague tasks, same-file conflicts marked `[P]`, cross-story dependencies that break independence.
