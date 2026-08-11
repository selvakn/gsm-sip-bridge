---
description: "Task list for feature 033 — slim default image with optional SWu engine"
---

# Tasks: Slim default image with optional SWu engine

**Input**: Design documents from `specs/033-slim-optional-swu-image/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/interfaces.md

**Tests**: INCLUDED — the project constitution makes integration-first testing
NON-NEGOTIABLE, and contracts/interfaces.md defines acceptance checks.

**Organization**: Grouped by user story (US1 P1, US2 P2, US3 P3). Each story is
independently testable; US2/US3 build on the `ARG INCLUDE_SWU` scaffolding
delivered as the US1 MVP.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- All paths are repo-relative from `/home/selva/projects/ec20/gsm-sip-bridge`

---

## Phase 1: Setup

- [X] T001 Record the baseline: current published image size (~119 MB) and the
  slim size target (≤50 MB) in the PR description, from `docker history`. No code
  change; establishes the SC-001 yardstick.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Rust seams the slim image and its fail-fast depend on. Pure Rust,
green on their own, no Docker needed.

**⚠️ CRITICAL**: complete before the Dockerfile work (US1/US3) so the binary no
longer needs `dig` and can detect a missing SWu payload.

- [X] T002 [P] Add `fn resolve_host(&self, host: &str) -> io::Result<Vec<Ipv4Addr>>`
  to the `CommandRunner` trait in `gsm-sip-bridge/src/supervise/runner.rs`; real
  impl uses `(host, 0u16).to_socket_addrs()` keeping `SocketAddr::V4`; implement
  it on the test `CommandRunner` double to return canned addresses (per
  contracts C3). Justify at the seam why real DNS is not used in tests.
- [X] T003 [US3] Rewire `resolve_epdg_ip` in
  `gsm-sip-bridge/src/supervise/orchestrate.rs` to call `runner.resolve_host(&fqdn)`
  and select the first `Ipv4Addr`, dropping the `dig +short A` shell-out; add a
  unit test covering: multi-address→first, empty→`None`, and the `epdg_ip` /
  `epdg_fqdn` override short-circuits (contracts C3). Depends on T002.
- [X] T004 [P] [US1] Add helper
  `fn swu_payload_available(marker: &Path, dialer: &Path) -> bool` (injectable
  paths, unit-tested) and gate on it in `run()`
  (`gsm-sip-bridge/src/supervise/orchestrate.rs`) **before line discovery**:
  when `tunnel_engine == "swu"` and the payload is absent (and the
  `GSM_SIP_BRIDGE_SWU_PAYLOAD` override is unset), exit `ExitCode::FAILURE`
  naming the `-swu` image (contracts C4). Placed in `run()` rather than
  `start_vowifi_subsystem` so it holds with no modem/lines and on the
  discover-retry path (SC-005). Real paths:
  `/etc/gsm-sip-bridge/swu-available` + `/opt/SWu-IKEv2/swu_emulator.py`.

**Checkpoint**: `make format && make lint && make test` green; binary resolves
DNS in-process and guards SWu-on-slim. (T003 and T004 both touch orchestrate.rs —
do them sequentially, not in parallel with each other.)

---

## Phase 3: User Story 1 - Slim default image (Priority: P1) 🎯 MVP

**Goal**: Default build omits Python/SWu; strongSwan path unchanged; slim ≤50 MB.

**Independent Test**: `make docker-build` → image has no python3/pydeps/SWu/marker
and is ~45–50 MB; strongSwan VoWiFi boots; `swu` engine fails fast.

- [X] T005 [US1] In `docker/Dockerfile`: add `ARG INCLUDE_SWU=false`; add
  `swu-false` (empty `/swu-root`) and `swu-true` (assembles
  `/swu-root/opt/SWu-IKEv2`, `/swu-root/opt/pydeps`, and
  `/swu-root/etc/gsm-sip-bridge/swu-available`, `FROM python-builder`) stages;
  add `FROM swu-${INCLUDE_SWU} AS swu-payload` (research R1/R2).
- [X] T006 [US1] In the runtime stage of `docker/Dockerfile`: re-declare
  `ARG INCLUDE_SWU`; gate python3 with
  `RUN if [ "$INCLUDE_SWU" = "true" ]; then apk add --no-cache python3; fi`;
  replace the fixed `COPY --from=python-builder …` lines with
  `COPY --from=swu-payload /swu-root/ /`. Leave strongSwan/vpcd COPYs untouched.
- [X] T007 [US1] Build slim (`make docker-build`) and assert per quickstart:
  no `python3`, no `/opt/pydeps`, no `/opt/SWu-IKEv2`, no
  `/etc/gsm-sip-bridge/swu-available`; image size ≤50 MB (SC-001/SC-002).
- [X] T008 [US1] Verify slim runtime behavior: strongSwan path boots unchanged
  (SC-003), and starting with `tunnel_engine = "swu"` exits fast with the
  actionable message from T004 without spawning line processes (SC-005, C4).

**Checkpoint**: Slim image is the shippable MVP.

---

## Phase 4: User Story 2 - Full image on demand, SWu kept in CI (Priority: P2)

**Goal**: Full image builds on demand under `-swu`; regular release stays slim;
SWu Rust code/tests remain in every build. Depends on US1 (needs the ARG).

**Independent Test**: `make docker-build-swu` yields a full superset image;
`publish-swu.yml` publishes `:X.Y.Z-swu`; a normal release publishes no `-swu`
tag; `make test`/`make lint` still cover the SWu path.

- [X] T009 [P] [US2] Add `docker-build-swu` target to `Makefile`
  (`docker build -f docker/Dockerfile --build-arg INCLUDE_SWU=true -t gsm-sip-bridge:swu .`);
  add it to `.PHONY` and give it a `## ` help description.
