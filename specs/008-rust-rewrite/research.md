# Phase 0 — Research & Decisions

**Feature**: `008-rust-rewrite` (gsm-sip-bridge v5.0.0)
**Date**: 2026-05-05
**Spec**: [spec.md](./spec.md)

This file records every load-bearing technical decision made before implementation begins, so that the plan, data model, contracts, and tasks all draw from a single source of truth. Each decision lists what was chosen, why, and the alternatives considered. Decisions confirmed by the user during `/speckit-plan` are tagged **(user-confirmed)**; decisions taken without explicit confirmation are tagged **(default)** and may be revisited.

---

## R-01 — PJSIP API surface: wrap the C `pjsua` API directly **(user-confirmed)**

**Decision**: Use the C-level `pjsua` API (not the C++ `pjsua2` wrapper). Generate FFI bindings with `bindgen` at build time.

**Rationale**:
- Rust↔C FFI is well-trodden; Rust↔C++ is not. Avoiding `cxx` keeps build complexity and `unsafe` surface smaller.
- `pjsua2` is itself a thin C++ wrapper over `pjsua`; routing through it would mean three layers (Rust → cxx shim → pjsua2 → pjsua) where one suffices.
- The current C++ class structure (`BridgeAccount`, `BridgeCall`, `AlsaMediaPort`) translates naturally to Rust structs that hold opaque `pjsua_*_id` handles plus methods that call C functions.
- `bindgen` regenerating from a pinned PJSIP version keeps us aligned with PJSIP's C ABI (which is stable across minor releases) without manually maintaining bindings.

**Alternatives considered**:
- **`pjsua2` via `cxx`** — works, but adds the `cxx` dependency, the C++ build step, and the cognitive cost of two languages plus Rust on the stack. No countervailing benefit; pjsua2's "object orientation" is something Rust can do natively over the C API.
- **Existing third-party crate (e.g., `pjsua-rs`)** — reviewed, but ecosystem coverage is patchy: most crates lag PJSIP releases, expose only a subset of the API, and hide design choices we want to make ourselves (e.g., audio media port custom callback, conference bridge slot management). We'll take inspiration but generate bindings ourselves.

---

## R-02 — Async / threading model: hybrid (tokio for network; OS threads for audio + PJSIP) **(user-confirmed)**

**Decision**:
- A single `tokio` runtime (multi-threaded, default flavor) hosts: the metrics HTTP server, the Discord webhook client, the database writer task, the SMS handler's network-bound coroutines, the module retry timer, and the graceful-shutdown signal handler.
- Each EC20 module owns its own dedicated `std::thread` for AT-command serial I/O and a separate dedicated `std::thread` (or pair of threads — capture/playback) for ALSA audio. PJSIP runs its own internal worker threads as configured by `pjsua_init`.
- Audio frames flow through `crossbeam-queue::ArrayQueue` (lock-free SPSC) on the per-frame hot path. No mutexes on that path.
- Cross-world communication uses `tokio::sync::mpsc` from threads-into-tokio and `crossbeam::channel` for thread-to-thread.

**Rationale**:
- ALSA capture/playback is naturally blocking; forcing it through `spawn_blocking` adds executor scheduling jitter that conflicts with the 200 ms p95 latency target (SC-003).
- PJSIP manages its own threads; layering tokio on top of that gains nothing.
- Network-bound work (Discord HTTPS, metrics scrape, retry timers) is exactly what tokio is good at. Mixing two threading styles inside one process is a well-supported pattern (`tokio::runtime::Handle::block_on`, `tokio::task::spawn_blocking`, channels).

**Alternatives considered**:
- **All-tokio** — forces `spawn_blocking` for ALSA and conflicts with PJSIP's own thread model; risks audio jitter.
- **All-threads** — the metrics HTTP server, Discord retry/backoff, and DB connection pooling all become hand-rolled; adds avoidable code.

**Implication for FR-022**: "Lock-free SPSC pathway" maps to `crossbeam-queue::ArrayQueue` between the ALSA capture thread and the PJSIP custom audio media port read callback (and symmetrically for playback). No mutex on the per-frame path.

---

## R-03 — TLS verification policy for SIP: strict by default, explicit opt-out **(user-confirmed)**

