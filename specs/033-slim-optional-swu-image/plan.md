# Implementation Plan: Slim default image with optional SWu engine

**Branch**: `033-slim-optional-swu-image` | **Date**: 2026-08-10 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `specs/033-slim-optional-swu-image/spec.md`

## Summary

Cut the published container image from ~119 MB to ~45–50 MB by making the
SWu/Python tunnel engine an opt-in **build-time payload** rather than something
every image carries. The default build (`ARG INCLUDE_SWU=false`) omits the
Python interpreter, the Python dependency tree, and the vendored SWu-IKEv2
dialer — ~72 MB that only the non-default `swu` engine ever uses. A full image
(`INCLUDE_SWU=true`) is published on demand under a `-swu` tag. Independently,
drop runtime tooling the strongSwan path never touches: replace the single
`dig` shell-out with in-process DNS resolution (removing `bind-tools`), and gate
`net-tools` to the full image only.

The **SWu engine's Rust code stays in the binary and in the test/lint suite
unchanged** — nothing is feature-gated out of compilation. Only the *image
payload* and *runtime apk packages* differ between variants. When the slim image
is asked to run the `swu` engine, the bridge fails fast at startup pointing to
the `-swu` image.

## Technical Context

**Language/Version**: Rust 1.x (workspace, `rust-toolchain.toml`); Dockerfile
(BuildKit); GitHub Actions YAML; POSIX sh/bash; Make.
**Primary Dependencies**: Alpine 3.21 base; PJSIP 2.16 (static); strongSwan-epdg
fork; vpcd; std library for DNS (`std::net::ToSocketAddrs`). No new crates.
**Storage**: N/A (container image + config.toml).
**Testing**: `cargo test` via `make test` (workspace, integration-first);
`make lint` (clippy `-D warnings` whole workspace + shellcheck + deny);
`make format`; plus manual/CI docker builds of both variants.
**Target Platform**: linux/amd64 + linux/arm64 container images.
**Project Type**: Single Rust workspace + Docker packaging + CI.
**Performance Goals**: Slim image ≥55% smaller than the current ~119 MB (target
~45–50 MB). No runtime latency change.
**Constraints**: strongSwan path behavior byte-for-byte unchanged; SWu path
still green in CI; DNS resolution result equivalent to prior `dig +short A`
(an A record for the ePDG FQDN); floating tags (`:latest`, `:X.Y.Z`) resolve to
slim.
**Scale/Scope**: One Dockerfile, one Rust seam addition + one guard, one new CI
workflow, Makefile + docs. No changes to call handling or tunnel logic.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

- **I. Integration-First Testing (NON-NEGOTIABLE)**: PASS. The two new Rust
  seams — a DNS resolver method on `CommandRunner` and a SWu-payload-availability
  check — are exercised through the existing real `CommandRunner`/orchestration
  seam with canned/real inputs, not new mocks. The image variants are validated
  by actually building both and inspecting/booting them (integration), not by
  stubbing docker. No new mock is introduced; the existing `CommandRunner` test
  double already carries its justification.
- **II. Green-on-Commit (NON-NEGOTIABLE)**: PASS. Each task ends at a green
  `make test`/`make lint`. The SWu Rust path stays compiled and tested (FR-005a).
- **III. Frequent Atomic Commits**: PASS. Tasks are split so each is one logical
  change (DNS seam; slim Dockerfile; CI workflow; docs) committable on its own.
- **IV. Makefile-Driven Build**: PASS. A `docker-build-swu` target is added
  alongside `docker-build`; required minimum targets untouched.
- **V. Simplicity & Refactorability**: PASS. Uses the standard BuildKit
  stage-alias pattern (`FROM swu-${INCLUDE_SWU}`) — no new build tooling. The
  variant guard is a file-existence check mirroring the existing
  `/dev/net/tun` check, not a new abstraction.

No violations → Complexity Tracking left empty.

## Project Structure

### Documentation (this feature)

```text
specs/033-slim-optional-swu-image/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/           # Phase 1 output (build/tag/seam contracts)
│   └── interfaces.md
├── checklists/
│   └── requirements.md  # from /speckit-specify
└── tasks.md             # Phase 2 output (/speckit-tasks)
```

### Source Code (repository root)

```text
docker/
├── Dockerfile                 # ARG INCLUDE_SWU split; gate python3/net-tools;
│                              # stage-alias swu payload; drop bind-tools/wget
└── entrypoint.sh              # unchanged (guard lives in the binary)

gsm-sip-bridge/src/supervise/
├── runner.rs                  # + resolve_host() on CommandRunner (+ test impl)
└── orchestrate.rs             # resolve_epdg_ip() → use resolve_host (drop dig);
                               # start_vowifi_subsystem() → swu-availability guard

.github/workflows/
├── publish.yml                # regular release: slim only (default ARG); docs blurb
└── publish-swu.yml            # NEW: on-demand full image, :X.Y.Z-swu

Makefile                       # + docker-build-swu target
docs/ + README.md              # variant/tag guidance (FR-009)
RELEASE_NOTES.md               # call out floating-tag → slim switch
```

**Structure Decision**: Single Rust workspace with Docker packaging. Changes are
confined to `docker/Dockerfile`, two files under
`gsm-sip-bridge/src/supervise/`, CI workflows, the Makefile, and docs. No new
crates, modules, or runtime dependencies; the SWu engine code path is untouched.

## Complexity Tracking

> No Constitution Check violations — this section intentionally left empty.
