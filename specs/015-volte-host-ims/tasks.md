# Tasks: Host-Side IMS Registration over LTE (VoLTE)

**Input**: Design documents from `/specs/015-volte-host-ims/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: Test tasks ARE included. The project constitution makes
integration-first testing non-negotiable (Principle I) and requires a green
suite on every commit (Principle II), so tests are not optional here.

**Organization**: Grouped by user story. Phases are ordered by the spec's
priorities (P1 → P2 → P3); note this differs from the order they were actually
delivered, because Gate G1's negative result demoted US2 from P1 to P3 after
implementation had begun.

**Status**: This list was generated *after* implementation and cross-validated
against the tree at commit `a54a711`. `[x]` means verified present in code with
evidence; `[ ]` means genuinely outstanding. See "Cross-Validation Report" at
the end.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story this task belongs to

## Path Conventions

Single Rust workspace. Source at `gsm-sip-bridge/src/`, integration tests at
`gsm-sip-bridge/tests/`, feature docs at `specs/015-volte-host-ims/`.

---

## Phase 1: Setup

**Purpose**: Establish the feature's place in the workspace.

- [x] T001 Create the feature spec directory and spec at `specs/015-volte-host-ims/spec.md`
- [x] T002 [P] Create the implementation plan at `specs/015-volte-host-ims/plan.md`
- [x] T003 [P] Record hardware findings in `specs/015-volte-host-ims/research.md`
- [x] T004 [P] Define entities in `specs/015-volte-host-ims/data-model.md`
- [x] T005 [P] Define the transport contract in `specs/015-volte-host-ims/contracts/ims-transport-contract.md`
- [x] T006 [P] Define the CLI contract in `specs/015-volte-host-ims/contracts/volte-cli-contract.md`
- [x] T007 [P] Write reproduction and validation steps in `specs/015-volte-host-ims/quickstart.md`
- [x] T008 Register the `volte` module in `gsm-sip-bridge/src/lib.rs`

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: The shared seam every user story depends on. **MUST complete
before any user story.** Also the phase that protects the production VoWiFi
path (FR-019).

- [x] T009 Define `ImsTransport`, `ImsTransportHandle` in `gsm-sip-bridge/src/ims/transport.rs`
- [x] T010 Define `TransportStage` and `TransportError` so failures keep their stage (FR-015) in `gsm-sip-bridge/src/ims/transport.rs`
- [x] T011 Implement `EpdgTransport` wrapping the existing P-CSCF file read in `gsm-sip-bridge/src/ims/transport.rs`
- [x] T012 Make `EpdgTransport::teardown` a documented no-op (the tunnel is supervised elsewhere) in `gsm-sip-bridge/src/ims/transport.rs`
- [x] T013 Export the transport module from `gsm-sip-bridge/src/ims/mod.rs`
- [x] T014 Migrate `run_inner`'s inline `read_pcscf` onto `EpdgTransport` in `gsm-sip-bridge/src/ims/agent.rs`
- [x] T015 [P] Contract tests: idempotent prepare, IPv6 P-CSCF, staged errors, teardown safety in `gsm-sip-bridge/src/ims/transport.rs`
- [x] T016 Verify the VoWiFi suite passes unmodified after the refactor (FR-019, SC-006)

---

## Phase 3: User Story 1 — Attach to the carrier's IMS network over cellular (P1)

**Goal**: The bridge holds a network attachment dedicated to the carrier's IMS
service, usable from its own software.

**Independent test**: One command yields an attachment reporting the
carrier-assigned IMS service name and address family.

### Gate

- [x] T017 [US1] Verify on hardware that the carrier grants an IMS PDN to a host-controlled context (research.md R1)

### Implementation

- [x] T018 [P] [US1] Define `ImsPdn` with assigned APN, bearer id and both address families in `gsm-sip-bridge/src/volte/pdn.rs`
- [x] T019 [P] [US1] Implement quote-aware AT field splitting in `gsm-sip-bridge/src/volte/pdn.rs`
- [x] T020 [US1] Parse `+CGCONTRDP` for assigned APN and bearer id in `gsm-sip-bridge/src/volte/pdn.rs`
- [x] T021 [US1] Parse `+CGPADDR`, treating an all-zero IPv4 address as unassigned rather than as failure in `gsm-sip-bridge/src/volte/pdn.rs`
- [x] T022 [P] [US1] Parse `+CGACT` activation state in `gsm-sip-bridge/src/volte/pdn.rs`
- [x] T023 [P] [US1] Parse `+QNETDEVCTL` binding, treating `0,0,0,0` as unbound in `gsm-sip-bridge/src/volte/pdn.rs`
- [x] T024 [P] [US1] Convert the 3GPP dotted-byte address form to IPv6 in `gsm-sip-bridge/src/volte/pdn.rs`
- [x] T025 [US1] Implement `bring_up` with reuse detection (FR-004) and displaced-context capture (FR-005) in `gsm-sip-bridge/src/volte/pdn.rs`
- [x] T026 [US1] Implement `tear_down` restoring the previous binding (FR-005) in `gsm-sip-bridge/src/volte/pdn.rs`
- [x] T027 [US1] Poll for the network-assigned address after activation (research R11) in `gsm-sip-bridge/src/volte/pdn.rs`
- [x] T028 [US1] Retry `AT+CGACT` on transient `+CME ERROR: 3` after a deactivate in `gsm-sip-bridge/src/volte/pdn.rs`
- [x] T029 [US1] Derive the network-expected link-local from the assigned IID (FR-024) in `gsm-sip-bridge/src/volte/pdn.rs`
- [x] T030 [US1] Implement the interface configuration step sequence in `gsm-sip-bridge/src/volte/netcfg.rs`
- [x] T031 [US1] Set `addr_gen_mode=none` before the link comes up (FR-024, research R7) in `gsm-sip-bridge/src/volte/netcfg.rs`
- [x] T032 [US1] Install both the network-assigned `/128` and accept RA-derived SLAAC in `gsm-sip-bridge/src/volte/netcfg.rs`
- [x] T033 [US1] Wait for carrier before configuring (research R12) in `gsm-sip-bridge/src/volte/netcfg.rs`
- [x] T034 [US1] Wait out duplicate address detection before soliciting (research R12) in `gsm-sip-bridge/src/volte/netcfg.rs`
- [x] T035 [US1] Treat the default route, not address presence, as proof of routability (research R10) in `gsm-sip-bridge/src/volte/netcfg.rs`
- [x] T036 [US1] Implement `teardown` reverting host configuration in `gsm-sip-bridge/src/volte/netcfg.rs`
- [x] T037 [US1] Implement `attach`/`detach`/`status` orchestration in `gsm-sip-bridge/src/volte/mod.rs`
- [x] T038 [US1] Report assigned APN, bearer id, family, addresses and routability (FR-003) in `gsm-sip-bridge/src/volte/mod.rs`
- [x] T039 [US1] Warn before displacing the existing data-path binding (FR-006) in `gsm-sip-bridge/src/volte/mod.rs`
- [x] T040 [US1] Add the `volte-pdn` subcommand with `up`/`down`/`status` in `gsm-sip-bridge/src/cli.rs`
- [x] T041 [US1] Wire the `volte-pdn` handler in `gsm-sip-bridge/src/main.rs`

### Tests

- [x] T042 [P] [US1] Unit tests for all AT response parsing against verbatim hardware transcripts in `gsm-sip-bridge/src/volte/pdn.rs`
- [x] T043 [P] [US1] Unit tests asserting the interface configuration *ordering* invariants in `gsm-sip-bridge/src/volte/netcfg.rs`
- [x] T044 [US1] Hardware validation: attach, idempotent re-attach, teardown, status-after-down, double-teardown

---

## Phase 4: User Story 3 — Register with the carrier's IMS core over cellular (P2)

**Goal**: An accepted IMS registration over LTE using the SIM's credentials.

**Independent test**: `volte-register` returns an accepted registration.

### Gates

- [x] T045 [US3] **Gate G3**: obtain a P-CSCF address (captured `2400:5200:a100:819::6` from a Vi ePDG tunnel; research R13)
- [x] T046 [US3] **Gate G2**: verify Gm IPsec over IPv6 (satisfied by the G3 capture run; research R14)

### Implementation

- [x] T047 [US3] Implement `LteImsPdnTransport` satisfying `ImsTransport` in `gsm-sip-bridge/src/volte/mod.rs`
- [x] T048 [US3] Make `P-Access-Network-Info` configurable via `ImsRegisterConfig` in `gsm-sip-bridge/src/ims/mod.rs`
- [x] T049 [US3] Preserve `3GPP-WLAN` at both existing call sites (FR-019) in `gsm-sip-bridge/src/ims/agent.rs` and `gsm-sip-bridge/src/main.rs`
- [x] T050 [US3] Parse `AT+QENG="servingcell"` into MCC/MNC/TAC/ECI in `gsm-sip-bridge/src/volte/mod.rs`
- [x] T051 [US3] Build the E-UTRAN access-network value, never falling back to WLAN, in `gsm-sip-bridge/src/volte/mod.rs`
- [x] T052 [US3] Derive the home PLMN from the SIM rather than requiring flags in `gsm-sip-bridge/src/main.rs`
- [x] T053 [US3] Add the `volte-register` subcommand in `gsm-sip-bridge/src/cli.rs`
- [x] T054 [US3] Report the failing stage on failure (FR-015) in `gsm-sip-bridge/src/main.rs`
- [x] T055 [US3] Refuse to register when the PDN is attached but unroutable in `gsm-sip-bridge/src/main.rs`

### Tests

- [x] T056 [P] [US3] Unit tests for serving-cell parsing and the E-UTRAN value in `gsm-sip-bridge/src/volte/mod.rs`
- [x] T057 [US3] Hardware validation: accepted `200 OK` registration over LTE

---

## Phase 5: User Story 2 — Determine the IMS entry point, reporting definitively (P3)

**Goal**: Probe every supported mechanism and report per-method results.
Success is a complete, accurate report — not necessarily a discovered address.

**Independent test**: `volte-discover` prints a per-method breakdown.

### Gate

- [x] T058 [US2] **Gate G1**: spike all discovery mechanisms on live hardware (result: all negative; research R2)

### Implementation

- [x] T059 [P] [US2] Define `DiscoveryMethod`, `MethodResult`, `MethodAttempt`, `DiscoveryReport` in `gsm-sip-bridge/src/volte/pcscf.rs`
- [x] T060 [US2] Keep `NoResult` and `Failed` distinct throughout in `gsm-sip-bridge/src/volte/pcscf.rs`
- [x] T061 [US2] Implement the DHCPv6 Information-Request builder and reply parser (RFC 3319) in `gsm-sip-bridge/src/volte/pcscf.rs`
- [x] T062 [US2] Implement the DHCPv6 probe without `unsafe`, checking link-local readiness first, in `gsm-sip-bridge/src/volte/pcscf.rs`
- [x] T063 [US2] Implement the PCO probe, distinguishing firmware truncation from carrier silence, in `gsm-sip-bridge/src/volte/pcscf.rs`
- [x] T064 [US2] Implement the DNS probe, reporting "nowhere to ask" rather than hanging, in `gsm-sip-bridge/src/volte/pcscf.rs`
- [x] T065 [US2] Implement the `EpdgCache` source reading the VoWiFi capture in `gsm-sip-bridge/src/volte/pcscf.rs`
- [x] T066 [US2] Implement the ordered chain with config override precedence (FR-008, FR-010) in `gsm-sip-bridge/src/volte/pcscf.rs`
- [x] T067 [US2] Report every attempt even after a hit (FR-011) in `gsm-sip-bridge/src/volte/pcscf.rs`
- [x] T068 [US2] Add the `volte-discover` subcommand with `--method` isolation in `gsm-sip-bridge/src/cli.rs`
- [x] T069 [US2] Wire the handler, deriving the realm from the SIM, in `gsm-sip-bridge/src/main.rs`

### Tests

- [x] T070 [P] [US2] Unit tests for DHCPv6 encode/decode including the carrier's actual empty reply in `gsm-sip-bridge/src/volte/pcscf.rs`
- [x] T071 [P] [US2] Unit tests for chain ordering, override precedence, and full reporting in `gsm-sip-bridge/src/volte/pcscf.rs`
- [x] T072 [US2] Hardware validation: chain reproduces the G1 findings through production code

---

## Phase 6: User Story 4 — Keep the registration alive and observable (P3)

**Goal**: The registration renews before expiry and its state is visible.

**Independent test**: `volte-status` reports state and time remaining.

### Implementation

- [x] T073 [US4] Reuse `ims::renewal_due`, `RegistrationState`, `RegistrationStatus` rather than new types (FR-022, SC-007) in `gsm-sip-bridge/src/volte/registration.rs`
- [x] T074 [US4] Take the registration lifetime from what the network granted in `gsm-sip-bridge/src/volte/registration.rs`
- [x] T075 [US4] Implement the renewal loop with bounded backoff (FR-016) in `gsm-sip-bridge/src/volte/registration.rs`
- [x] T076 [US4] Record renewal failures with a reason distinguishing rejection from transport error (FR-023) in `gsm-sip-bridge/src/volte/registration.rs`
- [x] T077 [US4] Publish state to a status file, best-effort so it never breaks a registration, in `gsm-sip-bridge/src/volte/registration.rs`
- [x] T078 [US4] Add `--once` and `--status-path` to `volte-register` in `gsm-sip-bridge/src/cli.rs`
- [x] T079 [US4] Add the `volte-status` subcommand reporting attachment plus registration in `gsm-sip-bridge/src/cli.rs` and `gsm-sip-bridge/src/main.rs`

### Tests

- [x] T080 [P] [US4] Unit tests for granted-expiry precedence, backoff bounding, status round-trip in `gsm-sip-bridge/src/volte/registration.rs`
- [x] T081 [P] [US4] Test that an injected newline in a failure reason cannot corrupt the status format in `gsm-sip-bridge/src/volte/registration.rs`
- [x] T082 [US4] Hardware validation: registration accepted, status published, `volte-status` reports state
- [ ] T083 [US4] **SC-004**: observe two consecutive automatic renewals on hardware (**1 of 2 observed** at 10:16:40, exactly 55 min after registration = 3600s expiry − 300s headroom; second due ~11:11)

---

## Phase 7: Polish & Cross-Cutting Concerns

- [x] T084 [P] Enforce VoWiFi/VoLTE mutual exclusion before touching the modem in `gsm-sip-bridge/src/volte/guard.rs`
- [x] T085 Match agent detection on argv structure, not substring, in `gsm-sip-bridge/src/volte/guard.rs`
- [x] T086 [P] Add a VoLTE registration lock with stale-lock takeover in `gsm-sip-bridge/src/volte/guard.rs`
- [x] T087 Add the `[volte]` config section with load-time validation in `gsm-sip-bridge/src/config/mod.rs`
- [x] T088 [P] Document the `[volte]` section in `config.toml.example`
- [x] T089 [P] Add VoLTE registration/PDN gauges and an attempts counter in `gsm-sip-bridge/src/metrics/mod.rs`
- [x] T090 Wire metrics into the registration lifecycle and attachment in `gsm-sip-bridge/src/volte/registration.rs` and `gsm-sip-bridge/src/volte/mod.rs`
- [x] T091 [P] Remove dead public API (`prepare_pcscf`, `pcscf_socket`, `required_link_local`)
- [x] T092 [P] Correct the discovery-method table in `specs/015-volte-host-ims/data-model.md`
- [x] T093 [P] Correct the CLI contract to match the shipped commands in `specs/015-volte-host-ims/contracts/volte-cli-contract.md`
- [x] T094 Keep `make lint` clean and the workspace suite green on every commit (Constitution II)
- [ ] T095 [P] Refresh `specs/015-volte-host-ims/quickstart.md` step 4 to the final CLI surface
- [ ] T096 [P] Re-validate `specs/015-volte-host-ims/checklists/requirements.md` against the amended spec
- [ ] T097 Supervise VoLTE from `docker/entrypoint.sh` alongside VoWiFi
- [ ] T098 [P] Document host-side VoLTE in `README.md` and `docs/operations.md`
- [ ] T099 [P] Clarify in `docs/ec20-volte-setup.md` that it covers modem-internal VoLTE on EC20, not this path
- [ ] T100 Resolve research R9: confirm which source address the network routes

---

## Dependencies

```
Phase 1 (Setup)
      ↓
