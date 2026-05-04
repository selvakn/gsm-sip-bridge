# Implementation Plan: Rust Rewrite (gsm-sip-bridge v5.0.0)

**Branch**: `008-rust-rewrite` | **Date**: 2026-05-05 | **Spec**: [./spec.md](./spec.md)
**Input**: Feature specification from `/specs/008-rust-rewrite/spec.md`

## Summary

Rewrite the v4.1.x C++17 GSM-SIP bridge in Rust at full feature parity (all 12 README features, all v4.1.x acceptance scenarios pass), as a clean break with manual operator migration. Wrap PJSIP via FFI to its C `pjsua` API; everything else uses native Rust crates. Project ships as a 3-crate Cargo workspace (`pjsua-sys`, `pjsua-safe`, `gsm-sip-bridge`) producing the binary `gsm-sip-bridge`. Hybrid concurrency model: a single `tokio` runtime hosts network/HTTP/DB-writer/timer work, while each EC20 module runs dedicated OS threads for AT serial I/O and ALSA audio. Audio-frame hot path uses a `crossbeam-queue` SPSC ring buffer (lock-free, no mutex per frame). Targets: end-to-end mouth-to-ear ≤200 ms p95; up to 8 concurrent modules per host; 30-day production stability with zero memory-safety incidents; `<5%` of source lines inside `unsafe` outside the FFI crate.

## Technical Context

**Language/Version**: Rust stable, pinned via `rust-toolchain.toml` (target: latest stable at v5.0.0 GA; MSRV: stable - 2 minor versions).
**Primary Dependencies**:
- FFI: `pjsua` (C API of `libpjproject` 2.14.x) via `bindgen`-generated `pjsua-sys`
- Async runtime: `tokio` (multi-thread)
- HTTP server (metrics): `axum`
- HTTP client (Discord webhook): `reqwest` with `rustls-tls` and `json` features
- Persisted store: `rusqlite` with `bundled` feature
- Metrics primitives: `prometheus` crate (Rust client)
- Logging: `tracing` + `tracing-subscriber`
- Config: `serde` + `toml`
- CLI: `clap` v4 derive
- Audio: `alsa` (libasound2 bindings)
- USB enumeration: `rusb` (libusb-1.0 bindings)
- Serial port (AT commander): `serialport`
- Lock-free SPSC: `crossbeam-queue::ArrayQueue` and `crossbeam::channel`
- Test mocking: `wiremock` for Discord; `tempfile` for filesystem fixtures
- Coverage: `cargo llvm-cov`
- Supply-chain lint: `cargo deny`

**Storage**: Single embedded SQLite database file (default `/var/lib/gsm-sip-bridge/store.db`), WAL mode, synchronous=NORMAL. Tables for calls and SMS. Schema is a clean break from v4.1.x; manual migration via copy-pasteable SQL in the migration guide.

**Testing**: `cargo test --workspace --all-features` exercises real components per Constitution Principle I. PJSIP runs against a localhost SIP loopback. Discord is mocked with `wiremock` (justified: real `discord.com` impractical in CI). EC20 hardware is replaced by a `socat` PTY pair driving the AT commander plus an ALSA `null` device (justified: physical hardware impractical in CI). Coverage gate: ≥90% lines.

