---
description: "Task list for Multi-Card VoWiFi (013)"
---

# Tasks: Multi-Card VoWiFi

**Input**: Design documents from `/specs/013-multi-card-vowifi/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: Included — the constitution's Integration-First Testing principle is NON-NEGOTIABLE for
this repo and every prior feature (discovery, config, vowifi/mod.rs) ships table-driven
integration tests alongside the code; this feature follows the same convention.

**Organization**: Tasks are grouped by user story (spec.md's US1/US2/US3, priority order)
following feature 013's plan.md. All file paths are relative to the repo root.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no unfinished dependency)
- **[Story]**: US1 (P1, auto-discovery), US2 (P2, concurrent lines), US3 (P3, per-line ID/status)

---

## Phase 1: Setup

**Purpose**: Nothing new to scaffold — `serde`, `serde_json`, `tempfile` are already workspace
dependencies (checked against `gsm-sip-bridge/Cargo.toml`); no new crate, no new Makefile target
per plan.md. This phase only stakes out the new test-fixture file so later tasks don't collide.

- [X] T001 Create `gsm-sip-bridge/tests/test_vowifi_lines.rs` with a module doc comment
      describing its scope (role assignment / line-table resolution / per-line resource
      derivation, data-model.md) and no tests yet — the file US2/US3 test tasks below append to.

**Checkpoint**: Nothing else blocks starting Phase 2.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Config schema and shared data types every user story's code depends on.

**⚠️ CRITICAL**: No user story task can begin until this phase is complete.

- [X] T002 Add `max_lines: u32` (default 8, research.md item 7) to `VowifiConfig` in
      `gsm-sip-bridge/src/config/mod.rs`; parse `[vowifi].max_lines` with the existing
      `as_u64_range`-style helpers; default-value and rejects-out-of-range unit tests alongside
      the existing `vowifi_*` tests in the same file.
- [X] T003 Add optional `[[vowifi.line]]` array-of-tables parsing to `config/mod.rs` (FR-009):
      each entry may set `modem_serial` or `modem_port` (explicit override) plus optional
      `mcc`/`mnc`/`imsi_override`; store as `Vec<VowifiLineOverride>` on `VowifiConfig`; empty
      when absent (today's behavior unaffected). Unit tests: absent array parses to empty vec,
      one entry parses, unknown keys warn (matching `warn_unknown_keys_in` convention).
- [X] T004 [P] Define the shared data-model types in a new file
      `gsm-sip-bridge/src/vowifi/discovery.rs`: `ProbedModem`, `SimStatus`, `RoleAssignment`,
      `ResolvedLine`, `LineTable` (type alias `Vec<ResolvedLine>`), and `LineResolution` (the
      serializable artifact, `#[derive(Serialize, Deserialize)]`) exactly per data-model.md's
      field tables. Wire the module into `gsm-sip-bridge/src/vowifi/mod.rs` (`pub mod discovery;`).
      No logic yet — plain structs/enums plus `Serialize`/`Deserialize` derives, so this can
      compile and be imported by every later task.
- [X] T005 [P] Add `SimStatus`-reading helper `read_sim_status(at: &mut AtCommander) -> SimStatus`
      to `gsm-sip-bridge/src/modules/usim.rs` (or call site in the new discovery code, whichever
      keeps `AT+CIMI`/`AT+CPIN?` logic in one place) — reuses the existing `AtCommander` the way
      `vowifi/imsi.rs`/`vowifi/plmn.rs` already do; no behavior change to existing callers.

**Checkpoint**: Config schema and shared types exist; `cargo test --workspace` still green;
User Story phases can now begin.

---

## Phase 3: User Story 1 - Auto-Discovery of AT-Capable Modems (Priority: P1) 🎯 MVP

**Goal**: Discover every attached VoWiFi-capable modem (audio-capable or not) without a
hand-typed serial path, probing for the AT-capable interface instead of assuming one, and
reading each SIM's identity.

**Independent Test** (from spec.md): attach a VoWiFi-capable modem, leave `modem_port` unset,
start the system, verify it logs the discovered modem, its AT port, and its SIM identity.

### Tests for User Story 1