Phase 2 (Foundational — ImsTransport seam)   ← blocks everything
      ↓
Phase 3 (US1 — attachment, P1)               ← gated by G1-era hardware proof
      ↓
Phase 4 (US3 — registration, P2)             ← gated by G2 + G3
      ↓
Phase 5 (US2 — discovery, P3)                ← independent of US3 after demotion
      ↓
Phase 6 (US4 — lifecycle, P3)                ← needs US3
      ↓
Phase 7 (Polish)
```

**Story independence**: US1 stands alone and was shippable before any gate
cleared. US2 became independent of everything once demoted to diagnostics. US3
depends on US1 (attachment) and on Gates G2/G3. US4 depends on US3.

## Parallel Opportunities

- **Phase 3**: T018–T024 are all pure parsers in one file, independently
  writable; T042/T043 test different modules.
- **Phase 5**: T061–T065 are four independent probes.
- **Phase 7**: T088, T089, T091, T092, T093, T095, T096, T098, T099 touch
  disjoint files.

## Implementation Strategy

**MVP**: Phases 1–3 (US1). Delivers a routable IMS PDN and is independently
valuable as a diagnostic, which is what made it safe to ship while Gate G3 was
still unresolved.

**Increment 2**: Phase 4 (US3) — the headline outcome, once a P-CSCF exists.
**Increment 3**: Phases 5–6 (US2, US4).
**Increment 4**: Phase 7.

---

# Cross-Validation Report

Validated against commit `a54a711`. Method: locate each task's artefact in the
tree, confirm tests exist, and re-check hardware claims against recorded
transcripts.

## Summary

| | Count |
|---|---|
| Total tasks | 100 |
| Verified implemented | **93** |
| Outstanding | **7** |

**443 workspace tests pass; `make lint` clean; 0 `unsafe` in `gsm-sip-bridge/src`.**

## Evidence for the implemented tasks

| Area | Evidence |
|---|---|
| `volte/pdn.rs` | 14 public fns, 11 tests |
| `volte/netcfg.rs` | 15 public fns, 12 tests |
| `volte/pcscf.rs` | 20 public fns, 21 tests |
| `volte/registration.rs` | 9 public fns, 15 tests |
| `volte/guard.rs` | 7 public fns, 14 tests |
| `volte/mod.rs` | 9 public fns, 13 tests |
| `ims/transport.rs` | trait + 2 impls, 8 tests |
| CLI | `VoltePdn`, `VolteDiscover`, `VolteRegister`, `VolteStatus` all present |

**Requirement anchors**: FR-003/004/005/006/008/009/010/011/015/016/019/022/
023/024 are all cited in code comments at their implementation sites.

**FR-012 / FR-013 (SIM credentials, identity derivation) carry no anchor in
`volte/` — correctly.** A grep for identity or AKA logic under `volte/` returns
nothing: that code is *reused* from `ims/`, which is exactly what FR-017 and
SC-007 require. Absence of code here is the evidence, not a gap.

**SC-006 (no VoWiFi regression)** — `git diff 5923749..HEAD -- gsm-sip-bridge/tests/`
is **empty**: not one VoWiFi test was modified across the whole feature, and
they all pass. Outside `volte/`, the feature touched only `cli.rs`,
`config/mod.rs`, `ims/agent.rs`, `ims/mod.rs`, `ims/transport.rs`, `lib.rs`,
`main.rs`, `metrics/mod.rs` — no VoWiFi module was edited.

## Outstanding tasks

| ID | Task | Why it matters |
|---|---|---|
| **T083** | Observe two consecutive renewals (SC-004) | **The only unmet success criterion — now half met.** The first renewal fired at 10:16:40, exactly 55 min after registration (3600s granted expiry − 300s headroom), and was accepted; the status file advanced accordingly. The second is due ~11:11. The renewal *timing* is therefore proven on hardware; only "two consecutive" remains |
| T095 | Refresh `quickstart.md` step 4 | Lists pre-final commands; misleads a new operator |
| T096 | Re-validate the spec checklist | Records a pass against the pre-amendment spec |
| T097 | `entrypoint.sh` supervision | The real integration gap — VoLTE cannot run unattended. Unblocked now that `[volte]` config exists |
| T098 | README / operations docs | Host-side VoLTE is undocumented for operators |
| T099 | Clarify `ec20-volte-setup.md` | Covers EC20 + Airtel modem-internal VoLTE; can mislead now a competing path exists |
| T100 | Resolve R9 (routed source address) | Benign today — both addresses are installed — but unproven |

## Honest caveats on "verified"

- **T044, T057, T072, T082** are hardware validations. They were observed once
  each against one SIM on one carrier. They are not automated and will not
  re-run in CI.
- **T046 (Gate G2)** was satisfied *incidentally* by the ePDG capture run
  rather than by the dedicated IPv6 XFRM exercise the plan specified. The
  evidence is stronger than the planned test (a real registration completed
  over the SA), but it is not the test that was written down.
- **T016 / SC-006** is verified structurally (no VoWiFi file or test changed)
  and by a green suite. A **live VoWiFi call has still not been placed since
  the `ImsTransport` refactor** — `quickstart.md` requires this, and it remains
  the one non-regression check not performed.
