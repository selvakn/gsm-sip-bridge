# Implementation Plan: Isolate a hanging serial port from wedging discovery

**Branch**: `030-bad-port-isolation` | **Date**: 2026-08-08 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/030-bad-port-isolation/spec.md`

## Summary

A single serial interface that hangs the kernel `option` driver on open/read
currently wedges the *entire* discovery scan — taking down daemon startup and
every healthy modem with it. This feature makes discovery resilient in two
independent ways:

1. **Timeout-proof probe (FR-001–FR-004, FR-013)** — run each per-port
   open/probe on an abandonable worker thread joined with a bounded
   `recv_timeout` (~5s). On timeout, log the port (including its USB interface
   path) and move on; after 3 consecutive timeouts on a port, quarantine it in
   memory so later rescans never re-probe it. The leaked worker thread stays
   blocked in the kernel (unavoidable, already true today) but no longer stalls
   the scan.
2. **Operator port blocklist (FR-005–FR-009)** — a `[discovery] excluded_ports`
   config list, matched by exact device path or by USB-topology prefix, that
   `candidate_tty_ports` filters out before any open happens.

Both apply to startup discovery *and* the module manager's ongoing rescans.

## Technical Context

**Language/Version**: Rust (workspace `gsm-sip-bridge`, edition per repo)
**Primary Dependencies**: `serialport` (serial I/O), `serde`/`toml` (config),
`std::thread` + `std::sync::mpsc` (bounded join), `tracing` (logs)
**Storage**: `config.toml` (`[discovery]` section, new); in-memory quarantine
state (no persistence)
**Testing**: `cargo test` via `make test`; integration-first per constitution —
a fake never-responding transport/port abstraction, no real hardware in CI
**Target Platform**: Linux (container on the EC20/EC200 host)
**Project Type**: Single Rust workspace (CLI + daemon), not web/mobile
**Performance Goals**: One wedged port adds ≈ per-port-timeout (~5s) to a scan,
not unbounded; healthy scans unchanged
**Constraints**: Cannot break the kernel hang from user space — must abandon,
not cancel; must not falsely abandon a slow-but-healthy port; empty config =
byte-for-byte today's behavior (FR-008)
**Scale/Scope**: A handful of modems, ≤ ~10 candidate ports per host; single
long-lived daemon process

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

- **I. Integration-First Testing** — PASS. Testing is at the two natural seams,
  not a mocked scan: (a) `run_bounded` is exercised with a real in-process
  never-returning closure over an actual thread + `recv_timeout`, proving the
  abandon path; (b) `select_at_capable_port` (the blocklist/quarantine-skip +
  abandon-then-continue logic) is driven by a scripted `probe_one` fake that
  records which ports were opened, proving a blocklisted/quarantined port is
  never probed (SC-003), that an abandoned candidate is skipped and the next
  tried (FR-002/FR-003), and that an all-timeout modem yields no usable port
  (FR-011). The matcher and quarantine counter are pure and unit-tested. The
  only untested layer is the real serial `open` over sysfs (`probe_one_candidate`
  / `candidate_tty_ports`) — the same hardware boundary the module already
  leaves untested for `probe_sim_status_at`; the literal kernel hang needs the
  specific unit (documented in the spec). No new mocks of internal boundaries.
- **II. Green-on-Commit** — PASS (process gate). `make format && make lint &&
  make test` before every commit.
- **III. Frequent Atomic Commits** — PASS. Work decomposes into independent
  commits: config schema, candidate/interface-path refactor, blocklist filter,
  bounded-probe mechanism, quarantine, logging, wiring. See tasks.md.
- **IV. Makefile-Driven Build** — PASS. No new entry points; existing `make`
  targets cover everything.
- **V. Simplicity & Refactorability** — PASS. Reuses the codebase's existing
  bounded-`recv_timeout` idiom (`ims/agent.rs`, `observability/reporter.rs`)
  rather than introducing an async runtime or a cancellation framework. No new
  dependency. The blocklist reuses the `[discovery]` config `section!` macro.

No violations — Complexity Tracking table omitted.

## Project Structure

### Documentation (this feature)

```text
specs/030-bad-port-isolation/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/
│   └── discovery-config.md   # [discovery] TOML schema + log/behavior contract
└── tasks.md             # Phase 2 output (/speckit-tasks)
```

### Source Code (repository root)

```text
gsm-sip-bridge/
├── src/
│   ├── config/
│   │   ├── raw.rs            # ADD: section! { RawDiscovery { excluded_ports, probe_timeout_ms } }
│   │   └── mod.rs            # ADD: DiscoveryConfig runtime type + From<RawDiscovery>
│   ├── config/mod.rs         # ADD: DiscoveryConfig + field on AppConfig (line ~857)
│   └── modules/
│       └── discovery.rs      # CHANGE: candidate ports carry USB iface path;
│                             #         blocklist filter; bounded-probe worker;
│                             #         per-port quarantine; timeout logging;
│                             #         thread a DiscoveryPolicy through scan_all_inner
│                             #         + public wrappers (unfiltered default kept)
docs/configuration.md         # DOCUMENT: [discovery] keys (test_config_docs.rs enforces)
config.toml.example           # DOCUMENT: [discovery] example (excluded_ports entry, commented)
```

**Structure Decision**: Single existing Rust workspace. All behavior lives in
`modules/discovery.rs` (the scan) and `config/{raw,mod}.rs` (the new section).
No new crate, module tree, or binary. The quarantine's cross-rescan state is
owned by the long-lived caller (the module manager loop that already calls
`scan_modules_excluding_cards` repeatedly), passed into the scan as part of the
policy, so `scan_all_inner` itself stays stateless and testable.

## Complexity Tracking

> No constitution violations — section intentionally empty.