**Decision**: When `[sip].transport = tls`, PJSIP MUST verify the server certificate against the system trust store by default. A `[sip].tls_verify` config key with values `strict` (default) or `skip` may be set; setting `skip` triggers a clearly worded `WARN` log at startup naming the operator-acknowledged risk. CLI cannot override this (per FR-076).

**Rationale**: Modern security defaults; matches `reqwest`/`rustls` behavior; lets homelab operators with self-signed PBXs proceed with eyes open. The startup warning is operator-actionable: it names the host being trusted unconditionally so the operator can see in logs that they're in skip mode.

**Configuration mapping**: `pjsua_transport_config.tls_setting.verify_server` and `verify_client` set to `1` (strict) or `0` (skip) at PJSIP init.

**Alternatives considered**:
- **Skip by default** — rejected; insecure default.
- **No opt-out** — rejected; would block self-signed PBX setups (a real homelab use case).

---

## R-04 — Process supervision: Docker-only **(user-confirmed)**

**Decision**: Ship a Docker Compose stack (matching v4.1.x scope: bridge + Prometheus + Grafana + a SQLite browser) and document Docker as the supported supervision path. Do not ship a systemd unit file. Operators running outside Docker bring their own supervisor.

**Rationale**: User picked Docker-only. Avoids cross-distro init system testing burden. Keeps deployment story tight.

**Implications**:
- The Dockerfile builds the Rust binary and copies the Grafana dashboard JSON, prometheus config, and udev rule into the image where appropriate.
- `docker-compose.yml` uses `host` network mode (matching v4.1.x) so USB-attached ALSA cards and SIP UDP/TCP/TLS work without port mapping.
- Container restart policy: `restart: unless-stopped`.
- `EnvironmentFile`-style secret loading still applies via the env vars passed by Compose (`environment:` + `env_file:`).

---

## R-05 — Workspace layout: three crates **(user-confirmed)**

**Decision**: Cargo workspace at the repo root, three member crates:

| Crate | Role | `unsafe` posture |
|---|---|---|
| `pjsua-sys` | Auto-generated `bindgen` output. One large `unsafe extern "C"` block. Build script (`build.rs`) finds PJSIP via `pkg-config`, runs bindgen, emits link directives. | All `unsafe` (this is FFI). |
| `pjsua-safe` | Hand-written safe Rust wrappers around `pjsua-sys`. Exposes a Rust-shaped API (`Endpoint`, `Account`, `Call`, `AudioMediaPort`, `AccountConfig`, etc.). Every `unsafe` block has a `// SAFETY: …` justification per FR-080. | Localised `unsafe`; surfaces `Result<T, PjsipError>`. |
| `gsm-sip-bridge` | The binary crate: bridge logic, multi-card pool, AT commander, ALSA media port, SMS handler, persisted store, metrics, Discord webhook, CLI, config loader. | **Zero** `unsafe`. |

**Rationale**:
- Directly serves SC-009 (`<5%` `unsafe` outside FFI). The binary crate is auditable as 0% unsafe.
- Constitution principle V (simplicity / refactorability): the seam between `pjsua-safe` and `gsm-sip-bridge` is a normal Rust API; bridge tests don't have to know about FFI.
- PJSIP version bumps only touch `pjsua-sys` and possibly `pjsua-safe`. Operator-facing crates stay stable.

**Out-of-tree publishing**: Not required for v5.0.0. If we later want to publish `pjsua-sys` / `pjsua-safe` to crates.io, the workspace boundary is already drawn.

---

## R-06 — Configuration format: TOML **(default)**

**Decision**: Single `config.toml` file. Loaded with `serde` + the `toml` crate. Sensitive fields support either literal string values or `env:VAR_NAME` references (per FR-075..078).

**Rationale**:
- Idiomatic for Rust ecosystem (Cargo, rustfmt, etc. all use TOML).
- Forgiving syntax for operators (no YAML indentation traps).
- Compact for the size of our configuration (≤30 keys).

**Alternatives considered**:
- **YAML** — adds a parser dep, indentation footguns, and is unfamiliar territory for some operators in the homelab segment.
- **Keep INI** — directly contradicts the "clean break" decision (Q3 in spec). INI semantics are less clearly defined across libraries (e.g., escaping, types).