- [X] T010 [US2] Build full (`make docker-build-swu`) and assert per quickstart:
  `python3`, `/opt/SWu-IKEv2/swu_emulator.py`, the `swu-available` marker, and
  `route`/`ifconfig` (net-tools) are all present; image is a strict superset of
  slim (C1).
- [X] T011 [US2] Add `.github/workflows/publish-swu.yml`: `workflow_dispatch`
  (input `version`), builds `docker/Dockerfile` with `INCLUDE_SWU=true` for
  linux/amd64 + linux/arm64, pushes `${IMAGE_NAME}:${version}-swu-<platform>`,
  then a merge job creating the `${version}-swu` multi-arch manifest; separate
  GHA cache scope from the slim build (research R5, C2).
- [X] T012 [US2] Confirm `.github/workflows/publish.yml` publishes the slim
  image only (no `INCLUDE_SWU=true` leg) under canonical tags/`latest`; update
  the release-body Docker blurb and add a `RELEASE_NOTES.md` note that
  floating/canonical tags now resolve to slim and SWu users must pin `-swu`
  (FR-008, edge case, C2).
- [X] T013 [P] [US2] Assert FR-005a: `grep` confirms no `#[cfg(feature …)]`
  gates `SwuEngine`/its dispatch; `make lint && make test` remain green with the
  SWu path compiled and covered (C5). No production code change expected.

**Checkpoint**: Both variants build; only slim ships on release; SWu stays in CI.

---

## Phase 5: User Story 3 - Drop unused runtime tooling (Priority: P3)

**Goal**: Remove the `bind-tools` and `wget` apk packages from both variants;
gate `net-tools` to the full image. Depends on T003 (native DNS) so the slim
image resolves without `dig`. (Package removal only — busybox still provides
`wget`/`route`/`ifconfig` applets in both.)

**Independent Test**: `dig` absent and `bind-tools`/`wget` packages not installed
in both; `net-tools` package absent in slim, present in full; ePDG hostname
resolution + tunnel bring-up still work.

- [X] T014 [US3] In `docker/Dockerfile` runtime `apk add`: remove the
  `bind-tools` and `wget` packages (verified unused — R4; `wget` only appears in
  pydeps docstrings, `dig`'s only caller was replaced in T003).
- [X] T015 [US3] Move `net-tools` out of the always-installed set into the
  `INCLUDE_SWU=true` conditional `RUN`. It is load-bearing for the SWu dialer's
  `/sbin/ifconfig` (net-tools overwrites the busybox symlink with the real
  binary the dialer parses); its `route` never wins over busybox in PATH.
  strongSwan/`supervise` use only iproute2's `ip` (R4).
- [X] T016 [US3] Rebuild both variants and assert (via `apk info -e`, not
  `command -v`, since busybox shadows `wget`/`route`): `dig` absent and
  `bind-tools`/`wget` packages not installed in both; `net-tools` package absent
  in slim and present in full (real `/sbin/ifconfig`); ePDG FQDN resolution
  succeeds without `bind-tools` (SC-006, C1).

**Checkpoint**: Both images trimmed to their final footprint.

---

## Phase 6: Polish & Cross-Cutting

- [X] T017 [P] Documentation (FR-009): in `README.md` and the relevant
  `docs/` page, state that the slim image is the default published artifact, that
  the full/SWu image is built on demand via `publish-swu.yml` under the `-swu`
  tag, and how to choose between them.
- [X] T018 Run the full `quickstart.md` validation end-to-end against freshly
  built slim and full images.
- [X] T019 Final gate: `make format && make lint && make test` green;
  `shellcheck` clean for any new shell in workflows/Makefile.

---

## Dependencies & Execution Order

### Phase dependencies

- **Setup (P1)**: none.
- **Foundational (P2)**: blocks US1/US3 Dockerfile work (binary must resolve DNS
  in-process and detect a missing SWu payload first).
- **US1 (P3 phase)**: needs Foundational; delivers the `ARG INCLUDE_SWU`
  scaffolding — the MVP.
- **US2 (P4 phase)**: needs US1 (builds the full image via the same ARG).
- **US3 (P5 phase)**: needs Foundational T003 (native DNS) before removing
  `bind-tools`; otherwise independent of US2.
- **Polish (P6)**: after the stories it documents/validates are done.

### Within stories

- Tests are written to fail first, then made green (constitution II/TDD default).
- Rust seams (T002→T003; T004) before Dockerfile changes that rely on them.
- `make test`/`make lint`/`make format` green before every commit (constitution).

### Parallel opportunities

- T002 and T004 touch different files (`runner.rs` vs `orchestrate.rs`) — [P].
  T003 and T004 both edit `orchestrate.rs` — sequential.
- T009 and T013 are independent of the Dockerfile edits — [P].
- T017 (docs) is [P] with final verification.

---

## Implementation Strategy

### MVP first

1. Phase 1 Setup → Phase 2 Foundational → Phase 3 US1.
2. **STOP & VALIDATE**: slim image ≤50 MB, strongSwan unchanged, swu fails fast.
3. Ship the slim image as the new default (US1 alone is a complete win).

### Incremental delivery

1. US1 (slim MVP) → 2. US2 (on-demand full image + CI + docs) → 3. US3 (final
   apk trim). Each phase ends green and is independently demoable.

---

## Notes

- Commit after each task (constitution III, atomic commits); each commit green.
- The SWu **Rust** engine is never feature-gated — only the image payload/apk
  packages differ between variants (FR-005a).
- Pre-commit, always run `make format && make lint && make test` (CLAUDE.md).
