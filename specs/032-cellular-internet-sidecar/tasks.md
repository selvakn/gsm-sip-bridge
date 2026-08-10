---
description: "Task list for the cellular-internet sidecar feature"
---

# Tasks: Cellular-internet sidecar container

**Input**: Design documents from `/specs/032-cellular-internet-sidecar/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/

**Tests**: Integration testing is NON-NEGOTIABLE per the constitution. The one
branchable seam — the DNS readiness probe that defines "healthy" — has a required
integration test (T007). The QMI dial/self-heal paths touch real modem hardware
not present in CI and are validated via `quickstart.md` on a real EC20 (documented
hardware boundary, as in spec 030).

**Organization**: Grouped by user story for independent implementation/testing.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- File paths are exact and relative to the repository root.

---

## Phase 1: Setup (Shared Infrastructure)

- [X] T001 Create `docker/cellular-internet/` and a lightweight Alpine
  `docker/cellular-internet/Dockerfile` installing only `libqmi` (`qmicli`),
  `udhcpc` (BusyBox), and a DNS lookup util; sets `internet-entrypoint.sh` as
  entrypoint. Keep the layer count/size minimal (research R1).
- [X] T002 [P] Wire the build into `make`: extend the `lint` target's shellcheck
  invocation in `Makefile` to cover `docker/cellular-internet/*.sh` (currently
  only `docker/*.sh`), and add a `docker-build-internet` target. (Constitution IV)

**Checkpoint**: Image skeleton builds; new scripts are in shellcheck's scope.

---

## Phase 2: Foundational (Blocking Prerequisites)

**⚠️ CRITICAL**: The sidecar must be able to dial and report health before ANY
user story is meaningful. No user-story phase can complete without this.

- [X] T003 [US-all] In `docker/cellular-internet/internet-entrypoint.sh`:
  config validation + fail-fast — require `INTERNET_APN`; require
  `INTERNET_QMI_DEV` to exist and be a QMI (`cdc-wdm`) node, else exit nonzero
  with an actionable message and NEVER touch the AT port (FR-010, FR-002).
  Follow `docker/entrypoint.sh`'s `log()`/`set -uo pipefail` style.
- [X] T004 [US-all] In `internet-entrypoint.sh`: bring up cellular data —
  set `qmi_wwan` raw-ip on `INTERNET_QMI_DEV`, `qmicli --wds-start-network`
  for `INTERNET_APN`, `udhcpc` on the wwan iface (auto-detect
  `INTERNET_WWAN_IFACE` when unset), install the default route (FR-002, FR-009,
  research R2).
- [X] T005 [P] [US-all] Create `docker/cellular-internet/internet-healthcheck.sh`:
  DNS-resolve `INTERNET_PROBE_HOST` via `INTERNET_PROBE_RESOLVER` through the
  cellular link; exit 0 on resolve, 1 otherwise; side-effect-free except updating
  the status file (FR-003, contract `contracts/healthcheck-compose.md`).
- [X] T006 [US-all] Status output: `internet-entrypoint.sh` +
  `internet-healthcheck.sh` write `/run/internet-status`
  (`state/iface/ipv4/probe/last_change/since`) and log transitions; sidecar-local
  only — no Prometheus/Discord (FR-008, data-model state table).

**Checkpoint**: `docker run` the sidecar on a real EC20 → it dials, gets IPv4,
and its healthcheck flips healthy once DNS resolves.

---

## Phase 3: User Story 1 — Same-card internet gates the bridge (P1) 🎯 MVP

**Goal**: The bridge does not start until internet is genuinely reachable.

**Independent Test**: On a cellular-only host, bring the stack up with the
override; confirm `gsm-sip-bridge` starts only after `internet` is healthy, and
that PBX registration / ePDG succeed first try.

### Tests for User Story 1

- [X] T007 [P] [US1] Integration test for the readiness probe:
  run `docker/cellular-internet/internet-healthcheck.sh` against a resolvable
  host/resolver (expect exit 0) and against a guaranteed-unresolvable target
  (expect nonzero), asserting `/run/internet-status` reflects each. No internal
  mocks (Constitution I). Place under the repo's shell/integration test location
  (e.g. a `bats`-style test in `docker/cellular-internet/tests/` invoked by
  `make test`, or a harness that shells out — match the repo's script-test
  convention).

### Implementation for User Story 1

- [X] T008 [US1] Create `docker/docker-compose.cellular-internet.yml`: add the
  `internet` service (`network_mode: host`, `privileged: true`,
  `restart: unless-stopped`, `/dev:/dev`, `env_file: .env`) with a healthcheck
  running `internet-healthcheck.sh` at `start_period: 90s`, `interval: 10s`,
  `timeout: 5s`, `retries: 1` (research R4, contract).
- [X] T009 [US1] In the same override, inject onto `gsm-sip-bridge` via Compose
  merge: `depends_on: internet: condition: service_healthy` (FR-004). Do NOT
  modify `docker/docker-compose.yml`.

**Checkpoint**: With the override, the bridge waits for a real uplink; MVP works.

---

## Phase 4: User Story 2 — Independently managed sidecar + self-heal (P2)

**Goal**: The sidecar owns the data lifecycle and recovers from drops without
touching the bridge.

**Independent Test**: Restart only the `internet` container → connectivity
restored without restarting the bridge; a simulated link drop auto-recovers.

### Implementation for User Story 2

- [X] T010 [US2] In `internet-entrypoint.sh`: supervise loop — after the initial
  dial, poll link/session state (reuse the DNS probe) every
  `INTERNET_PROBE_INTERVAL`; on loss, tear down and re-run
  `wds-start-network` + `udhcpc`; on `/dev/cdc-wdm0` disappearance, wait for
  re-enumeration then re-dial (FR-007, research R7).
- [X] T011 [US2] Extend the status state machine to cover
  `dialing → up → probe-fail → down → redialing → up`, reflected in
  `/run/internet-status` and logs (data-model transitions).
- [X] T012 [P] [US2] Confirm `restart: unless-stopped` on the `internet` service
  in the override (hard process death) and document restart-only management in
  `quickstart.md` §5.

**Checkpoint**: US1 + US2 both work; sidecar and bridge are independently
manageable.

---

## Phase 5: User Story 3 — Deployments without same-card internet unaffected (P3)

**Goal**: Default off; existing deployments see no change.

**Independent Test**: Bring the stack up WITHOUT the override → no `internet`
container, bridge starts with no added wait (byte-for-byte today).

### Implementation for User Story 3

- [X] T013 [US3] Guard check: assert `docker compose -f
  docker/docker-compose.yml config` contains no `internet` service and the
  bridge has no `depends_on: internet` (SC-003). Add as a lightweight `make`
  check or a documented verification step in `quickstart.md` §6.
- [X] T014 [P] [US3] Verify the base `docker/docker-compose.yml` diff for this
  feature is empty (the dependency lives only in the override); note this
  invariant in the override file's header comment.

**Checkpoint**: All three stories independently functional.

---

## Phase 6: Polish & Documentation (Cross-Cutting)

- [X] T015 [P] Write/replace the operator runbook
  `docs/ec20-internet-plus-vowifi.md` (supersede the design-only
  `docs/option2-quectel-internet-plus-vowifi.md`): QMI internet bring-up via the
  sidecar, the override command, readiness gating, and "both on one card"
  verification. Retire/redirect the old doc.
- [X] T016 [P] Add `sample_configs/ec20-internet-plus-vowifi.toml` (bridge-side
  VoWiFi config for the same-card case; `modem_port` pinned,
  `tunnel_engine = "strongswan"`, header comment pointing at the sidecar) and
  index it in `sample_configs/README.md`.
- [X] T017 [P] Document the sidecar env vars in `docker/.env.example`
  (`INTERNET_APN`, `INTERNET_QMI_DEV`, `INTERNET_PROBE_HOST`,
  `INTERNET_PROBE_RESOLVER`, grace/interval) and in `docs/configuration.md`.
- [X] T018 Run `make format && make lint && make test` (shellcheck must pass on
  the new scripts) and fix any findings.
- [ ] T019 Execute `specs/032-cellular-internet-sidecar/quickstart.md` end-to-end
  on a real EC20: internet up (SC-001), gate holds when internet down (SC-002),
  default-off unchanged (SC-003), restart-only recovery (SC-004), internet + live
  VoWiFi call together (SC-005), unattended drop recovery (SC-006).

---

## Dependencies & Execution Order

- **Setup (T001–T002)** → no deps; start immediately.
- **Foundational (T003–T006)** → depends on Setup; **BLOCKS all stories**.
- **US1 (T007–T009)** → depends on Foundational. MVP.
- **US2 (T010–T012)** → depends on Foundational; independent of US1 (both build on
  the entrypoint/status but are separately testable).
- **US3 (T013–T014)** → depends on the override existing (T008) to prove the base
  file stays clean; otherwise independent.
- **Polish (T015–T019)** → after the stories it documents/validates.

### Within the sidecar scripts

- T003 (validate) → T004 (dial) → T010 (self-heal loop).
- T005 / T006 (probe + status) → T007 (probe test) and → T008 / T009 (gate).

### Parallel opportunities

- T002 ∥ T001-followups; T005 ∥ T004 (different files); T015 / T016 / T017 all [P]
  (different docs/samples); T007 can be written as soon as T005 / T006 exist.

---

## Implementation Strategy

### MVP first (User Story 1)

1. Phase 1 Setup → 2. Phase 2 Foundational → 3. Phase 3 US1 → **STOP & VALIDATE**
   (bridge gated on real internet) → deploy/demo.

### Incremental delivery

Foundation → US1 (gate, MVP) → US2 (self-heal/independent) → US3 (default-off
guard) → Polish (docs + hardware validation). Each story adds value without
breaking the previous.

---

## Notes

- [P] = different files, no dependencies.
- No bridge Rust code or `config.toml` changes — coordination is Compose-level.
- Keep the base `docker/docker-compose.yml` untouched; the dependency is
  override-only (the default-off guarantee).
- Commit after each task or logical group (Constitution III); `make format && make
  lint && make test` green before every commit (Constitution II).
