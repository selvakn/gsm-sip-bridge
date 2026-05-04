# Quickstart — gsm-sip-bridge v5.0.0 (Rust)

**Target audience**: New contributors, operators evaluating the Rust release.
**Goal**: Clone → build → tests pass → single-card debug run, in under 60 minutes on a fresh Linux machine that meets the prerequisites. (SC-008)
**Spec**: [./spec.md](./spec.md)
**Plan**: [./plan.md](./plan.md)

---

## Prerequisites

A Linux host with:
- A C toolchain (`build-essential` on Debian/Ubuntu) — bindgen needs `clang`/`libclang-dev`.
- Linux ALSA development headers (`libasound2-dev`).
- Linux libusb-1.0 development headers (`libusb-1.0-0-dev`).
- PJSIP 2.14.x (`libpjproject-dev`) installed via system package or built from source.
- `pkg-config`.

Optionally:
- `socat` for the AT-commander PTY harness used by tests.
- Docker + Docker Compose if you want to run the full observability stack.
- One or more Quectel EC20 modules if you want to exercise the bridge end-to-end (CI does not require physical hardware).

```bash
sudo apt install build-essential pkg-config clang libclang-dev \
                 libasound2-dev libusb-1.0-0-dev libpjproject-dev \
                 socat
```

The Rust toolchain is pinned by `rust-toolchain.toml` and installed automatically by `rustup` on the first cargo invocation. No manual `rustup toolchain install` needed.

---

## 60-second smoke

```bash
git clone <repo-url> audio-echo
cd audio-echo
make build      # cargo build --workspace --release
make test       # cargo test  --workspace --all-features
```

If both succeed, your environment is good.

---

## Project layout (what you'll see in the repo)

```text
audio-echo/                 # repo root
├── Cargo.toml              # workspace
├── rust-toolchain.toml
├── Makefile                # entry point per Constitution principle IV
├── pjsua-sys/              # FFI bindgen output (auto-generated)
├── pjsua-safe/             # safe Rust wrappers around pjsua
├── gsm-sip-bridge/         # the binary crate (zero `unsafe`)
├── docker/                 # Dockerfile + docker-compose stack
├── docs/
│   ├── configuration.md
│   ├── operations.md
│   └── migrating-from-v4.1.x.md
├── etc/
│   └── 99-ec20-gsm-sip-bridge.rules
└── specs/                  # speckit specifications (this directory)
```

Three crates, one binary plus two debug bins (`gsm-echo`, `sip-echo`). All source under `gsm-sip-bridge/src/` is `unsafe`-free; the FFI lives in `pjsua-sys/` (auto-generated) and `pjsua-safe/` (audited wrappers, every block carries a `// SAFETY:` justification).

---

## Common Make targets

| Command | What it runs | Why |
|---|---|---|
| `make build` | `cargo build --workspace --release` | Produces `target/release/gsm-sip-bridge` plus the two debug bins. |
| `make test` | `cargo test --workspace --all-features` | Full integration suite. Runs against real PJSIP + real SQLite + wiremock for Discord + PTY pair for AT. |
| `make run` | `cargo run --release --bin gsm-sip-bridge -- --config config.toml` | Dev launch. Expects a `config.toml` at repo root or supply your own via `make run CONFIG=/path/to/config.toml`. |
| `make dev-gsm` | `cargo run --bin gsm-echo -- ...` | Single-card GSM-only loopback. Helpful when SIP isn't your problem. |
| `make dev-sip` | `cargo run --bin sip-echo -- ...` | Single-card SIP-only loopback. Helpful when GSM isn't your problem. |
| `make lint` | `cargo fmt --check && cargo clippy --workspace -- -D warnings && cargo deny check` | Constitution principle IV requires `lint` to exist. |
| `make format` | `cargo fmt` | In-place formatting. |
| `make clean` | `cargo clean` | Removes the entire target directory. |
| `make docker-build` | `docker compose build` | Builds the production image. |
| `make help` | Lists every target with a one-line description. | |

CI invokes `make test` and `make lint`. Both must be green for a commit to merge (Constitution principle II).

---

## Configuration for a single-card debug run

Create `config.toml` at the repo root with:

```toml
[sip]
server   = "127.0.0.1"          # localhost SIP loopback
port     = 5060
username = "test"
password = "test"
transport = "udp"

[sms]
enabled = false                 # disable for the smoke run

[bridge]
sip_destination = ""            # DID passthrough — let the local PBX route
```

