# Release Notes

## v5.2.0

- **Fix Repeated Discovery Log** -- `discovered EC20 module` was logged at INFO every 5 seconds for already-managed modules due to the hotplug rescan. Downgraded to DEBUG; startup visibility is provided by `module initialized` and new hotplug cards by `new module detected`.
- **Hotplug Rescan Interval** -- Increased USB rescan interval from 5 seconds to 60 seconds. Hot-plugging cards is rare and the frequent scan was unnecessary.
- **`--config` Optional for Card Commands** -- `gsm-sip-bridge card <subcommand>` no longer requires `--config`. clap 4.6 did not accept an empty-string default for `PathBuf`, causing a spurious error. The argument is now `Option<PathBuf>`; card commands fall back to the default socket path (`/tmp/gsm-sip-bridge.sock`) when omitted.

## v5.1.0

- **Auto-Recovery** -- Cards automatically reload on USB disconnect or network loss with exponential backoff and per-slot give-up tracking (IMEI-keyed persistence).
- **Startup Diagnostics** -- Phone number and network type logged per card at startup.
- **Unix Socket Control API** -- On-demand daemon management via Unix socket.
- **CLI Card Subcommands** -- `card restart`, `card set-mode`, `card get-mode`, `card list` for runtime card management.
- **SQLite Schema v2** -- `card_slots` and `card_mode_prefs` tables with automatic v1→v2 migration.
- **Network Mode Preferences** -- 2G/4G preferences persisted and re-applied on card initialization.

## v5.0.4

- **gsm-echo ALSA Audio Loopback** -- Added real ALSA capture/playback to `gsm-echo`. Previously, `AT+QPCMV=1,2` routed audio to USB but nothing read or wrote the ALSA device, resulting in silence. Now spawns a dedicated loopback thread (8kHz, S16_LE, mono) on call answer and stops it on hangup, with overrun/underrun recovery.
- **VoLTE Detection** -- `gsm-echo` now queries `AT+QNWINFO` on each incoming call and logs `volte=true/false` based on whether the active RAT is LTE.
- **Docker Build DNS Fix** -- Added `network: host` to docker-compose build config to resolve BuildKit DNS failures reaching package mirrors.
- **EC20 VoLTE Setup Guide** -- Added `docs/ec20-volte-setup.md` documenting the procedure to enable VoLTE on the EC20 module (deactivate MBN profile, force IMS, LTE-only mode).

## v5.0.3

- **Fix Missing USB Audio Routing** -- Added `AT+QPCMV=1,2` to module initialization, routing voice audio through the USB Audio Class interface. Without this command, audio went to the EC20's analog PCM pins instead of the USB ALSA device, resulting in silence on both GSM echo and SIP-bridged calls.
- **Wire gsm-echo Debug Binary** -- Replaced the placeholder stub with a working implementation that auto-discovers an EC20 module (or accepts `--serial`/`--audio` overrides), configures AT commands, and monitors for incoming calls with auto-answer and call lifecycle logging.
- **Wire sip-echo Debug Binary** -- Replaced the placeholder stub with a working implementation that loads config, registers with the SIP PBX, and waits for incoming calls with graceful shutdown via SIGINT/SIGTERM.

## v5.0.2

- **Docker Image Size Reduction** -- Migrated to Alpine-based runtime with static PJSIP linking. Image reduced from 129MB to 25MB (81% smaller). Uses a 4-stage build: PJSIP static on Alpine, bindgen on Debian, Rust build on Alpine, minimal Alpine runtime.
- **Static PJSIP Linking** -- All PJSIP libraries statically compiled into the binary; no `.so` files needed at runtime. Added `PJSUA_SYS_BINDINGS` and `PJSUA_SYS_STATIC` env vars to `pjsua-sys` build script for pre-generated bindings and static link control.
- **Call Stability Fix** -- Fixed stale `SIP_PEER_DISCONNECTED` flag causing subsequent calls to immediately hang up. The flag from a previous call's BYE was not consumed when the module was in Idle state.
- **Audio Quality Tuning** -- Disabled echo cancellation (`ec_tail_len=0`), set max quality, explicit 20ms ptime, and auto jitter buffer for improved audio on musl runtime.
- **Removed `alsa` Crate** -- Dropped unused direct ALSA dependency from `gsm-sip-bridge`.
- **Release Binary Optimization** -- Added `strip=true` and `lto="thin"` to workspace release profile.
- **Healthcheck** -- Switched from `curl` to `wget` in both Dockerfile and docker-compose.

## v5.0.1

- **Ringback Tone Fix** -- The tonegen was playing the 400 Hz ringback only once instead of looping. Now uses `PJMEDIA_TONEGEN_LOOP` so the GSM caller hears continuous ringing until the SIP extension answers.
- **Uptime Metric Fix** -- `gsm_sip_bridge_uptime_seconds` was defined but never set. Now computed on each Prometheus scrape.
- **Call Duration Histogram Fix** -- `gsm_sip_bridge_call_duration_seconds` was never observed. Now recorded at end of each call.
- **SIP Call Rate Metric Fix** -- `gsm_sip_bridge_sip_calls_total` was never incremented. Now tracks initiated/error outcomes.
- **Audio Errors Metric Fix** -- `gsm_sip_bridge_audio_errors_total` was never incremented. Now tracks sound device failures.
- **README Refresh** -- Full rewrite with Mermaid diagrams, TOML config examples, and architecture documentation.
- **Grafana Dashboard Screenshot** -- Added fresh capture from the running instance.

## v5.0.0

