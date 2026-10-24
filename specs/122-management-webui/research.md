# Research: Read-only management web UI

**Feature**: `122-management-webui` | **Date**: 2026-10-24

Each entry gives the decision, the reason for it, and the alternatives that were rejected. Code references are to `gsm-sip-bridge/src/` as of `343c2f2`.

---

## R1. Which process serves the UI

**Decision**: The daemon (`commands/daemon.rs`) serves the UI from a second axum listener, started next to `metrics::server::serve`. A new `webui` module owns the router.

**Rationale**:
- The daemon already holds every live fact the UI needs:
  - per-line agent state (`metrics::ingest::liveness()`, fed by every agent's `AgentReport` over the control socket);
  - CS slot state (`CardPool`, reachable through `ControlCmd::ListSlots`);
  - the metric registries (uptime, build info, store queue depth, modules active/failed).
- `axum` 0.8, `tokio` (full), `serde_json` and `rusqlite` are already dependencies, so there is nothing new to link.

**Alternatives rejected**:
- *A separate `webui` subcommand/process.* It would need its own copy of the line state, either through a new query protocol or by scraping `/metrics` and parsing it. That's more moving parts (Constitution V).
- *Mounting the UI on the metrics router.* Ruled out by clarification Q2: the unauthenticated scrape path and the authenticated UI must be separable.

## R2. Where the line view comes from

**Decision**: The line view is assembled from the daemon's existing ingest records, extended with a new optional `AgentReport.details` struct (contracts/agent-report.md). `AgentState` itself is `Copy`, so it can't carry these fields. The new fields are:

| Field | Purpose |
|---|---|
| `registration_state` | Registration wording |
| `registered_at` | Time registered |
| `signal` | Latest signal reading |
| `active_call_details` | Calls in progress |

CS lines come from `CardPool`'s slot list plus the daemon-local signal map (R4).

The registration wording comes from `ims::RegistrationState`'s existing `Display`. That's the same type `vowifi-status` and `volte-status` print (`volte/registration.rs` FR-022), so the UI and the CLI cannot drift apart (spec FR-012).

**Rationale**: Agents already push a full state snapshot every `agent_report_interval_seconds`, and the daemon already tracks staleness (`evaluate_liveness`, 3× the interval). Adding an optional `#[serde(default)]` field to `AgentReport` is backward compatible: an older agent simply shows "unknown" for the new fields. The details are kept in a separate ingest map, so `AgentRecord` stays `Copy`.

**Alternatives rejected**:
- *Query each agent's status port on page load*, as `vowifi::print_status` does. That adds a synchronous network fan-out per page view. The VoLTE equivalent is a file under `/tmp` (`volte/registration.rs` `DEFAULT_STATUS_PATH`) that is not guaranteed to be visible to the daemon. It also gives two sources of truth for the same state.

## R3. Signal polling for VoWiFi/VoLTE lines

**Decision**: Signal is polled inside the existing modem SMS sweep thread (`volte::sms::run_modem_reader`). That thread already runs every `MODEM_SWEEP_INTERVAL` (20s) for every modem-backed VoWiFi line (`ims/agent/mod.rs:353`) and every VoLTE line (`volte/bridge.rs:350`, `commands/volte.rs:1209`).

Every `ceil(signal_poll_interval / MODEM_SWEEP_INTERVAL)` passes, the sweep:
1. takes `modem_lock`;
2. opens the port once through the existing `open_with_retry`;
3. sends `AT+CSQ` and then `AT+QENG="servingcell"`;
4. releases the port.

The reading is stored in a shared `Arc<Mutex<Option<SignalReading>>>` that the agent's reporter copies into `AgentReport.details.signal`.

A `pcsc_reader` line has no sweep thread and no AT port, so it reports no signal. The UI shows "not available" (spec FR-020 amended accordingly).

**Rationale**:
- This is the only path that already holds `modem_lock`, honours `serialport`'s exclusive open and has a retry budget sized to stay clear of `vowifi-usim-bridge` (see the doc comment at `volte/sms.rs:636-679`).
- A second, independent poller would be a third AT user on the same port. That is exactly the contention class that `serialport-already-locks-dont-double-lock` and the CSIM crash loop came from.
- The sweep is watched by the agent watchdog (`Phase::SmsSweep`), so a wedged signal read is caught the same way a wedged sweep is.

The two commands go in **one** port hold, matching the "one AT round-trip unit" bound the sweep already respects for `AT+CMGF`+`AT+CMGL`.

**Alternatives rejected**:
- *Poll from the daemon by opening the modem port.* The daemon doesn't own VoWiFi/VoLTE ports.
- *Poll from `vowifi-usim-bridge`.* It is an APDU relay with a tight retry budget, and adding unrelated AT traffic there risks EAP-SIM.

## R4. Signal polling for circuit-switched lines

**Decision**: Signal is polled from `modules::worker::ModuleWorker::tick_liveness_probe`. When the signal interval has elapsed **and no call is active** (`call_ctx.is_none()`), the worker sends `AT+CSQ` and `AT+QENG="servingcell"` instead of the bare `AT` probe. A successful `AT+CSQ` counts as liveness, just as `AT` does. The worker:
- sets the new gauges directly (it runs in the daemon process, which owns `/metrics`);
- updates a daemon-local `webui::signal` map keyed by module id.

**Rationale**: The worker owns the CS modem port exclusively and already sends a periodic probe. Re-using that tick adds no new thread and no new port user. Skipping during calls means a poll never queues in front of call-control AT traffic (spec edge case 1).

## R5. Parsing `AT+QENG="servingcell"`

**Decision**: Add a parser next to `volte::pani::parse_serving_cell`, which already parses this response for the cell identity. It extracts RSRP, RSRQ, SINR and the RAT from the LTE form. For any non-LTE or `SEARCH`/`LIMSRV` form, or on a parse failure, the LTE fields are `None`.

RSSI comes from `AT+CSQ` and is converted to dBm as `-113 + 2·rssi`. Code `99` means unknown, so the field is `None` (FR-023).

**Rationale**: One response format, one parsing location. The PANI parser's unit tests already capture real EC20/EC200U response lines, which can seed the fixtures.

## R6. Event persistence: where to hook

**Decision**:
- **Alert raises and resolutions**: recorded inside `alerts::dispatch`. It is the single entry point every category's call site uses, in every process (daemon ingest, `CardPool`, `supervise::orchestrate`'s `AlertContext`).
- **Non-alert events**: recorded in `metrics::ingest::apply_event`, where the matching counters are already incremented. These are:
  - `TunnelRecovered` (with the dead-peer verdict);
  - `UplinkHoldStarted` / `UplinkHoldEnded`;
  - module init success/failure, recorded at the `MODULE_INIT_TOTAL` sites in `modules/pool/mod.rs`;
  - scheduled restart, recorded at the `SCHEDULED_RESTART_TOTAL` site in `modules/pool/restart_cycle.rs`.