- [X] T006 [P] [US1] Extend `gsm-sip-bridge/tests/test_discovery.rs` with a fake-serial-transport
      case (mirroring `at_commander.rs`'s `MockStream` pattern) proving AT-probing finds the
      right `ttyUSB*` among several candidates on one fake USB device directory, independent of
      which `bInterfaceNumber` it is — i.e. no reliance on a fixed table entry.
- [X] T007 [P] [US1] Add a `test_discovery.rs` case: a device matching the audio-less model
      (today's `EC200`/`0901`) is no longer skipped — it now appears in scan output with
      `audio_device: None` and a discovered `at_port` (FR-003), using the existing
      `fake_device_dir` tempfile helper.
- [X] T008 [P] [US1] Add `test_discovery.rs` cases for SIM-read outcomes: `AT+CPIN?` reports
      locked → `SimStatus::Locked`; no SIM response → `SimStatus::Absent`; `AT+CIMI` succeeds →
      `SimStatus::Ready{ imsi }` (FR-006, edge case "PIN-locked or not ready").
- [X] T009 [P] [US1] Add `test_vowifi_lines.rs` cases for `RoleAssignment`: an audio-capable
      modem defaults to circuit-switched, an audio-less one defaults to VoWiFi (FR-008); an
      explicit `[[vowifi.line]]` override claims a modem regardless of audio capability (FR-009);
      no modem appears in both output vectors for any input set (FR-007).
- [X] T010 [P] [US1] Add `test_vowifi_lines.rs` cases for `LineTable` resolution ordering:
      stable card-id (hardware-serial) order regardless of USB enumeration order; a failed
      modem (no AT port / bad SIM) is reported and excluded, remaining modems still resolve
      (acceptance scenario 3); the N=1 case's derived `VowifiConfig` equals today's unindexed
      defaults exactly (`netns="ims"`, `strongswan_tun_iface="tun23"`,
      `pcscf_source_path="/tmp/pcscf"`, etc. — FR-020, data-model.md validation rules).
- [X] T011 [P] [US1] Add a `test_vowifi_lines.rs` case for the `max_lines` bound (FR-016):
      more usable modems than `max_lines` resolves exactly `max_lines` lines, in card-id order,
      with the excess reported as skipped (not silently dropped).

### Implementation for User Story 1

- [X] T012 [US1] Rewrite `gsm-sip-bridge/src/modules/discovery.rs`: delete
      `KnownDevice.at_interface_number` and its lookup; add `probe_at_port(dev_path) ->
      Option<PathBuf>` that opens each `ttyUSB*` child interface found under the device and
      sends a live `AT\r` expecting `OK` (short timeout, reusing `AtCommander`'s transport
      abstraction so T006's fake transport can drive it in tests); stop excluding matches with no
      `audio_device`; return `ProbedModem` (T004's type) instead of today's audio-gated
      `DiscoveredModule`. Keep `derive_module_id` unchanged (FR-005 — same identifier scheme).
- [X] T013 [US1] In the same file, wire the SIM read (T005's helper) into the scan: after a
      working AT port is found, read `SimStatus`; a modem with `at_port: None` or a non-`Ready`
      `SimStatus` is logged with its reason (FR-006) and does not appear in the returned
      `Vec<ProbedModem>` used for line-table resolution (it may still appear in a raw "attempted"
      log line for operator visibility).
- [X] T014 [US1] Implement `RoleAssignment::from_probed(modems: &[ProbedModem], overrides: &[
      VowifiLineOverride]) -> RoleAssignment` in `gsm-sip-bridge/src/vowifi/discovery.rs`
      (data-model.md's partition function — default audio-based split, override always wins,
      FR-007/008/009).
- [X] T015 [US1] Implement `LineTable::resolve(assignment: &RoleAssignment, config: &VowifiConfig)
      -> LineTable` in the same file: stable card-id ordering, `max_lines` bound (FR-016), and per-
      line `VowifiConfig` derivation using the research.md item 5 formulas (netns, XFRM if_id/
      iface, veth iface/addrs, vpcd_port, pcscd socket path, charon vici/log paths, pcscf_source_
      path) as a pure function of `(base_config, index)` — the N=1 identity from T010 must hold
      by construction, not as a special case.
- [X] T016 [US1] Add the `Discover` subcommand definition to `gsm-sip-bridge/src/cli.rs`
      (`--out <path>`, `--shell-env`) per `contracts/discover-cli-contract.md`.
- [X] T017 [US1] Implement the subcommand handler in `gsm-sip-bridge/src/main.rs` (or a new
      `gsm-sip-bridge/src/vowifi/discovery.rs` function it calls): runs the shared scan, and when
      `[vowifi].enabled = false` writes/prints `LINE_COUNT=0`/empty exclusions and exits 0; when
      enabled, resolves `RoleAssignment` + `LineTable`, writes `LineResolution` JSON to `--out`
      (default `/tmp/gsm-sip-bridge-lines.json`, overridable via `GSM_SIP_BRIDGE_LINES_FILE`), and
      with `--shell-env` also prints the indexed bash-array format from the contract. Zero usable
      lines while enabled logs a prominent error but still exits 0 (the spec's clarification —
      degrade, don't fail the command).
- [X] T018 [US1] Wire `main.rs`'s circuit-switched daemon startup path (before `CardPool::new`)
      to read `LineResolution.circuit_switched_excluded_ports` from the `--out` JSON (env var
      `GSM_SIP_BRIDGE_LINES_FILE`, defaulting to the same path as T017; missing file = empty
      exclusion set, so a fleet that never ran `discover` — e.g. VoWiFi permanently disabled —
      behaves exactly as today) and pass it to `scan_modules`/`CardPool` so those ports are
      skipped (FR-007). Modify `modules/mod.rs`'s `CardPool::new`/its call site accordingly.

**Checkpoint**: `gsm-sip-bridge discover` runs standalone, logs every discovered modem/AT port/SIM
with no hand-typed device path, and the circuit-switched daemon still starts and serves
audio-capable cards unaffected (FR-021). This alone is a demoable MVP.

---

## Phase 4: User Story 2 - One VoWiFi Line Per SIM, Concurrently (Priority: P2)

**Goal**: Every discovered VoWiFi SIM gets its own tunnel, IMS registration, and inbound call
path, running concurrently and independently recoverable.

**Independent Test** (from spec.md): two VoWiFi SIMs on different carriers — two tunnels come up,
two IMS registrations succeed, each SIM's calls bridge, including two at once.

### Tests for User Story 2

- [X] T019 [P] [US2] Add `test_vowifi_lines.rs` cases proving two `ResolvedLine`s never share a
      derived resource: distinct `netns`, `strongswan_if_id`, `strongswan_tun_iface`,
      `veth_local_addr`/`veth_peer_addr`, `vpcd_port`, pcscd socket path, charon vici/log paths,
      `pcscf_source_path` (FR-011) — table-driven over line counts 1..=8.
- [X] T020 [P] [US2] Add a `gsm-sip-bridge/src/vowifi/mod.rs` unit test proving `RecentCalls`
      keyed per `card_id` (the `HashMap<String, RecentCalls>` replacing today's single instance)
      evicts and snapshots independently per key — extending the existing `RecentCalls` test
      module in that file.

### Implementation for User Story 2

- [X] T021 [US2] Add `--line <index>` to the `VowifiImsAgent` CLI variant in
      `gsm-sip-bridge/src/cli.rs`; in `main.rs`'s `handle_vowifi_ims_agent_command`, load
      `LineResolution.lines[index].config` (from the `--out` JSON, env `GSM_SIP_BRIDGE_LINES_FILE`)
      instead of `config.vowifi`, and pass that `&VowifiConfig` into `ims::agent::run` exactly as
      today — per `contracts/agent-topology-contract.md`, no change needed inside `ims/agent.rs`
      itself (confirm by reading it: it already takes `&VowifiConfig` with no singleton
      assumption).
- [X] T022 [US2] `vowifi/usim_bridge.rs` — no change needed: it already takes `--vpcd-port`, and
      the corrected PC/SC design (research.md item 4) is one shared `pcscd` exposing one vpcd
      reader with N **slots**, so each line's `vowifi-usim-bridge` just connects to its own slot's
      port (`LINE_VPCD_PORT[i]` = base+i). (The original per-line-pcscd `--pcsc-socket` idea was
      dropped: pcsc-lite has no runtime socket override, so N pcscd can't coexist; `eap-sim-pcsc`
      selects each SIM by IMSI across the shared reader's slots. Fix: `--enable-vpcdslots=8` in
      `docker/Dockerfile` + one shared pcscd in `docker/entrypoint.sh`.)
- [X] T023 [US2] Rework `gsm-sip-bridge/src/vowifi/mod.rs`'s `run_inner`/`handle_connection` for
      Agent B: read `LineResolution` to get `LINE_COUNT` and each line's `(veth_peer_addr,
      control_port, card_id)`; spawn one accept-loop thread per line (each binding its own
      `TcpListener`), sharing one `Endpoint`/`Account`/Discord client/store handle; replace the
      single `Arc<Mutex<RecentCalls>>` with `Arc<Mutex<HashMap<String, RecentCalls>>>` keyed by
      `card_id` (per T020); each thread's closure captures its own `card_id` and threads it through
      to every `tracing` call and into `forward_vowifi_sms`'s `module_id` argument (replacing the
      hardcoded `VOWIFI_SMS_MODULE_ID` constant for the multi-line case — keep it as the fallback
      label only if `LINE_COUNT` is unavailable/zero). No `ControlMessage` wire-format change
      (research.md item 6).
- [X] T024 [US2] Update `docker/entrypoint.sh`: call `gsm-sip-bridge discover --shell-env` once,
      up front — **before** starting the circuit-switched daemon supervisor loop (closing
      research.md item 3's race) — `eval`ing its `LINE_COUNT`/`LINE_*` arrays. Wrap the existing
      per-line block (today's single-shot `ensure_epdg_interface`, swanctl render, `pcscd`/
      `vowifi-usim-bridge`/`charon` start, tunnel-readiness wait, reliability supervisor,
      keepalive, veth-pair creation) in a `for i in $(seq 0 $((LINE_COUNT - 1)))` loop, indexing
      every path/port/PID variable by `$i` (e.g. `CHARON_PID_$i`, or a bash array
      `CHARON_PIDS[i]`) so `cleanup()` can still stop every instance. Start
      `vowifi-ims-agent --line $i` per iteration; start `vowifi-sip-agent` **once**, after the
      loop, once all lines' veth pairs exist. `LINE_COUNT=0` (VoWiFi enabled but no usable line)
      logs the spec's prominent error and skips the whole VoWiFi block while still letting the
      CS daemon supervisor (already started first) continue running — matching the clarification.
- [X] T025 [US2] Update `docker/entrypoint.sh`'s `cleanup()` trap to iterate and kill every
      per-line PID (charon, pcscd, usim-bridge supervisor, ims-agent supervisor) instead of the
      today's fixed singleton set; `ip netns del` every `ims{i}`, not just one.

**Checkpoint**: Two VoWiFi lines come up concurrently, register independently, and bridge calls
concurrently without cross-talk; a fault on one line doesn't affect the other (live-verified in
Phase 6). User Stories 1 and 2 both work together.

---

## Phase 5: User Story 3 - Per-Line Identification and Status (Priority: P3)

**Goal**: Every VoWiFi log line, metric, status report, and forwarded SMS identifies its card and
SIM; one status command reports every line.

**Independent Test** (from spec.md): with two lines active, `vowifi-status` lists both with their
own tunnel/registration state; an SMS on each SIM attributes to the correct card.

### Tests for User Story 3

- [X] T026 [P] [US3] Add a `gsm-sip-bridge/src/metrics/mod.rs` unit test asserting the new
      `vowifi_tunnel_up`/`vowifi_registration_state`/`vowifi_calls_total` gauge/counter vecs
      register without panicking and accept a `card_id` label (mirroring existing registration
      tests in that file, if any exist, or a minimal smoke test if not).
- [X] T027 [P] [US3] Add a `vowifi/mod.rs` test that `print_status` (or its refactored
      per-line equivalent) produces one block per line in `LineResolution`, each labeled with its
      `card_id`, and that one line's query failing doesn't suppress the other line's block
      (acceptance scenario 1).

### Implementation for User Story 3

- [X] T028 [US3] Add the `vowifi_tunnel_up`, `vowifi_registration_state`, `vowifi_calls_total`
      metric families to `gsm-sip-bridge/src/metrics/mod.rs` per
      `contracts/agent-topology-contract.md`, following the file's existing
      `register_gauge_vec!`/`register_counter_vec!` conventions.
- [X] T029 [US3] Have `ims::agent::run` (Agent A) update `vowifi_tunnel_up`/
      `vowifi_registration_state` labeled with its own `card_id` (passed in alongside its
      `--line`-selected `VowifiConfig`, T021) at the same points it already logs
      registration/tunnel state changes.
- [X] T030 [US3] Have Agent B's per-line accept-loop threads (T023) increment
      `vowifi_calls_total{card_id, outcome}` at the same points `handle_connection` already
      records a `RecentCalls` entry.
- [X] T031 [US3] Rework `vowifi::print_status`/the `vowifi-status` subcommand to iterate every
      line in `LineResolution` (reading the same `--out` JSON as T017/T021), querying each line's
      Agent A status port and Agent B per-line call history, printing each block labeled by
      `card_id`, and reporting overall failure only if every line's queries fail (FR-018,
      `contracts/agent-topology-contract.md`).
- [X] T032 [US3] Update the container health check (wherever FR-019 is currently implemented —
      locate via the health-check entry point referenced in feature 011/012's plan.md) to consider
      every line's status, not just the first, without failing the container when only some lines
      are down (spec's degrade clarification carried through to the health surface).

**Checkpoint**: All three user stories independently functional; status/logs/metrics/SMS all
attribute correctly per line.

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: Documentation, formatting, and the live verification the spec's Assumptions section
requires before calling this feature done.

- [X] T033 [P] Update `docker/entrypoint.sh`'s top-of-file doc comment and
      `specs/013-multi-card-vowifi/quickstart.md` cross-references to describe the multi-line
      startup sequence (discover-once-then-loop) in place of the old single-shot description.
- [X] T034 [P] Update `README.md`/deployment docs (wherever `[vowifi].modem_port`/single-SIM setup
      is documented today) to describe auto-discovery as the default path and
      `[[vowifi.line]]`/`max_lines` as the override surface.
- [X] T035 Run `cargo fmt --all && make lint && cargo test --workspace` and fix anything red —
      the mandatory pre-commit gate (CLAUDE.md).
- [ ] T036 Execute `specs/013-multi-card-vowifi/quickstart.md` steps 1–7 against real hardware
      (two VoWiFi-capable modems, two carrier SIMs) — operator-run, the same boundary every prior
      VoWiFi feature has drawn; record results against SC-001 through SC-008.

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies.
- **Foundational (Phase 2)**: Depends on Setup — BLOCKS every user story.
- **User Story 1 (Phase 3)**: Depends on Foundational only. Delivers a standalone MVP (discovery
  + `discover` CLI + CS exclusion wiring) even with zero VoWiFi lines ever actually started.
- **User Story 2 (Phase 4)**: Depends on Foundational + US1 (needs `LineTable`/`LineResolution`
  and the `discover` CLI to exist) — not independent of US1 in practice, though the spec frames it
  as a separate priority tier; sequence P1 → P2 as the spec's own priority order implies.
- **User Story 3 (Phase 5)**: Depends on US2 (per-line agent processes/threads must exist before
  their status/metrics can be reported).
- **Polish (Phase 6)**: Depends on all three user stories.

### Parallel Opportunities

- T004–T005 (Foundational) after T002–T003.
- T006–T011 (US1 tests) can all be written in parallel before T012–T018 implementation.
- T019–T020 (US2 tests) in parallel; likewise T026–T027 (US3 tests).
- T033–T034 (Polish docs) in parallel with each other, not with T035–T036 (sequential: format/
  lint/test must pass before the live proving run is meaningful).

---

## Implementation Strategy

### MVP First (User Story 1 Only)

Phases 1–3 deliver `gsm-sip-bridge discover` as a standalone, demoable capability: attach any mix
of VoWiFi-capable modems, run it with no config changes, see every modem/AT port/SIM reported and
the circuit-switched pool correctly excluding VoWiFi-claimed ports. This is independently valuable
even before any VoWiFi line actually starts running.

### Incremental Delivery

1. Setup + Foundational → config schema and shared types compile.
2. User Story 1 → discovery + `discover` CLI + CS exclusion → **MVP**, demoable standalone.
3. User Story 2 → per-line resource derivation + Agent A/B rework + entrypoint loop → two lines
   actually run concurrently.
4. User Story 3 → status/metrics/attribution polish → operationally usable at scale.
5. Polish → docs, gate, live proving against SC-001..SC-008.
