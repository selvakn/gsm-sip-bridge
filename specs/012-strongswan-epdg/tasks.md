# Tasks: strongSwan-Based ePDG Tunnel (Option 2)

**Input**: Design documents from `/specs/012-strongswan-epdg/`
**Prerequisites**: plan.md, spec.md (incl. Clarifications), research.md, data-model.md,
contracts/tunnel-engine-contract.md, contracts/vpcd-bridge-protocol.md, quickstart.md

**Tests**: Included throughout. The project constitution mandates Integration-First Testing
(NON-NEGOTIABLE) and TDD as the default practice, and `CLAUDE.md`'s pre-commit checklist
(`cargo fmt --all && make lint && cargo test --workspace`) applies to **every** task below —
none of that is relaxed for this feature. Rust test tasks precede their implementation tasks
and must fail first. Live-carrier tasks (marked **LIVE**) are operator-run per `quickstart.md`
and cannot be automated — same model as 011.

**Organization**: Tasks are grouped by user story (spec.md priorities). Note the one hard
inter-story dependency, unusual but structural: **US2 (SIM auth) must complete before US1
(longevity) can be exercised** — no tunnel exists to keep alive until EAP-AKA works. Both are
P1; US2 is sequenced first because it is the feature's critical unknown (spec US2 rationale).

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependency on an incomplete task)
- **[Story]**: US1–US4, per spec.md's prioritized user stories
- Exact file paths in every task description

## Path Conventions

Single Cargo workspace, existing layout — no new crate. Paths relative to the repo root,
primarily `gsm-sip-bridge/src/`, `docker/`, and `docker/strongswan/` (new config-template dir),
per plan.md's Project Structure section.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Scaffolding and vendored builds — new module/CLI stubs, the strongSwan-fork and
vpcd Docker build stages, and the config-template surface. Nothing here makes an acceptance
scenario pass on its own, but T005 is where the biggest non-code risk (fork-on-musl) gets
burned down, so it goes first, not last.

- [X] T001 Create `gsm-sip-bridge/src/vowifi/usim_bridge.rs` (empty module) and register
      `pub mod usim_bridge;` in `gsm-sip-bridge/src/vowifi/mod.rs`
- [X] T002 [P] Add `VowifiUsimBridge` (args: `--modem`, `--vpcd-host` default `127.0.0.1`,
      `--vpcd-port` default `35963`) and `VowifiImsi` (arg: `--modem`) variants to the
      `Commands` enum in `gsm-sip-bridge/src/cli.rs`, mirroring `ImsRegisterArgs`' style
- [X] T003 Wire both new variants into the pre-daemon dispatch in
      `gsm-sip-bridge/src/main.rs` (mirroring `Commands::ImsRegister`/`ImsCall` handling),
      initially calling stub handlers that log "not yet implemented" and exit non-zero
      (depends on T001, T002)
