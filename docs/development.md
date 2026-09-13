# Development / Building from Source

For development or non-Docker deployments. Most users should prefer the
[Docker Compose deployment](../README.md#quick-start-docker-compose).

## Prerequisites

- Rust stable (pinned by `rust-toolchain.toml`)
- System packages: `build-essential`, `pkg-config`, `clang`, `libclang-dev`
- Libraries: `libasound2-dev`, `libusb-1.0-0-dev`, `libpjproject-dev` (>= 2.14), `uuid-dev`
- Hardware: One or more Quectel EC20 USB modems with active SIM cards
  (see [supported-hardware.md](supported-hardware.md) for one-time module prep)
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

## Testing against a real linked PJSIP (`--features pjsip-linked`)

`make test`/`make build` normally exercise `pjsua-safe` in **stub mode** —
no real PJSIP calls are made, which is what makes the default test suite
fast and hardware-independent. `pjsua-safe`'s own integration tests
(`pjsua-safe/tests/*.rs`) additionally run against a **real, linked**
PJSIP when built with `--features pjsip-linked`:

```bash
cargo test -p pjsua-safe --features pjsip-linked
```

Two environment-specific gotchas apply here, both handled automatically
where possible:

- **Wrong `libclang` picked up.** `bindgen` (via `clang-sys`) defaults to
  whatever `llvm-config` is first on `PATH`. If a dev machine has an
  unusual, very new LLVM toolchain installed alongside the distro one (seen
  in the wild: an unreleased Homebrew LLVM 23 build), that libclang has a
  real parsing regression — several PJSIP structs that are `typedef`'d via
  a forward declaration before their full body appears later in the header
  (`pjsua_media_config`, `pjsip_cred_info`, `pjsip_rx_data`,
  `pjsua_msg_data`, ...) come out with no fields at all, which then fails
  `pjsua-safe`'s build with a wall of unrelated-looking `no field X`
  errors. `pjsua-sys/build.rs` now auto-detects and prefers a mainstream,
  distro-packaged `libclang` (Debian/Ubuntu's `libclang-19`/`-20`/`-21`)
  whenever `LIBCLANG_PATH` isn't already set, and warns loudly (naming the
  libclang it picked, or the opaque structs it detected if bindgen still
  produced them) so this is never silently confusing again. Set
  `LIBCLANG_PATH` yourself to override the auto-detection.

- **Locally built/installed PJSIP missing this project's `config_site.h`.**
  The Docker image always builds PJSIP with `docker/pjsip-config-site.h`
  (enables `PJMEDIA_CODEC_L16_HAS_16KHZ_MONO`, the VoWiFi bridge's wideband
  codec, and `PJ_GETHOSTIP_DISABLE_LOCAL_RESOLUTION`). The `libpjproject-dev`
  apt package this doc's own Prerequisites section tells you to install does
  **not** set either — so `--features pjsip-linked` tests that depend on
  those (e.g. anything asserting an `L16/16000` codec offer) fail locally
  even though the Docker build is fine. `pjsua-sys/build.rs` detects this
  and warns at build time, naming the missing defines. To fix it for real,
  rebuild PJSIP from source with the project's `config_site.h` in place —
  version pinned by `docker/Dockerfile`'s `PJSIP_VERSION` build arg
  (`2.16` as of this writing):
  ```bash
  wget https://github.com/pjsip/pjproject/archive/refs/tags/2.16.tar.gz
  tar xzf 2.16.tar.gz && cd pjproject-2.16
  cp ../gsm-sip-bridge/docker/pjsip-config-site.h pjlib/include/pj/config_site.h
  ./configure --prefix=/usr/local CFLAGS="-fPIC"   # shared libs, matching
                                                    # the apt package's layout
                                                    # — don't add Docker's
                                                    # --disable-shared here
  make dep && make
  sudo make install
  sudo ldconfig
  ```
  This is optional — it only affects the subset of `--features
  pjsip-linked` tests that care about the wideband codec or the
  hostname-lookup behavior; everything else (including this project's own
  `[sip].public_addr` tests) passes against the stock apt package.

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
| `make test` | Run all workspace tests (via `cargo nextest` when installed, for its per-test timeout) |
| `make run` | Start the bridge |
| `make lint` | rustfmt check + clippy (`--workspace --all-targets -D warnings`) + cargo-deny + shellcheck + unsafe audit |
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
make lint                # clippy --workspace --all-targets -D warnings, + deny/shellcheck/unsafe
make test                # all tests must pass
```

Two optional tools change what these actually check, so it is worth having
them installed locally rather than discovering the difference in CI:
`cargo-deny` (otherwise the dependency/licence policy is silently skipped) and
`cargo-nextest` (otherwise there is no per-test timeout, and a test that hangs
wedges the whole run).
