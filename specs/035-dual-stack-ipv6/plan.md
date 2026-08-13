# Implementation Plan: Dual-Stack IPv6 for the Cellular-Internet Sidecar

**Branch**: `035-dual-stack-ipv6` | **Date**: 2026-08-13 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/035-dual-stack-ipv6/spec.md`

## Summary

Extend the existing `cellular-internet` sidecar (feature 032) so its cellular data
session is **dual-stack**: keep the IPv4 path exactly as today (it stays the
health-gating uplink for VoWiFi and IPv4-only sites), and additionally bring up a
**global IPv6 address + default route** on the WWAN interface so the CGNAT'd host
becomes reachable inbound over IPv6 (SSH/management). IPv6 is **best-effort**: it
never gates the healthcheck, never blocks the bridge, and is retried in the
background without disturbing the IPv4 session. When the global IPv6 address first
appears or changes, an optional operator-supplied **hook script** is invoked with
the new address as its single argument (so the operator can drive their own DDNS).

Technical approach: request a v4+v6 session from QMI (single WDS start with
`ip-type=8` where the modem supports it, else a second `ip-type=6` WDS session
adopted alongside the v4 one), parse the granted IPv6 address/prefix/gateway from
`--wds-get-current-settings`, and apply it with `ip -6 addr add` / `ip -6 route
replace default`. All changes stay inside `docker/cellular-internet/` shell
scripts; the sidecar remains QMI-only and never opens the AT port.

## Technical Context

**Language/Version**: POSIX `sh` (busybox ash under Alpine) — no bashisms, matching
the existing sidecar scripts.
**Primary Dependencies**: `qmicli` (libqmi), `ip` (iproute2), `nslookup`/`getent`,
`timeout`. No new package unless research finds `ip -6` needs one (iproute2 already
present).
**Storage**: The sidecar-local status file (`/run/internet-status`), extended with
IPv6 fields. No other persistence.
**Testing**: POSIX `sh` integration tests under
`docker/cellular-internet/tests/*.sh`, run via `make test-shell` (already wired
into `make test`), plus `shellcheck -x` via `make lint`. Modem faked with scripted
`qmicli`/`ip` stubs on `PATH` (the constitution-sanctioned "hardware not available
in CI" mock); the dial/teardown/hook logic under test is the real thing.
**Target Platform**: Linux host, Alpine container, host networking, privileged,
Quectel EC20/EC25-class modem exposing a QMI (`cdc-wdm`) control node.
**Project Type**: Single-project shell sidecar within a larger Rust workspace; this
feature touches only the sidecar shell scripts, its tests, compose/env, and docs.
**Performance Goals**: No regression to IPv4/VoWiFi. IPv6 state reflected in status
within one probe interval (default 10s) of a change; hook fired within the same
window. Hook execution must not delay the supervise loop.
**Constraints**: Never open the modem AT port (bridge needs it for `AT+CSIM`).
Never make the container unhealthy or block the bridge because of an IPv6 problem.
Default-safe on IPv6-incapable carriers/modems (degrade to today's IPv4-only).
**Scale/Scope**: One modem / one WWAN interface per sidecar instance, as today.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

- **I. Integration-First Testing (NON-NEGOTIABLE)**: PASS. New behavior is covered
  by extending the real `wds_lifecycle_test.sh` (real dial/teardown functions) and
  adding a v6/hook lifecycle test that exercises the real functions against scripted
  `qmicli`/`ip` stubs. The mock is only the modem hardware, justified in-file, as
  the existing suite already does.
- **II. Green-on-Commit (NON-NEGOTIABLE)**: PASS. `make format && make lint && make
  test` gates every commit (also enforced by CLAUDE.md's pre-commit checklist).
- **III. Frequent Atomic Commits**: PASS. Work is sliced per user story (v6 bring-up
  → status/health guarantee → hook), each independently testable and committable.
- **IV. Makefile-Driven Build**: PASS. Uses existing `make test`/`make lint`/`make
  test-shell`/`make docker-build-internet` targets; no new entry points needed.
- **V. Simplicity & Refactorability**: PASS. IPv6 is layered onto the existing
  single dial/teardown lifecycle rather than a second supervisor. One deliberate
  bit of added indirection — the address-change hook subprocess — is required by the
  spec (FR-008/FR-009) and isolated behind one function; noted in Complexity
  Tracking as justified, not gratuitous.

No violations. Proceed.

## Project Structure

### Documentation (this feature)

```text
specs/035-dual-stack-ipv6/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/           # Phase 1 output
│   ├── sidecar-config.md    # new/changed env vars + hook calling convention
│   └── status-file.md       # extended status-file schema (v6 fields)
└── tasks.md             # Phase 2 output (/speckit.tasks — NOT created here)
```

### Source Code (repository root)

```text
docker/cellular-internet/
├── internet-entrypoint.sh     # CHANGED: dual-stack dial, v6 apply/cleanup,
│                              #          background v6 retry, address-change hook
├── internet-lib.sh            # CHANGED: status writer gains v6 fields;
│                              #          helpers for global-v6 detection
├── internet-healthcheck.sh    # UNCHANGED behavior: still gates on IPv4 only
│                              #          (assert this stays true — FR-004)
├── Dockerfile                 # CHANGED only if research shows a missing pkg
└── tests/
    ├── wds_lifecycle_test.sh  # CHANGED: assert v4 path unchanged under dual-stack
    ├── probe_test.sh          # UNCHANGED
    ├── ipv6_lifecycle_test.sh # NEW: v6 apply/cleanup, best-effort degrade,
    │                          #      status v6 fields
    └── ipv6_hook_test.sh      # NEW: hook fires once per distinct address,
                               #      not on unchanged, isolated from loop

docker/docker-compose.cellular-internet.yml  # UNCHANGED (env-driven); doc note only
.env.example                                   # CHANGED: document new INTERNET_* vars
docs/ec20-internet-plus-vowifi.md              # CHANGED: IPv6 reach-back section
sample_configs/ec20-internet-plus-vowifi.toml  # UNCHANGED (bridge config, not sidecar)
```

**Structure Decision**: Single-project shell sidecar. All logic changes are confined
to the two sourced scripts (`internet-entrypoint.sh`, `internet-lib.sh`); the
healthcheck is deliberately left behavior-identical and guarded by a test asserting
IPv4-only gating. Two new `tests/*.sh` cover the v6 lifecycle and the hook. This
keeps the change on the existing, well-factored 032 seam (`INTERNET_NO_MAIN=1`
sourcing + scripted `qmicli`/`ip` stubs) rather than introducing new machinery.

## Complexity Tracking

| Violation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|--------------------------------------|
| Address-change **hook subprocess** (new indirection vs. Principle V) | FR-008/FR-009 require notifying operator tooling of a changed, unstable carrier prefix without coupling the sidecar to any DNS provider or credentials | Built-in DDNS was explicitly declined by the operator (adds config, secrets, a network dependency); status-file-only was declined in favor of push. A single fire-and-forget hook, isolated from the supervise loop, is the minimal mechanism that satisfies the requirement. |
| **Second WDS (v6) session** alongside the v4 one | EC20/EC25 firmwares negotiate v4 and v6 as separate PDN contexts, so a global v6 address needs its own `ip-type=6` session | A v4-only single session cannot deliver the feature's purpose (inbound v6). The v6 session reuses the existing retained-CID/teardown bookkeeping, so it adds a parallel identity pair, not a parallel supervisor. Implemented unconditionally (no combined `ip-type=8` fast path) to keep the v4 dial byte-identical and avoid a second dial code path — see research.md R1. |
