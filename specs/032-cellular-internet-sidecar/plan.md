# Implementation Plan: Cellular-internet sidecar container

**Branch**: `032-cellular-internet-sidecar` | **Date**: 2026-08-10 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/032-cellular-internet-sidecar/spec.md`

## Summary

Package the cellular-internet dialer as a **separate, lightweight container**
that brings up internet over an EC20's QMI data path (`/dev/cdc-wdm0`), leaving
the modem's AT port free for the bridge's `AT+CSIM`. The container exposes a
Docker healthcheck that turns healthy **only when a DNS-resolution probe through
the cellular link succeeds** (grace 90s, interval 10s). Same-card-internet
deployments opt in via a **Compose override file** that both adds the sidecar and
injects `depends_on: internet: condition: service_healthy` onto the bridge, so
the bridge waits indefinitely for a genuinely-reachable uplink before starting.
Deployments with their own uplink don't include the override and are unaffected.
The bridge itself needs **no code change** — it already treats its uplink as
ambient host connectivity; all coordination is Compose-level.

## Technical Context

**Language/Version**: POSIX/bash shell (sidecar entrypoint + probe); no Rust
changes to the bridge. Existing bridge is Rust (unchanged).
**Primary Dependencies**: `libqmi` (`qmicli`) for QMI dial on the Qualcomm EC20;
`udhcpc` (BusyBox) for IPv4 lease; BusyBox `nslookup`/`dig`-equivalent for the
DNS probe; Docker Compose healthcheck + `depends_on` (`condition: service_healthy`).
**Storage**: none persistent. Sidecar-local status is a small state file
(e.g. `/run/internet-status`) + container logs (FR-008). No DB, no volume needed.
**Testing**: `cargo test` unchanged; new shell logic covered by `shellcheck -x`
(via `make lint`) and a probe-script integration test that runs the real probe
against a resolvable name (pass) and a bogus target (fail) — no internal mocks.
The modem dial path is a hardware boundary, validated by the quickstart on a real
EC20 (mirrors how the serial-open boundary is left to hardware in spec 030).
**Target Platform**: Linux host, Docker Compose; Alpine-based sidecar image.
**Project Type**: Deployment/infra feature on the existing single Docker image
stack — adds one sibling image + a Compose override, not a new code project.
**Performance Goals**: sidecar reaches healthy within ~90s of a normal cellular
attach; probe re-evaluates every 10s; adds no measurable overhead to the bridge.
**Constraints**: sidecar MUST NOT hold the modem AT port (QMI-only on
`/dev/cdc-wdm0`); opt-in and **default off** — byte-for-byte today's behavior
without the override; image kept lightweight (internet tooling ships only here,
never in the bridge image, so the bridge image size is unchanged).
**Scale/Scope**: one modem provides the uplink; one sidecar per host.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

- **I. Integration-First Testing (NON-NEGOTIABLE)** — PASS. The one piece of
  branchable logic — the DNS reachability probe that defines "healthy" — is
  tested by running the actual probe script against a real resolvable hostname
  (exit 0) and a guaranteed-unresolvable target (nonzero), no internal mock. The
  QMI dial/reconnect loop talks to real modem hardware not present in CI; it is a
  documented hardware boundary exercised by the quickstart on a real EC20,
  consistent with the untested serial-open boundary in spec 030. Compose
  `depends_on` ordering is a Docker-runtime behavior, verified in the quickstart.
- **II. Green-on-Commit (NON-NEGOTIABLE)** — PASS (process gate). `make format &&
  make lint && make test` before every commit; the new shell scripts are covered
  by `make lint`'s shellcheck.
- **III. Frequent Atomic Commits** — PASS. Decomposes into independent commits:
  sidecar image, dial entrypoint, probe/healthcheck, status output, Compose
  override, Makefile/lint wiring, docs. See tasks.md.
- **IV. Makefile-Driven Build** — PASS with one addition. The sidecar image must
  build and lint via `make`: extend `make lint`'s shellcheck glob to cover the
  sidecar scripts, and add a `docker-build-internet` target (or fold into
  `docker-build`). No operation requires memorizing raw `docker`/`qmicli`.
- **V. Simplicity & Refactorability (NON-NEGOTIABLE-ish)** — PASS. One small
  Alpine image running `qmicli` + a probe loop, plus a Compose override that
  reuses the stack's existing host-networking + privileged pattern. No bridge
  code change, no new abstraction, no async framework. Fewest moving parts that
  satisfy "wait for real internet."

No violations — Complexity Tracking table omitted.

## Project Structure

### Documentation (this feature)

```text
specs/032-cellular-internet-sidecar/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output (config/state entities)
├── quickstart.md        # Phase 1 output (enable + verify runbook)
├── contracts/
│   ├── sidecar-config.md      # env-var config surface + defaults
│   └── healthcheck-compose.md # healthcheck exit-code contract + Compose override wiring
└── tasks.md             # Phase 2 output (/speckit-tasks)
```

### Source Code (repository root)

```text
docker/
├── cellular-internet/
│   ├── Dockerfile            # NEW: Alpine + libqmi(qmicli) + udhcpc; lightweight
│   ├── internet-entrypoint.sh# NEW: qmi raw-ip → APN → wds-start-network → udhcpc
│   │                         #      → default route → monitor/re-dial loop; writes status
│   └── internet-healthcheck.sh# NEW: DNS-resolve configured host via configured
│                             #      resolver over wwan; exit 0/1; update status file
├── docker-compose.yml        # UNCHANGED (default off — no dependency added here)
└── docker-compose.cellular-internet.yml # NEW: override — adds `internet` service
                              #      (profiled/override) + injects bridge depends_on

Makefile                      # CHANGE: shellcheck glob covers docker/cellular-internet/*.sh;
                              #         add docker-build-internet target
docs/
├── ec20-internet-plus-vowifi.md # NEW/updated: runbook that references the sidecar
└── configuration.md          # DOCUMENT: sidecar env vars + override usage (if enforced)
sample_configs/
└── ec20-internet-plus-vowifi.toml # NEW: bridge-side sample for the same-card case
.env.example                  # DOCUMENT: sidecar env vars (APN, device, probe host/resolver)
```

**Structure Decision**: No new Rust crate/module and no change to the bridge
binary or its `config.toml`. The feature is a **sibling container** plus a
**Compose override**. Scripts live under `docker/cellular-internet/` (build
context), with `make lint`'s shellcheck extended to include them. The
opt-in/default-off guarantee is delivered by keeping the base
`docker-compose.yml` free of any dependency and shipping the sidecar + the
bridge's `depends_on` only in `docker-compose.cellular-internet.yml`.

## Complexity Tracking

> No constitution violations — section intentionally empty.