- [X] T004 [P] Create `docker/strongswan/` config templates per research.md items 3/4/9:
      `charon-logging.conf` (filelog → `/tmp/charon.log`, `ike = 1`, `cfg = 1`,
      `flush_line = yes`), `p-cscf.conf` (`load = yes`, `enable { ims = yes }`),
      `osmo-epdg.conf` (`load = no`), `charon-extra.conf` (`install_virtual_ip = no`,
      `retry_initiate_interval`), `swanctl-epdg.conf.template` (connection + child named `ims`,
      `if_id_in/out = 23`, `vips`, `remote_ts` both families, NAI placeholders `@IMSI@`
      `@MCC@` `@MNC@` `@EPDG_IP@` `@SRC_ADDR@`, `keyingtries = 0`, `dpd_delay = 30s`), and
      `ims.updown` handling **both** `up-client`/`down-client` and `-v6` variants
      (wiki's script only did v6 — research.md item 3 verify-note)
- [X] T005 [P] Add `strongswan-builder` stage to `docker/Dockerfile`: clone
      `https://gitea.osmocom.org/ims-volte-vowifi/strongswan-epdg.git` branch `jolly/work`,
      `autoreconf -if`, `./configure` with `--enable-eap-aka --enable-eap-sim
      --enable-eap-sim-pcsc --enable-p-cscf --enable-openssl` (+ minimal plugin set),
      `make install` into a staging tree. **Burn down research.md item 8's musl risk here**;
      if the fork trips on musl, carry a patch in `docker/patches/` (precedent: the SWu patch)
      or fall back to a Debian-built static stage — record the outcome in research.md
- [X] T006 [P] Add `vpcd-builder` stage to `docker/Dockerfile`: build vsmartcard's
      `virtualsmartcard` (autotools, `pcsc-lite-dev`), staging the vpcd IFD-handler `.so` and
      its `/etc/reader.conf.d/vpcd` snippet; pin a commit/tag for reproducibility
- [X] T007 Extend the runtime stage in `docker/Dockerfile`: add the `pcsc-lite` daemon
      package, COPY the strongSwan staging tree, vpcd driver + reader conf, and
      `docker/strongswan/` templates into place. Acceptance: image builds; `charon --version`
      and `swanctl --version` run; pcscd starts and lists the vpcd reader; the SWu/Python
      subsystem is byte-for-byte untouched (depends on T005, T006, T004)

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Pieces every subsequent story needs: the IMSI helper (US1's config rendering AND
US2's manual harness need it) and the `TUNNEL_ENGINE` switch skeleton so all later entrypoint
work lands inside a branch that leaves the default `swu` path untouched from day one.

**⚠️ CRITICAL**: No user story work can begin until this phase is complete.

- [X] T008 [P] Unit test for the `vowifi-imsi` handler in `gsm-sip-bridge/src/vowifi/mod.rs`
      (or a small `vowifi/imsi.rs`): scripted `AtCommander` transport (existing `MockStream`
      pattern in `gsm-sip-bridge/src/modules/at_commander.rs`, whose hardware-unavailable
      justification already stands) returns a canned `AT+CIMI` response; assert the handler
      yields exactly the IMSI string. Test must fail first
- [X] T009 Implement `vowifi-imsi`: read IMSI via `AtCommander::query_imsi()`, print to
      stdout (nothing else — the entrypoint consumes it verbatim), non-zero exit on failure;
      replace T003's stub (depends on T008)
- [X] T010 Add the `TUNNEL_ENGINE` skeleton to `docker/entrypoint.sh`: validate
      `swu`|`strongswan` (unknown value → fatal, per data-model.md), default `swu`; `swu`
      branch is the existing flow verbatim; `strongswan` branch initially logs "not yet
      implemented" and exits. Also document `TUNNEL_ENGINE` in `docker/epdg/.env`

**Checkpoint**: Foundation ready — user story implementation can begin.

---

## Phase 3: User Story 2 — SIM authentication via the modem-resident SIM (P1)

**Goal**: charon completes EAP-AKA against the SIM inside the EC200U, through
pcscd → vpcd → `vowifi-usim-bridge` → `AT+CSIM` (contracts/vpcd-bridge-protocol.md).

**Independent Test**: with only the modem attached (no PC/SC reader), a manually-initiated
swanctl connection reaches `EAP method EAP_AKA succeeded` on both test carriers (SC-004).

- [ ] T011 [P] [US2] Unit tests for the vpcd framing codec in
      `gsm-sip-bridge/src/vowifi/usim_bridge.rs` `#[cfg(test)]`: 2-byte big-endian
      length-prefix encode/decode (empty, 1-byte control, max-length APDU, short-read
      handling), control-message parsing (`0x00`/`0x01`/`0x02`/`0x04`), APDU-vs-control
      discrimination by length. Must fail first
- [ ] T012 [US2] Implement the framing codec + message types in
      `gsm-sip-bridge/src/vowifi/usim_bridge.rs` (depends on T011)
- [ ] T013 [P] [US2] Integration test (same file, `#[cfg(test)]`): real in-process TCP
      socket pair — test plays the vpcd side (connect-accept, power on → ATR request →
      command APDU → power off), bridge runs against a scripted modem transport; assert the
      full session transcript including canned-ATR reply and APDU round-trip. No transport
      mocks (constitution I). Must fail first
- [ ] T014 [US2] Implement the bridge session loop per data-model.md's state machine:
      vpcd connect/reconnect with backoff, power-on acquires the serial port
      (retry/backoff while busy — `serialport` opens with `TIOCEXCL`), power-off/disconnect
      releases it, reset re-runs the session prologue (SELECT MF, EF_DIR AID discovery via
      `modules/usim.rs::discover_usim_aid`), ATR request served from a canned USIM ATR
      constant (depends on T012, T013)
- [ ] T015 [P] [US2] Table-driven fixture tests for APDU normalization in
      `gsm-sip-bridge/src/vowifi/usim_bridge.rs` `#[cfg(test)]`, one case per documented
      EC200U/SIM quirk from `docker/patches/0001-ec200u-at-csim-fixes.patch`: (a) modem
      returns full data + `9000` while client drives classic `61xx`+GET RESPONSE — bridge
      caches and serves locally; (b) modem itself returns `61xx` — bridge chains GET RESPONSE
      against the modem; (c) SELECT `P2=0x00` → `6B00` → retried once with `P2=0x0C`;
      (d) SELECT of a foreign USIM AID (RID `A0…871002`) redirected to the discovered AID;
      (e) non-hex `+CSIM` fragment rejected. Must fail first
- [ ] T016 [US2] Implement APDU forwarding + normalization per
      contracts/vpcd-bridge-protocol.md: hex-encode into `AT+CSIM=<len>,"<hex>"`,
      slow-AUTHENTICATE wait discipline (no retransmit inside a pending transaction), the
      five normalizations from T015, verbatim pass-through for everything else (AUTHENTICATE
      INS `88` stays opaque, AUTS included). If `modules/usim.rs` needs a raw-APDU helper,
      add it without touching existing callers' behavior (depends on T014, T015)
- [ ] T017 [US2] Error mapping + tests in `gsm-sip-bridge/src/vowifi/usim_bridge.rs`: serial
      busy past the retry window → `SW=6F00` (card-mute) so charon fails the EAP round
      cleanly; modem `ERROR`/garbage → `SW=6F00` + raw exchange logged at warn; vpcd
      disconnect mid-session → port released, reconnect loop (depends on T016)
- [ ] T018 [US2] **LIVE** Trace `eap-sim-pcsc`'s real APDU sequence: run pcscd + vpcd +
      bridge (T007 image) with the fork's plugin initiating against a rendered swanctl conf;
      capture the bridge's APDU log. Confirm or refute research.md's verify-items — ATR
      treated as opaque, actual SELECT P2 values, AID selection behavior — and update T015's
      fixtures + normalization code to the observed ground truth (depends on T007, T016)
- [ ] T019 [US2] **LIVE** Manual auth harness per quickstart.md §1 steps 1–4 (hand-rendered
      swanctl conf using `vowifi-imsi` output): `EAP method EAP_AKA succeeded` +
      `CHILD_SA ims{1} established` on **Airtel (404/094)** and on **Vi (404/043)** —
      SC-004. Record per-carrier findings in `docs/vowifi-epdg-research-notes.md`
      (depends on T009, T017, T018)

**Checkpoint**: EAP-AKA works with no card reader — the feature's critical unknown is retired.
US2 acceptance scenarios 1 and 3 verifiable; scenario 2 (re-auth) lands with US1's soak.

---

## Phase 4: User Story 1 — Tunnel survives unattended long-running operation (P1)

**Goal**: the `strongswan` entrypoint branch establishes the tunnel into a persistent
netns/XFRM interface, publishes P-CSCF, and keeps the tunnel alive through rekeys, re-auth,
and outages (tunnel-engine contract obligations 1–8).

**Independent Test**: tunnel up via `TUNNEL_ENGINE=strongswan`, survives one carrier rekey
cycle and one forced outage with namespace/agents untouched (quickstart.md §3–4).

- [ ] T020 [P] [US1] Implement idempotent netns/interface plumbing in the `strongswan`
      branch of `docker/entrypoint.sh` (FR-011): create netns `$NETNS` if absent, `ip link
      add tun23 type xfrm if_id 23` (skip/replace if present), move into netns, `lo` +
      interface up, default routes both families via the interface,
      `disable_policy=1` sysctl on it (research.md item 3); absorb leftover state from a
      previous run
- [ ] T021 [US1] Implement engine startup in the same branch: resolve ePDG (reuse the
      existing `EPDG_IP`/`dig` block), render `/etc/swanctl/conf.d/epdg.conf` from
      `docker/strongswan/swanctl-epdg.conf.template` (IMSI via `vowifi-imsi`, `IMSI` env
      override honored, MCC/MNC zero-padded), start pcscd, supervise `vowifi-usim-bridge`
      (restart-on-exit like the agents), start charon, `swanctl --load-all`, `swanctl
      --initiate --child ims` (depends on T010, T020, and Phase 3)
- [ ] T022 [US1] Implement readiness + P-CSCF publication per the tunnel-engine contract:
      watch `/tmp/charon.log` for `CHILD_SA` establishment (backstop: poll `swanctl
      --list-sas`), extract `received P-CSCF server IP` lines, prefer IPv4, write
      `/tmp/pcscf` (never partial/empty — data-model.md validation), and refresh it on every
      re-establishment (depends on T021)
- [ ] T023 [US1] Reliability supervision: re-initiate loop if the `ims` CHILD_SA disappears
      (`swanctl --list-sas` poll), confirm `keyingtries = 0` + `retry_initiate_interval` +
      `dpd_delay = 30s` land from T004's templates, keep the existing TCP keepalive to
      `$PCSCF:5060` running unchanged (FR-012). Container logs must show
      connecting/established/rekeyed/reauth/disconnected/retrying transitions (FR-010)
      (depends on T022)
- [ ] T024 [US1] **LIVE** Forced-outage drill per quickstart.md §3: ≤60 s WAN interruption →
      back in service ≤90 s, netns + veth + agent PIDs untouched throughout (SC-002, US1
      acceptance scenario 2) (depends on T023; agents present via Phase 5's T026 or run
      without agents and verify netns/iface only, then re-verify after Phase 5)
- [ ] T025 [US1] **LIVE** 24 h rekey soak per quickstart.md §4: ≥1 carrier-scheduled rekey
      (and any re-auth — US2 acceptance scenario 2 closes here), zero tunnel-attributable
      agent restarts, `/tmp/pcscf` still valid at the end (SC-001, US1 acceptance
      scenarios 1/3) (depends on T024)

**Checkpoint**: the strongSwan tunnel is production-trustworthy standalone.

---

## Phase 5: User Story 3 — Existing bridge agents work unchanged (P2)

**Goal**: the proven 011 pipeline runs on top of the new engine with zero agent changes.

**Independent Test**: inbound VoWiFi call bridges end-to-end over a strongSwan tunnel
(quickstart.md §1 step 6 + §5).

- [ ] T026 [US3] Wire the existing shared tail (veth pair creation, both agent supervisors,
      keepalive) to run after the `strongswan` branch's readiness signal in
      `docker/entrypoint.sh`, exactly as it runs after the `swu` branch's; keep the veth
      half-pair rebuild check (still needed while `swu` is selectable — tunnel-engine
      contract obligation 5). Acceptance: `git diff` shows zero changes under
      `gsm-sip-bridge/src/ims/agent.rs`, `gsm-sip-bridge/src/vowifi/mod.rs` (agent code
      paths), and `gsm-sip-bridge/src/config/mod.rs` for this task (FR-007)
      (depends on T022)
- [ ] T027 [US3] **LIVE** End-to-end inbound call on Airtel over the strongSwan tunnel per
      quickstart.md §5: agents register (US3 acceptance scenario 1), inbound call answered
      and bridged ≤5 s with two-way audio (scenario 2); repeat once ≥12 h after startup
      (SC-003) — can piggyback on T025's soak window (depends on T025, T026)

**Checkpoint**: full 011 behavior reproduced on the new engine.

---

## Phase 6: User Story 4 — Deploy-time fallback to the old dialer (P3)

**Goal**: engine selection is a config-only switch and the legacy path is regression-clean.

**Independent Test**: same image runs both engines successfully, switched only by env
(quickstart.md §6).

- [ ] T028 [US4] Finalize selection docs: `TUNNEL_ENGINE` semantics + default in
      `docker/epdg/.env` and a comment block in `docker/docker-compose.yml`; default remains
      `swu` until SC-001..004 are recorded as passed (US4 acceptance scenario 3)
- [ ] T029 [US4] Equivalence review of the `swu` path: diff `docker/entrypoint.sh`'s
      `TUNNEL_ENGINE=swu` flow against the pre-feature script (git history) — behavior
      byte-for-byte equivalent apart from the branch plumbing; then **LIVE** spot check: one
      container start with `TUNNEL_ENGINE=swu` reaches tunnel-up + agents (SC-006, US4
      acceptance scenario 1) (depends on T010, T026)
- [ ] T030 [US4] **LIVE** Engine-switch drill per quickstart.md §6: `strongswan` → `swu` →
      `strongswan` on the same image, env-only changes, tunnel + agents return each time
      (SC-005, US4 acceptance scenario 2) (depends on T029)

**Checkpoint**: safe rollback exists; proving period can run in production.

---

## Phase 7: Polish & Cross-Cutting Concerns

- [ ] T031 [P] Documentation: add the strongSwan-engine section to
      `docs/vowifi-epdg-research-notes.md` (or a new `docs/vowifi-strongswan.md` linked from
      it): architecture chain, per-carrier live findings from T018/T019/T025, and the
      Option 1 → Option 2 migration status; update `specs/012-strongswan-epdg/quickstart.md`
      with actually-observed log lines where the plan's expectations differed
- [ ] T032 [P] FR-013 audit: sweep the feature's diff for newly hardcoded single-line
      assumptions — netns name, `if_id`, vpcd host/port, modem port, `/tmp/pcscf`,
      `/tmp/charon.log` must all be parametrized (env vars / CLI flags) with current
      defaults; fix any stragglers
- [ ] T033 Final gate: `cargo fmt --all && make lint && cargo test --workspace` clean on the
      full branch; `tools/count-unsafe.sh` still reports 0 unsafe blocks in
      `gsm-sip-bridge/src`; confirm every spec Success Criterion (SC-001..006) has a recorded
      pass/fail + evidence pointer, and flag the Option-1-removal follow-up feature if all
      passed

---

## Dependencies

```text
Phase 1 (Setup) ──► Phase 2 (Foundational) ──► Phase 3 (US2, P1: SIM auth)
                                                      │
                                                      ▼
                                          Phase 4 (US1, P1: longevity)
                                                      │
                                                      ▼
                                          Phase 5 (US3, P2: agents unchanged)
                                                      │
                                                      ▼
                                          Phase 6 (US4, P3: fallback proving)
                                                      │
                                                      ▼
                                          Phase 7 (Polish)
```

- US2 → US1 is a **hard** dependency (no tunnel without auth); US1 → US3 likewise (agents
  need a tunnel). US4's implementation is mostly already in place via T010; its phase is
  validation + docs, so it could run any time after Phase 5 — sequenced last because its
  live drills are cheapest when everything else is stable.
- Within phases, tests precede implementations (TDD); tasks without [P] depend on the
  previous task in their phase unless an explicit `(depends on …)` says otherwise.

## Parallel Execution Examples

- **Phase 1**: T002 ∥ T004 ∥ T005 ∥ T006 (four different files; T001→T003 chain runs
  alongside; T007 joins after T004/T005/T006).
- **Phase 3**: T011 ∥ T013 ∥ T015 (all tests, same file but independent cases — write
  together, land as one failing-tests commit if preferred) while T005's image work from
  Phase 1 is still soaking; T018 and T019 are sequential LIVE sessions.
- **Phase 4**: T020 ∥ (T021 prep: template review) — thereafter sequential; the 24 h soak
  (T025) is wall-clock time during which Phase 6's T028/T029 doc/review work can proceed.
- **Phase 7**: T031 ∥ T032.

## Implementation Strategy

- **MVP** = Phases 1–4 (US2 + US1): a strongSwan tunnel that authenticates with the modem's
  SIM and stays up unattended. That alone retires the feature's motivation; US3 then proves
  the payload path, US4 formalizes rollback.
- Deliver incrementally: each checkpoint is a committable, demonstrable state; every commit
  passes the full pre-commit gate (constitution II/III).
- LIVE tasks gate promotion, not merging: code merges when its automated tests pass; the
  `TUNNEL_ENGINE=swu` default means merged-but-unproven strongSwan code ships dark until
  T030/T033 flip the recommendation.

## Format Validation

All 33 tasks: checkbox ✓, sequential T001–T033 ✓, [P] only where file/dependency-safe ✓,
story labels on all Phase 3–6 tasks and none elsewhere ✓, explicit file paths in every
implementation task ✓.