**Schema sketch** (full schema in `contracts/config.toml.schema.md`):

```toml
[sip]
server         = "pbx.example.com"
port           = 5060
username       = "bridge-account"
password       = "env:SIP_PASSWORD"   # literal or env: reference
transport      = "udp"                 # udp | tcp | tls
local_port     = 5060
tls_verify     = "strict"              # strict | skip
display_name   = "GSM Bridge"

[bridge]
sip_destination       = ""             # empty -> DID passthrough
sip_dial_timeout_sec  = 30

[sms]
enabled             = true
discord_webhook_url = "env:DISCORD_WEBHOOK_URL"
db_path             = "/var/lib/gsm-sip-bridge/store.db"

[metrics]
port = 9091

[modules]
retry_interval_sec = 30
max_concurrent     = 8                 # informational; matches FR-014 cap
```

---

## R-07 — Persisted store: `rusqlite` with a single dedicated writer thread **(default)**

**Decision**: Use `rusqlite` (synchronous, with the `bundled` feature so we don't depend on a system SQLite). All writes go through one dedicated `std::thread` ("DB writer thread") that owns the `Connection` and pulls work items off a `crossbeam::channel`. Reads (e.g., for the SMS browser CLI subcommand or future replay endpoints) get their own short-lived connections in WAL mode.

**Rationale**:
- WAL mode permits concurrent readers; a single writer avoids `SQLITE_BUSY` thrashing entirely.
- Synchronous `rusqlite` is simpler than async `sqlx` and integrates cleanly with our hybrid model: tokio tasks (e.g., the SMS handler) send messages over a channel, the writer thread runs the SQL.
- `bundled` feature builds SQLite from source as part of the cargo build, removing a runtime dependency on `libsqlite3` and matching v4.1.x's `FetchContent_Declare(sqlite3)` approach.

**Alternatives considered**:
- **`sqlx`** — async-native, but the only async path that matters in our system is "writer hands off to writer thread", which `rusqlite` already provides cleanly.
- **System-installed SQLite** — depends on operator's distro version; bundled removes that variability.

**Schema** lives in `contracts/db.schema.sql`. WAL is enabled at startup.

---

## R-08 — Metrics endpoint: `axum` + `prometheus` crate **(default)**

**Decision**:
- `axum` (tokio-native HTTP server) hosts the `/metrics` endpoint.
- The `prometheus` crate (Rust port of the Go client) maintains the metric registry; `prometheus::TextEncoder` produces the exposition format.
- Metric names use the prefix `gsm_sip_bridge_*` (matches the binary name; this is a clean break from v4.1.x's `gsm_bridge_*`, documented in the migration guide per FR-072).

**Rationale**:
- `axum` is the canonical tokio HTTP server; minimal boilerplate; well-documented.
- The `prometheus` crate is the most widely used Rust Prometheus client; ships with histogram and counter primitives that match our spec.
- Renaming the prefix from `gsm_bridge_*` to `gsm_sip_bridge_*` is consistent with the project name and clean-break decision; the migration guide documents the rename mapping for operators with custom dashboards/alerts.

**Metric catalog** lives in `contracts/metrics.md`.

---

## R-09 — Discord webhook client: `reqwest` with `rustls-tls` **(default)**

**Decision**: `reqwest` with the `rustls-tls` feature (no OpenSSL dependency) and `json` feature. Discord retry policy: exponential backoff, max 3 retries, total timeout ≤30 s. On `429 Too Many Requests`, honour `Retry-After`. On unrecoverable failure, mark the SMS row's `forwarding_status = failed` and emit a `gsm_sip_bridge_sms_forwarded_total{outcome="failed"}` increment. Persistence to the local store is unconditional and happens before the network call (FR-031).

**Rationale**:
- `rustls` keeps the binary free of OpenSSL FFI for our HTTP path; PJSIP retains its own TLS stack for SIP.
- `reqwest` is the de-facto Rust HTTP client; well-supported by tokio.
- The retry/backoff numbers are conservative; they're configurable in a follow-up if real operators report problems.

**Discord embed shape** lives in `contracts/discord-webhook.md`.

---

## R-10 — Logging: `tracing` + `tracing-subscriber` **(default)**

**Decision**: All logging through the `tracing` crate. `tracing-subscriber::fmt` for human-readable output by default, with `--verbose` enabling `RUST_LOG=debug,gsm_sip_bridge=trace,pjsua_safe=debug` semantics. Structured logging uses key-value fields. Secret redaction (FR-078) is implemented as a `tracing::Layer` that scans well-known field names (`sip.password`, `discord.webhook_url`, `auth.*`) and replaces values with the fixed placeholder `[REDACTED]` before format. Two extra targets: `sip` (forwards a `pjsua` log callback into `tracing`) and `at` (logs every AT command + response).

**Rationale**: `tracing` is the modern Rust async-friendly logging stack; structured fields satisfy FR-082; layer-based redaction keeps secret handling centralized.

---

## R-11 — CLI: `clap` v4 with derive **(default)**

**Decision**: Define the CLI as a `#[derive(clap::Parser)]` struct in the binary crate. Subcommands kept minimal — flag-based parity with v4.1.x preferred over a subcommand explosion.

**Sketch**:
```text
gsm-sip-bridge --config /etc/gsm-sip-bridge/config.toml [--verbose] \
               [-s /dev/ttyUSB3 -a hw:2,0]   # single-card override
```

CLI does not accept secrets (FR-076).

**Schema in** `contracts/cli.md`.

---

## R-12 — ALSA, USB, Serial: which crates **(default)**

| Concern | Crate | Notes |
|---|---|---|
| ALSA capture/playback | `alsa` (pure FFI bindings to libasound2) | Same library v4.1.x links against; non-replaceable on Linux. The `alsa` crate itself is a thin FFI layer; we wrap it in a small abstraction (`AudioDevice`) so tests can substitute a stub. |
| USB enumeration (vendor/product 0x2c7c:0x0125 + serial-number scrape) | `rusb` (bindings to libusb-1.0) | Mature; used by many Rust serial-port projects. |
| Serial / `/dev/ttyUSB*` AT commander | `serialport` | Cross-platform Rust serial; we only need the Linux path. |
| Sine-wave beep generator | Hand-written; no crate | ~30 lines of pure Rust to fill a buffer at 400 Hz; trivial. |

These three (`alsa`, `rusb`, `serialport`) are unavoidable FFI to system libraries (libasound2, libusb-1.0, kernel tty). FR-003 explicitly allows FFI here. The `unsafe` introduced by these crates is in *their* code, not ours.

---

## R-13 — Build & toolchain: rust-toolchain.toml + Makefile wraps cargo **(default)**

**Decision**:
- `rust-toolchain.toml` pins the Rust toolchain to a recent stable (target: latest stable at the time of v5.0.0 GA). MSRV: stable - 2 minor versions (so we don't drift far ahead of distro packagers).
- `Cargo.lock` committed to the binary crate (best practice for applications).
- A repo-root `Makefile` wraps cargo to satisfy Constitution Principle IV. Required targets: `build`, `test`, `run`, `clean`, `lint`. New targets: `format` (cargo fmt), `dev` (cargo run with debug profile), `dev-gsm` and `dev-sip` for the existing debug echo binaries (kept as separate Cargo bins inside `gsm-sip-bridge`).
- `cargo deny` configured to flag advisory CVEs and forbidden licenses on `make lint`.

**Cargo bins** in the `gsm-sip-bridge` crate:
- `gsm-sip-bridge` — the production bridge.
- `gsm-echo` — single-card GSM-only audio loopback debug tool (matches v4.1.x).
- `sip-echo` — single-card SIP-only audio loopback debug tool (matches v4.1.x).

---

## R-14 — Testing strategy aligned with constitution **(default)**

**Decision**:
- Constitution Principle I requires integration-first testing with mocks only for impractical-to-run-locally services, with a `>90%` coverage target via integration tests.
- **Real components used in tests**:
  - `pjsua-safe` runs against a real PJSIP `pjsua` instance pointed at a `localhost` SIP loopback (a tiny test PBX or echo registrar) — same approach v4.1.x's `tests/integration/test_sip_echo.cpp` uses.
  - `rusqlite` runs against a real on-disk SQLite (test fixtures clean up via `tempfile`).
  - `axum` metrics endpoint runs on an ephemeral port; tests scrape it via `reqwest`.
  - AT commander runs against a `socat` PTY pair where the test side scripts AT responses (matches v4.1.x's `util_get_pty_for_tests`).
- **Mocked components (with justification at the mock site)**:
  - Discord webhook target: a local `wiremock` instance returns 200 / 429 / 500 on demand. Justification: hitting real `discord.com` from CI is impractical (rate limits, secrets, flaky network).
  - Quectel EC20 hardware: tests use the PTY-driven AT command stub plus an ALSA `null` device. Justification: physical EC20 modules cannot be present in CI; the integration of AT + ALSA is itself the unit under test.
- **Test layout**:
  - `gsm-sip-bridge/tests/integration/*.rs` — bridge-level integration tests (mirror of v4.1.x layout).
  - `pjsua-safe/tests/*.rs` — wraps test against real `pjsua` instance.
  - `cargo test` runs all of them; `make test` calls `cargo test --workspace --all-features`.

**Coverage**: `cargo llvm-cov` (LLVM source-based coverage) reported in CI. Pass threshold: ≥90% lines covered, per Constitution Principle I.

---

## R-15 — Migration deliverables (no CLI; doc-only) **(spec-confirmed)**

**Decision**: A single Markdown migration guide at `docs/migrating-from-v4.1.x.md` ships with the v5.0.0 release. It contains:
1. **Configuration**: a side-by-side table of every v4.1.x INI key and its v5.0.0 TOML equivalent, with before/after examples.
2. **Database**: copy-pasteable SQL the operator runs against the v4.1.x `sms.db` file: `ALTER TABLE` / `INSERT … SELECT` / `DROP TABLE` / `VACUUM`. Idempotent (re-runnable). Produces a new file `store.db` in place; old `sms.db` left untouched (per US5 acceptance scenario #3 about non-destructive migration).
3. **Metrics**: rename mapping `gsm_bridge_*` → `gsm_sip_bridge_*`, with examples for translating Grafana panel queries.
4. **CLI**: flag mapping (most flags retained verbatim; document any renames).
5. **Docker Compose**: full new `docker-compose.yml` shown verbatim (operators copy it over their old one).
6. **Roll-back instructions** as a final section.

**No CLI tool ships** (confirmed by Q5 in clarify session 2026-05-05 = Option A).

---

## R-16 — PJSIP version pinning **(default)**

**Decision**: Pin to `libpjproject` 2.14.x (the version v4.1.x uses today). Documented in the Dockerfile's PJSIP base image build and in `pjsua-sys/build.rs` minimum-version assertion. Bump policy: explicit, with bindgen regenerated and `pjsua-safe` smoke tests rerun.

**Rationale**: 2.14 is the same version v4.1.x is validated against; reduces the number of moving parts in the rewrite; avoids cross-version porting work.

**Where it's enforced**: `pjsua-sys/build.rs` calls `pkg_config::Config::new().atleast_version("2.14").probe("libpjproject")`. The Dockerfile pins the base image tag.

---

## Open items intentionally deferred to implementation tasks

These do not block planning; flagging them so they're not silently forgotten.

- **MTBF / uptime SLO**: clarify session marked this as "outstanding-low". Will surface in the runbook section of the README and in operator-facing release notes; no SLO in v5.0.0.
- **Process metrics namespace**: deciding whether to also expose Go-style `process_*` and `runtime_*` metrics from the `prometheus` crate's process collector (off by default in this crate). Recommend turning it on; settle in implementation.
- **Graceful shutdown ordering**: SIGTERM must (a) stop accepting new GSM RING events, (b) end in-flight calls cleanly, (c) flush pending DB writes, (d) flush pending Discord webhook posts within a configurable grace period (default 10s), (e) exit. Implementation detail.
- **Crash-safety of the persisted store**: WAL + `synchronous = NORMAL` is the v4.1.x choice; revisit in tasks if SC-005 (zero memory-safety incidents in 30 days) suggests stronger durability is needed.
