# Development / Building from Source

For development or non-Docker deployments. Most users should prefer the
[Docker Compose deployment](../README.md#quick-start-docker-compose).

## Prerequisites

- Rust stable (pinned by `rust-toolchain.toml`)
- System packages: `build-essential`, `pkg-config`, `clang`, `libclang-dev`
- Libraries: `libasound2-dev`, `libusb-1.0-0-dev`, `libpjproject-dev` (>= 2.14), `uuid-dev`
- Hardware: One or more Quectel EC20 USB modems with active SIM cards
  (see [hardware-setup.md](hardware-setup.md) for one-time module prep)
- SIP server account (Asterisk, FreePBX, MikoPBX, etc.)

Install build dependencies:

```bash
sudo apt install build-essential pkg-config clang libclang-dev \
  libasound2-dev libusb-1.0-0-dev libpjproject-dev uuid-dev libssl-dev
```

## Build and run

```bash
cp config.toml.example config.toml   # edit with your SIP/PBX details
export SIP_PASSWORD=yourpassword
make build
make test
make run
```

Useful invocations:

```bash
gsm-sip-bridge --config config.toml              # auto-detect all EC20 modules
gsm-sip-bridge --config config.toml --verbose    # verbose SIP + AT logging
gsm-sip-bridge -s /dev/ttyUSB3 -a hw:2,0         # single-card override
```

## Workspace layout

Three crates — `pjsua-sys` (generated FFI), `pjsua-safe` (safe wrappers,
all `unsafe` confined here with `// SAFETY:` comments), and
`gsm-sip-bridge` (the binary, zero `unsafe`). See
[architecture.md](architecture.md) for the module map. Feature specs,
plans, and task breakdowns live under `specs/`.

## Makefile targets

| Target | Description |
|---|---|
| `make build` | Build all crates in release mode |
| `make test` | Run all workspace tests |
| `make run` | Start the bridge |
| `make lint` | Clippy + rustfmt check + cargo-deny |
| `make coverage` | Generate lcov coverage report |
| `make docker-build` | Build the Docker image |
| `make docker-up` | Start the full Docker Compose stack |
| `make docker-down` | Stop the Docker Compose stack |
| `make docker-logs` | Tail logs from the bridge container |
| `make help` | Show all available targets |

## Before committing

Run, in order — all must pass:

```bash
cargo fmt --all          # fix formatting in place
make lint                # rustfmt check + clippy -D warnings + unsafe ratio
cargo test --workspace   # all tests must pass
```