- **Sink**: each process that opens a `StoreHandle` installs a process-global sink (`store::events::install_sink(store.sender())`). `supervise` gains a `StoreHandle` for this. A process with no sink installed drops events with a debug log, so a test or CLI path never panics.

**Dedup**: `ingest` retries a Failure whose Discord delivery failed (`record_alert_outcome` → `Idle` → re-decided), so `dispatch` can see the same Failure more than once. The recorder keeps a small in-process map of the open raise per `(category, unit_id)`:
- a Failure while a raise is already open is not recorded again;
- a Recovered with no open raise is not recorded;
- a Recovered with an open raise is linked to it by `raise_id`, and its duration is computed.

A process restart forgets the map. At worst that costs one duplicate raise after a restart, which is acceptable for a timeline.

**Excluded categories**: `Sms` and `MissedCall` are one-shot, and each already has its own row in `sms` and `calls` (spec FR-050 as amended).

**Alternatives rejected**:
- *Persist from Discord delivery outcomes.* That breaks FR-051: disabled or failed categories would vanish.
- *Forward every event to the daemon over the control socket.* `supervise` already fires its alerts locally, and SQLite (WAL plus a 5s busy timeout, `store/mod.rs`) already serves several writer processes.

## R7. Outbound call history

**Decision**: Schema v5 adds `calls.direction TEXT NOT NULL DEFAULT 'inbound' CHECK (direction IN ('inbound','outbound'))`. The default backfills every existing row as inbound, which is accurate because nothing ever stored an outbound row. Rows are written at the existing outcome points:

- **VoWiFi/VoLTE** (`vowifi::mod` outbound loop, the process that already owns a `StoreHandle`, `vowifi/mod.rs:506`), by outcome:

  | Outcome | Recorded as |
  |---|---|
  | `Placed`, then the active call ends | `answered`, duration from answer to end |
  | `Unanswered` / `CallerAbandoned` | `missed` |
  | `RefusedNetworkFailure`, a committed carrier rejection other than unanswered, `RefusedNoIdleLine` | `failed` |
  | `RefusedInvalidDestination` | `failed`, **only** when the request came from a trusted source |

  Untrusted-source 403s are not calls. They are typically SIP scanners, and recording them would flood the log.
- **CS** (`modules/pool/dispatch.rs::handle_outbound_request` and its hangup handling in the daemon), with the same mapping.

`caller_id` keeps its column name and is documented as the remote party: the caller for inbound calls, the dialled number for outbound ones. `module_id` is the line that carried the call, or `''` when the call was refused before a line was chosen.

**Rationale**: This mirrors the existing transport-column migration (v3/v4 in `store/schema.rs`), which used the same "default backfills accurately" argument.

## R8. Per-call media quality

**Decision**:
- **After the call**: schema v5 adds nullable `loss_percent`, `jitter_ms` and `rtt_ms` to `calls`. `ims::observability` already receives `report_media_quality` per source at teardown. It buffers the values and writes them into the same row in `insert_call_row`: local loss and jitter, plus RTT from the remote source.
- **During the call**: the agent's `active_call_details` entry carries the latest quality snapshot, read from the relay's receive statistics (`ims::media_stats::ReceiveStats`) through a shared handle that the call's dispatch loop already holds for the teardown verdict. When no snapshot exists, the fields are `None` and the UI shows "unavailable" (FR-032).
- **CS calls**: no RTP leg on the carrier side, so they always show "unavailable".

**Rationale**: Storing quality per row makes "the call sounded bad" checkable after the fact, which is the more common case. Live quality reuses statistics that are already computed.

## R9. Authentication

**Decision**:
- **Password**: `[webui].password`, a `Secret<String>` that supports `env:`. It is compared in constant time (`subtle::ConstantTimeEq`). `subtle` is already in `Cargo.lock` transitively and becomes a direct dependency of a few KB.
- **Session**: on login, a 32-byte random token (`rand` 0.9, already a dependency), base64url-encoded. It is stored in an in-memory `HashMap<token, expiry>` and sent as `Set-Cookie: gsb_session=…; HttpOnly; SameSite=Strict; Path=/; Max-Age=604800`.
  - There is no `Secure` flag, because the UI is served over plain HTTP by assumption. The quickstart says to add TLS through a reverse proxy.
  - A restart empties the map, which logs everyone out (clarification Q3).
  - Expired sessions are pruned lazily on lookup and by a periodic sweep.
- **Rate limit**: per client IP, 5 consecutive failures lock that IP out for 5 minutes and return `429`. A success resets the counter. The map is bounded at 1024 IPs (oldest evicted) so a scan cannot grow it without bound.
- **CSRF**: only `POST /api/login` and `POST /api/logout` change state. `SameSite=Strict` stops cross-site requests from carrying the session. Every data endpoint is `GET` and read-only.
- **Missing password**: if `[webui].enabled = true` with an empty password, `config::build` rejects the configuration (FR-003). That's a hard error at load time, consistent with how the project treats every other invalid config.

**Alternatives rejected**: HTTP basic auth (clarification Q3). bcrypt/argon2 hashing, which is pointless when the reference value is a plaintext secret in config or the environment and adds dependencies.

## R10. Frontend

**Decision**: Three static files, `index.html`, `app.js` and `style.css`, in `gsm-sip-bridge/src/webui/assets/`, embedded with `include_str!` and served with `Cache-Control: no-cache` and an ETag of the build's git SHA.
- Plain ES2020 JavaScript with no framework and no build step: hash routing across five pages, `fetch` with `credentials: 'same-origin'`.
- DOM is built only with `textContent`/`createElement`, never `innerHTML` (FR-044). A test greps `app.js` for `innerHTML`, `outerHTML`, `insertAdjacentHTML` and `document.write`, and fails if any appears.
- Refresh runs every 5s on the Overview and Calls pages and pauses on `visibilitychange` (FR-015).
- CSS uses custom properties with `prefers-color-scheme`, a single column below 640px, and no web fonts (FR-071/072).
- The OTP regex runs client-side: a standalone `\b\d{4,8}\b`, preferring a match within 40 characters of `otp|code|verification|password|pin` (case-insensitive) (FR-043).
- Responses carry a strict `Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; frame-ancestors 'none'`. It is a second line of defence for FR-044, and the reason the JavaScript and CSS are separate files rather than inline.