**Target Platform**: Linux (Debian/Ubuntu and equivalents). x86_64 and aarch64 (matches v4.1.x's pre-built PJSIP arm64 base image; commit 7565479).

**Project Type**: Cargo workspace producing a binary CLI daemon with two debug bins (`gsm-echo`, `sip-echo`) for component-level debugging.

**Performance Goals**:
- End-to-end one-way audio latency ≤ 200 ms p95 mouth-to-ear (SC-003).
- Sustain 8 concurrent bridged calls for ≥5 minutes with no audio underrun/overrun (SC-010).
- Steady-state CPU and resident memory no worse than v4.1.x baseline on the same hardware (SC-004; baseline measurement is a planning prerequisite tracked in tasks).

**Constraints**:
- Per-frame audio path MUST be lock-free (no mutex on the ALSA↔PJSIP audio data path).
- Binary crate `gsm-sip-bridge` MUST contain zero `unsafe` blocks; `unsafe` is confined to `pjsua-sys` (auto-generated) and `pjsua-safe` (audited wrappers). Aggregate `unsafe` outside FFI bindings <5% of total Rust source (SC-009).
- CLI MUST NOT accept secrets (FR-076).
- Logs MUST redact known-sensitive fields (FR-078).
- Must support up to 8 EC20 modules per host (FR-014).
- TLS verification for SIP defaults to strict, with explicit `tls_verify = "skip"` opt-out (R-03).

**Scale/Scope**:
- Up to 8 EC20 modules per host (one bridged call each, so up to 8 concurrent calls).
- Persisted store grows unbounded (no auto-pruning; manual prune procedure documented per FR-042).
- Discord webhook delivery: per-SMS one-shot HTTP POST with up to 3 retries on 5xx/429.
- Operator scale: tens of homelab/SOHO deployments expected; not designed for large fleets in v5.0.0.

## Constitution Check

The Audio Echo Constitution (`.specify/memory/constitution.md`, v1.0.0) defines five core principles. Each is evaluated below.

### I. Integration-First Testing (NON-NEGOTIABLE) — **PASS**
- Test plan in R-14 uses real PJSIP, real SQLite, real `axum` HTTP server, real `serialport` over PTY, real ALSA `null` device.
- Mocks limited to two cases (Discord webhook, physical EC20 hardware) — both have written justification at the mock site (Constitution requirement).
- Coverage target ≥90% via integration tests (`cargo llvm-cov` gate) — exceeds Constitution's "aim for >90%".
- The 3-crate workspace allows internal refactoring of `pjsua-safe` and bridge-level modules without breaking external behaviour (Constitution's refactorability requirement).

### II. Green-on-Commit (NON-NEGOTIABLE) — **PASS (process gate)**
- CI runs `make test` on every push; merges blocked if it fails.
- Pre-commit hook (provided in `etc/git-hooks/pre-commit`, opt-in) runs `cargo fmt --check && cargo clippy --workspace -- -D warnings && cargo test --workspace`.
- This is enforced by process; the plan does not weaken it.

### III. Frequent Atomic Commits — **PASS (process gate)**
- Tasks (Phase 2 output) will be sized for one commit each.
- This plan does not propose any large multi-concern commits.

### IV. Makefile-Driven Build — **PASS**
- Repo-root `Makefile` exposes the required minimum (`build`, `test`, `run`, `clean`, `lint`) plus `format`, `dev`, `dev-gsm`, `dev-sip`, `docker-build`, `docs`. All targets wrap `cargo` and `docker compose` so contributors don't have to memorize Cargo flags.
- `make help` lists every target with one-line descriptions.
- See R-13 for the full target list.

### V. Simplicity & Refactorability — **PASS, with one tracked complexity**
- The 3-crate workspace boundary is the only added structural complexity vs. a single crate.
- **Justification for the workspace** (recorded in Complexity Tracking below): isolating `unsafe` to two crates is essential for SC-009 (`<5%` `unsafe` outside FFI). A single crate would force `unsafe` audit across the whole bridge codebase.
- Otherwise: no abstractions added beyond what the requirements force. No traits without 2+ implementations. No dependency injection framework. No actor framework. Plain functions and structs.
- YAGNI honoured: no auto-pruning scheduler, no CLI for migration, no pluggable secret-provider abstraction beyond `env:VAR_NAME` literals — each is in scope only as the spec requires.

### Conclusion: Constitution Check **PASS**

All five principles upheld. One justified complexity (3-crate workspace) recorded in the tracking table below.

## Project Structure

### Documentation (this feature)

```text
specs/008-rust-rewrite/
├── plan.md                 # This file (/speckit-plan output)
├── research.md             # Phase 0 — decisions and rationales (R-01..R-16)
├── data-model.md           # Phase 1 — entities, fields, transitions
├── quickstart.md           # Phase 1 — clone-to-running in <60 minutes (SC-008)
├── checklists/
│   └── requirements.md     # Spec quality checklist (from /speckit-specify)
└── contracts/              # Phase 1 — externally observable interfaces
    ├── cli.md              # CLI surface: flags, exit codes, env vars
    ├── config.toml.schema.md  # Configuration file schema
    ├── db.schema.sql       # Persisted store schema
    ├── metrics.md          # Metric catalog: name, type, labels, semantics
    └── discord-webhook.md  # Discord embed shape and rate-limit handling
```

### Source Code (repository root)

The Rust workspace replaces the existing C++17 source tree. v4.1.x source remains on the `main` branch's tag `v4.1.1`; v5.0.0 work happens on this feature branch and merges as a major-version cutover.

```text
audio-echo/                         # repo root (directory name unchanged)
├── Cargo.toml                      # workspace manifest (members listed below)
├── Cargo.lock                      # committed
├── rust-toolchain.toml             # pins stable Rust; profile minimal
├── deny.toml                       # cargo-deny config
├── Makefile                        # constitution-mandated; wraps cargo
├── README.md                       # rewritten for v5.0.0 (Rust toolchain prereqs)
├── docs/
│   └── migrating-from-v4.1.x.md    # the migration guide (FR-074, R-15)
├── etc/
│   └── 99-ec20-gsm-sip-bridge.rules  # udev rule (carried over)
├── docker/
│   ├── Dockerfile                  # multi-stage; pjsip base image + cargo build
│   ├── docker-compose.yml          # bridge + Prometheus + Grafana + sqlite-web
│   ├── prometheus.yml
│   └── grafana/
│       └── provisioning/
│           ├── dashboards/
│           │   └── gsm-sip-bridge.json   # the dashboard (FR-052)
│           └── datasources/prometheus.yml
│
├── pjsua-sys/                      # CRATE 1: bindgen output, FFI declarations
│   ├── Cargo.toml
│   ├── build.rs                    # pkg-config + bindgen invocation
│   └── src/
│       └── lib.rs                  # `include!(concat!(env!("OUT_DIR"), "/bindings.rs"));`
│
├── pjsua-safe/                     # CRATE 2: safe Rust wrappers
│   ├── Cargo.toml
│   ├── src/
│   │   ├── lib.rs                  # public Rust API: Endpoint, Account, Call, AudioMediaPort
│   │   ├── endpoint.rs             # init/destroy, transport configuration, log callback bridge
│   │   ├── account.rs              # registration, account config, callback dispatch
│   │   ├── call.rs                 # outbound call, hangup, media bridge slots
│   │   ├── audio_media_port.rs     # custom audio media port with frame callback
│   │   ├── error.rs                # PjsipError type; Result<T, PjsipError>
│   │   └── log_bridge.rs           # forwards pjsua logs into `tracing`
│   └── tests/
│       └── smoke.rs                # boot endpoint, register against loopback PBX
│
└── gsm-sip-bridge/                 # CRATE 3: the binary (zero `unsafe`)
    ├── Cargo.toml                  # [[bin]] gsm-sip-bridge, gsm-echo, sip-echo
    ├── src/
    │   ├── main.rs                 # gsm-sip-bridge entry point
    │   ├── bin/
    │   │   ├── gsm_echo.rs         # debug bin: GSM audio loopback only
    │   │   └── sip_echo.rs         # debug bin: SIP audio loopback only
    │   ├── lib.rs                  # crate root for shared modules
    │   ├── config/
    │   │   ├── mod.rs              # serde structs for config.toml
    │   │   └── secret.rs           # env: resolution, redaction-aware Display
    │   ├── cli.rs                  # clap parser
    │   ├── runtime.rs              # tokio runtime bootstrap, graceful shutdown
    │   ├── modules/
    │   │   ├── mod.rs              # CardPool: detection, retry, lifecycle
    │   │   ├── discovery.rs        # rusb-based USB scan, stable ID derivation
    │   │   ├── card.rs             # CardInstance: per-module state machine
    │   │   ├── at_commander.rs     # serialport-driven AT command interface
    │   │   ├── beep.rs             # 400Hz sine generator
    │   │   └── audio_pipeline.rs   # ALSA capture+playback threads, SPSC ring buffer
    │   ├── sip/
    │   │   ├── mod.rs              # SipBridge: account, registration, call lifecycle
    │   │   └── alsa_media_port.rs  # impl pjsua_safe::AudioMediaPort over the SPSC queue
    │   ├── sms/
    │   │   ├── mod.rs              # SmsHandler: poll modules, persist, forward
    │   │   ├── reader.rs           # AT+CMGR / +CMGD orchestration
    │   │   └── discord.rs          # webhook client (reqwest)
    │   ├── store/
    │   │   ├── mod.rs              # Store: writer thread + read pool
    │   │   ├── schema.rs           # init/migrate (within-v5 schema versions)
    │   │   ├── calls.rs            # call record CRUD
    │   │   └── sms.rs              # sms record CRUD
    │   ├── metrics/
    │   │   ├── mod.rs              # registry, exposition
    │   │   └── server.rs           # axum metrics endpoint
    │   ├── observability/
    │   │   ├── logging.rs          # tracing-subscriber setup, redaction layer
    │   │   └── modemmanager.rs     # detect+warn at startup
    │   └── error.rs                # crate-wide error types
    │
    └── tests/
        ├── common/
        │   ├── mod.rs              # shared fixtures (PTY pair, ALSA null, wiremock, temp DB)
        │   ├── pty.rs              # socat PTY harness for AT commander
        │   └── pbx.rs              # localhost SIP loopback PBX harness
        ├── test_config.rs
        ├── test_discovery.rs
        ├── test_at_commander.rs
        ├── test_beep_generator.rs
        ├── test_audio_pipeline.rs
        ├── test_card_pool.rs
        ├── test_sip_registration.rs
        ├── test_sip_echo.rs        # equivalent of v4.1.x sip-echo integration
        ├── test_bridge_call.rs     # full GSM↔SIP bridge with PTY + loopback PBX
        ├── test_sms_reader.rs
        ├── test_sms_discord.rs     # uses wiremock
        ├── test_store_calls.rs
        ├── test_store_sms.rs
        ├── test_metrics.rs
        └── test_end_to_end.rs      # multi-card stress, 8-card scenario for SC-010
```

**Structure Decision**: Three-crate Cargo workspace under the existing repo root. v4.1.x's `src/`, `tests/`, `CMakeLists.txt`, and `build/` directories are deleted as part of the cutover commit (their content is preserved in git history at tag `v4.1.1`). The `etc/` and `screenshots/` directories are retained. The `docker/` directory is restructured to host the new Compose stack and Grafana provisioning. The repo directory name `audio-echo` is preserved (renaming a checked-out repo is operator-invasive); only the project/binary name changes to `gsm-sip-bridge`.

## Complexity Tracking

| Violation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|-------------------------------------|
| 3-crate Cargo workspace (instead of single crate) | Isolates `unsafe` to `pjsua-sys` (auto-generated) and `pjsua-safe` (audited). Enables auditable claim that the binary crate is 0% `unsafe`, directly serving SC-009 (`<5%` `unsafe` outside FFI). Also makes PJSIP version bumps a localized change. | Single crate would require auditing every `unsafe` block in a much larger codebase, blurring the FFI boundary and making SC-009 harder to verify. Two crates (sys + bridge combined) would mix safe wrappers and bridge logic, undermining the same property. |

No other deviations from Constitution Principle V. The workspace is the only structural complexity beyond a flat Rust binary, and it is justified by an explicit success criterion in the spec.