- **Complete Rust Rewrite** -- Replaced the C++17 implementation with a Rust workspace for memory safety, eliminating all manual memory management.
- **Three-Crate Architecture** -- `pjsua-sys` (bindgen FFI), `pjsua-safe` (safe wrappers with `// SAFETY:` comments), `gsm-sip-bridge` (zero `unsafe` binary).
- **Async Runtime** -- Tokio-based event loop with `crossbeam_channel` for the DB writer thread.
- **TOML Configuration** -- Replaced INI format with TOML; secrets support `env:VAR_NAME` syntax.
- **DID Passthrough via Headers** -- Outbound SIP INVITE carries `P-Asserted-Identity` and `X-GSM-Caller-ID` headers; leading `+` stripped from request URI.
- **PJSIP Conference Bridge Audio** -- Bidirectional audio via `pjsua_conf_connect` in `on_call_media_state` callback; ALSA device matched by card name from `/proc/asound/`.
- **SMS Text Mode** -- Switched from PDU to text mode (`AT+CMGF=1`) for simpler parsing and more reliable extraction.
- **SQLite Store Thread** -- Dedicated writer thread with `StoreCommand` enum; WAL mode for concurrent access.
- **Discord SMS Forwarding** -- Async webhook posting with DB status tracking (`pending`/`sent`/`failed`).
- **Multi-Arch Docker Image** -- Published to GHCR for linux/amd64 and linux/arm64.
- **CI Pipeline** -- GitHub Actions with clippy, rustfmt, cargo-deny, and full test suite.
- **Prometheus Metrics** -- All v4.x metrics carried forward with `gsm_sip_bridge_` prefix, plus new `store_writes_total`, `store_queue_depth`, and `build_info`.
- **Thread Registration** -- All PJSIP API calls preceded by `pj_thread_register()` to prevent assertion crashes from async threads.
- **Graceful Shutdown** -- SIGTERM/SIGINT handling with proper PJSIP cleanup and DB flush.

## v4.1.1

- **SIP Registration Retry** -- PJSIP now automatically retries registration after 5 minutes when the server rejects with a permanent failure (e.g. 403 Forbidden), preventing the bridge from silently going offline.
- **Database Rename** -- SMS and call database renamed from `sms.db` to `data.db` to reflect its broader scope; update `db_path` in `config.ini` if overridden.
- **sqlite-web Browser** -- Docker Compose stack now includes an optional read-only web UI (`sqlite-web`) for browsing call and SMS records at `http://localhost:8088`.

## v4.1.0

- **Call Logging** -- Every incoming GSM call is recorded in a local SQLite database with caller ID, module ID, timestamp, duration, SIP destination, and outcome (answered/missed/failed).
- **SMS Persistence** -- All received SMS messages are stored in SQLite with sender, body, timestamp, module, and Discord forwarding status, surviving restarts and Discord outages.
- **sqlite-web UI** -- Docker Compose stack now includes a read-only web interface for browsing call and SMS records at `http://localhost:8088`.

## v4.0.0

- **SMS-to-Discord Forwarding** -- Captures incoming SMS from all modules, persists to a local SQLite database, and posts rich embed notifications to a configurable Discord webhook.
- **SMS Monitoring** -- Independent SMS polling on all modules via AT commands (`AT+CMGL`), with automatic SIM cleanup after read.
- **Configurable via `[sms]` section** -- Enable/disable SMS, set Discord webhook URL, and configure database path in `config.ini`.

## v3.0.1

- **Build Performance** -- PJSIP Docker build layer is now cached across branches and tags, significantly reducing CI build times.
- **CMake FetchContent** -- Replaced vendored mINI header with CMake FetchContent for cleaner dependency management.
- **License** -- Added GNU GPL v3 license.

## v3.0.0

- **Prometheus Metrics** -- Exposes call counts, SIP registration state, module health, audio errors, and call duration histograms on a `/metrics` endpoint (default port 9091).
- **Grafana Dashboard** -- Ships a pre-provisioned dashboard with panels for system overview, call rates, active calls, duration percentiles, module health, and error rates.
- **Docker Compose Monitoring Stack** -- One-command deployment of the bridge with Prometheus and Grafana in host network mode.

## v2.0.0

- **Multi-Card Support** -- Detects all connected EC20 modules at startup, assigns stable hardware IDs derived from USB serial numbers, and handles concurrent calls across modules independently.
- **Automatic Module Recovery** -- Failed modules (SIM issues, serial errors) are retried every 30 seconds and rejoin the active pool when functional.
- **Single-Card Override** -- Explicit `--serial` and `--audio` flags bypass auto-detection for single-module setups.

## v1.1.0

- **DID Passthrough** -- `sip_destination` is now optional. When empty, the GSM caller's number is used as the SIP DID, letting the PBX inbound route decide the destination extension.
- **SIP Media Renegotiation Fix** -- Audio bridge now reconnects correctly after SIP re-INVITE (media hold/resume scenarios).
- **SIP TCP Transport Fix** -- Fixed connection type when using TCP transport.

## v1.0.0

- **GSM-to-SIP Call Bridging** -- Auto-answers incoming GSM calls on a Quectel EC20 module and bridges audio bidirectionally to a SIP extension via a PBX.
- **SIP Audio Echo** -- Standalone SIP echo server for testing (echoes audio back to caller).
- **GSM Audio Echo** -- Standalone GSM echo tool for hardware validation (echoes modem audio back to caller).
- **Caller ID Forwarding** -- GSM caller's number is forwarded to SIP via P-Asserted-Identity header for DID routing.
- **Lock-Free Audio Pipeline** -- SPSC ring buffers connect ALSA capture/playback to the PJSIP conference bridge with minimal latency.
- **USB Auto-Discovery** -- Detects EC20 modules by scanning the USB bus for vendor/product ID `2c7c:0125`.
- **Docker + CI** -- Multi-platform Docker image (amd64/arm64) with GitHub Actions CI pipeline.