**Rationale**: FR-070 (under 1 MB, no Node toolchain). The expected size is about 40–70 KB of assets plus about 150–300 KB of handler code.

## R11. Config page and redaction

**Decision**: At startup the daemon keeps a snapshot of the config document as parsed **before** `env:` resolution (`config::load_config` gets a sibling that returns both the `AppConfig` and the pre-resolution `toml::Value`). The Config page renders that snapshot:
- `env:NAME` references are shown as `env:NAME`. The resolved value never enters the snapshot.
- A literal string whose lowercased key contains any of `password`, `secret`, `token`, `webhook`, `api_key`, `credential` or `auth` is replaced by `"[REDACTED]"`. This is a plain substring check, so the `regex` crate isn't needed.
- A test enforces that this is a default rather than a list someone must maintain. It loads `config.toml.example`, replaces every `Secret<…>`-typed key with a unique literal sentinel, renders the page JSON, and fails if any sentinel appears. A new `Secret` field whose key doesn't match the pattern therefore fails CI.

The page header states that it shows the file as loaded at startup, so later edits to the file are not reflected (spec User Story 5, scenario 2).

**Rationale**: Showing the pre-resolution document means the resolved environment-variable secret is never in memory as page data at all, which is redaction by construction. `AppConfig` is not `Serialize`, and making it so would expose every resolved value.

## R12. New configuration surface

**Decision**: A new `[webui]` table, plus one key each in `[metrics]` and `[alerts]` (contracts/config.md):

| Key | Default | Notes |
|---|---|---|
| `[webui].enabled` | `false` | |
| `[webui].listen_addr` | `"0.0.0.0"` | |
| `[webui].port` | `8080` | |
| `[webui].password` | `""` | required when enabled |
| `[webui].grafana_url` | `""` | optional, rendered as a link |
| `[metrics].signal_poll_interval_seconds` | `60` | `0` disables the poll |
| `[alerts].event_retention_days` | `30` | |

Signal polling and event recording run whether or not the UI is enabled. The gauges are useful to Grafana on their own, and SC-009 already allows for the new gauges. They are cheap, and a timeline that only starts recording when someone turns the UI on would be empty at the moment it is most needed.

A bind failure is logged as an error and the rest of the bridge keeps running (FR-004).

**Docs**: `config.toml.example`, `docs/` config reference and `tests/test_config_docs.rs` follow the existing pattern.

## R13. Store reads from the UI

**Decision**: The UI opens its own read-only `rusqlite::Connection` (`SQLITE_OPEN_READ_ONLY`, `busy_timeout` 5s), held in a `Mutex` and used inside `tokio::task::spawn_blocking`. Queries are always paged (`LIMIT/OFFSET`, page size 50, maximum 200). Search is `LIKE` with the user string bound as a parameter. Counts for "today" and "7 days" use the existing `idx_calls_started_at`.

A database error returns `503` with a JSON error. The Overview page doesn't touch the database, so it keeps working (spec edge case).

**Rationale**: WAL lets readers proceed alongside the single writer thread. Sharing the store's `read_conn` would couple UI latency to startup-path queries. SC-010 (10k rows, under 2s on a Pi) is comfortably met by indexed range scans and a paged `LIKE`.

## R14. Event retention

**Decision**: A new `StoreCommand::PruneEvents { older_than }` is sent by the daemon at startup and every 6h. It deletes rows from `events` where `at < now − retention`. An index on `events(at)` keeps it cheap.

## R15. Testing approach (Constitution I)

**Decision**: Integration tests under `gsm-sip-bridge/tests/`. They run the real axum router on `127.0.0.1:0` against a real temp-file SQLite database, using the real `reqwest` client, which is already a dependency.

- **No mocks.** The ingest registry is driven by real `AgentReport`s through `metrics::ingest::apply_report`, as `test_observability_ingest.rs` already does.
- **Fixtures**: AT parsing (`AT+CSQ`, `AT+QENG`) is tested on captured response strings, and the AT worker path through the existing fake-modem harness in `modules/at_worker.rs` tests.
- **Hardware-dependent checks** go in quickstart.md as live verification: the signal poll on a real EC20, and the absence of contention with `vowifi-usim-bridge` during registration.