Then run, with the single-card override flags pointing at one connected EC20 module:

```bash
make run-bin BIN=gsm-sip-bridge ARGS="--config config.toml -s /dev/ttyUSB3 -a hw:2,0 --verbose"
```

You should see:

```text
INFO  starting gsm-sip-bridge v5.0.0
INFO  detected 1 EC20 module(s) (single-card override)
INFO  [ec20-A1B2C3] initializing (serial=/dev/ttyUSB3, audio=hw:2,0)
INFO  [ec20-A1B2C3] GSM network registration confirmed
INFO  SIP registration successful
INFO  ready, 1 module(s) active
```

Dial the GSM number on a phone; the bridge auto-answers and INVITEs `127.0.0.1`.

---

## Without hardware

`make test` runs the full integration suite with no physical EC20 module needed:

- AT command flows are exercised through a `socat` PTY pair (test scripts the modem-side responses).
- Audio I/O uses the ALSA `null` device.
- SIP runs PJSIP against a localhost SIP loopback fixture started by the test harness (same approach as v4.1.x).
- Discord forwarding runs against a `wiremock` instance returning scripted `200`/`429`/`5xx`.
- The persisted store uses a `tempfile`-backed SQLite database that is torn down per test.

If a test fails on a fresh machine, common causes:

1. `socat` not installed → install it, rerun `make test`.
2. PJSIP not at 2.14.x → `pkg-config --modversion libpjproject` should report 2.14.x.
3. Two cargo builds racing for the same `target/` (e.g., your IDE running cargo check while CI runs) → run `cargo clean` once.

---

## Building the Docker stack

```bash
make docker-build
docker compose -f docker/docker-compose.yml up
```

Services:

| Port | Service |
|---|---|
| `9091` | gsm-sip-bridge metrics endpoint |
| `9090` | Prometheus |
| `3000` | Grafana (default `admin`/`admin`; dashboard auto-provisioned) |
| `8088` | sqlite-web (read-only browser of `store.db`) |

The Compose stack uses `network_mode: host` so the bridge can see USB-attached ALSA devices and reach the PBX without port-mapping juggling.

---

## Migrating from v4.1.x

If you're upgrading an existing v4.1.x deployment, **read `docs/migrating-from-v4.1.x.md` end-to-end first**. The migration is doc-only (no migration CLI ships in v5.0.0) and consists of four steps:

1. Rewrite your `config.ini` as `config.toml` per the side-by-side mapping table.
2. Run the supplied SQL against your existing `sms.db` to produce a new `store.db` next to it. (The original `sms.db` is left untouched so you can roll back.)
3. Import the new Grafana dashboard JSON; if you have custom panels, update metric prefixes per the rename mapping in `docs/migrating-from-v4.1.x.md`.
4. Replace your `docker-compose.yml` with the v5.0.0 version (full file shown verbatim in the migration guide).

Roll-back: keep the v4.1.x binary and original config; the migration creates new artifacts side-by-side rather than overwriting.

---

## Verifying the rewrite is doing what the spec says

| Check | How |
|---|---|
| Audio latency p95 ≤ 200 ms (SC-003) | `tests/test_end_to_end.rs::latency_p95` measures with the documented loopback rig. |
| 8 concurrent calls held for ≥5 minutes (SC-010) | `tests/test_end_to_end.rs::eight_card_stress` uses 8 PTY pairs. |
| `<5%` `unsafe` outside FFI (SC-009) | `make lint` runs a script (`tools/count-unsafe.sh`) that grep-counts `unsafe` blocks in `gsm-sip-bridge/src` (must be 0) and reports the ratio for `pjsua-safe/src`. |
| Coverage ≥ 90% (Constitution principle I) | `make coverage` runs `cargo llvm-cov` and prints the line-coverage percentage. |
| Secrets never logged (FR-078) | `tests/test_logging.rs::redaction` boots the bridge with a known fake password, captures all log output, and asserts the value is absent. |
| Migration guide complete (FR-074) | `tests/test_migration_guide.rs` parses `docs/migrating-from-v4.1.x.md` and asserts every documented v4.1.x metric appears in the rename mapping table. |

---

## Where to ask for help

- File a GitHub issue with the label `triage` for bugs.
- For spec/plan questions, the speckit artefacts in `specs/008-rust-rewrite/` are the primary reference.
- The constitution at `.specify/memory/constitution.md` is the highest-authority document for development practices.
