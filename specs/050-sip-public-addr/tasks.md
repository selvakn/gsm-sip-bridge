---
description: "Task list for advertising a configured public address in SIP/SDP"
---

# Tasks: Advertise a configured public address in SIP/SDP

**Input**: Design documents from `/specs/050-sip-public-addr/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/sip-public-addr-contract.md, quickstart.md

**Tests**: Included. The project constitution makes Integration-First Testing
non-negotiable and TDD the default practice — every implementation task
below has a corresponding test task that must be written and failing first.

**Organization**: Tasks are grouped by user story (spec.md: US1/US3 are P1,
US2 is P2) to enable independent implementation and testing of each.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies on an
  incomplete task)
- **[Story]**: Which user story this task belongs to (US1/US2/US3)
- File paths are exact and relative to the repository root

## Path Conventions

Existing Cargo workspace, no new members: `pjsua-safe/{src,tests}/` (PJSIP
wrapper) and `gsm-sip-bridge/src/{config,vowifi,sip}/` (application/config).

---

## Phase 1: Setup

**Purpose**: Establish a clean, green baseline before touching anything.

- [X] T001 On branch `050-sip-public-addr`, run `make format && make lint && make test` from the repository root and confirm a clean, green baseline (Green-on-Commit, constitution) before any code change in this feature

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: The new field's types must exist, end to end, before any user
story's behavior can be implemented or tested against them.

**⚠️ CRITICAL**: No user story work can begin until this phase is complete.

- [X] T002 [P] Add `public_addr: Option<String>` to `RawSip` in `gsm-sip-bridge/src/config/raw.rs` (inside the `section!` macro invocation, alongside `display_name: Option<String>`), and set it to `None` in `RawSip`'s `Default` impl
- [X] T003 [P] Add `public_addr: Option<std::net::IpAddr>` to `SipConfig` in `gsm-sip-bridge/src/config/mod.rs` (alongside the existing `tls_verify` field)
- [X] T004 [P] Add `public_addr: Option<std::net::IpAddr>` to `EndpointConfig` in `pjsua-safe/src/endpoint.rs` (alongside `tls_verify`), with a doc comment noting it is applied to the SIP transport's `tp_cfg.public_addr` when present
- [X] T005 Update every existing `EndpointConfig` struct literal to set the new `public_addr` field (use `None` unless a task below says otherwise): `pjsua-safe/tests/smoke.rs`, `pjsua-safe/tests/two_call_bridge.rs`, `pjsua-safe/tests/sdp_offer.rs`, `pjsua-safe/tests/codec_priority.rs`, `gsm-sip-bridge/src/vowifi/mod.rs` (the `ep_config`/`EndpointConfig { ... }` construction near line 271), `gsm-sip-bridge/src/sip/mod.rs` (`endpoint_config()` near line 275) — this is mechanical and compile-enforced (depends on: T004)
- [X] T006 In `pjsua-safe/src/endpoint.rs`, store the resolved `public_addr` on `Endpoint` at construction time (`Endpoint::create`) and add an accessor `Account::register`/`Account::local` can call to read it (depends on: T004)

**Checkpoint**: Workspace compiles with `public_addr` threaded through every
type, but nothing yet applies it to PJSIP behavior. `make test` is green.

---

## Phase 3: User Story 1 - A remote SIP client gets working audio over a VPN (Priority: P1) 🎯 MVP

**Goal**: When `[sip].public_addr` is configured, the bridge advertises that
address in SDP and SIP signaling on both outbound and inbound calls, so a
SIP client reachable only over a routed/VPN network gets two-way audio.

**Independent Test**: Configure `[sip].public_addr`, place an outbound call
and receive an inbound call through the bridge's PJSIP transport, and
confirm the SDP `c=` line and Contact/Via headers on both carry the
configured address (per `contracts/sip-public-addr-contract.md`); confirm a
local/LAN call is unaffected.

### Tests for User Story 1 ⚠️

> Write these first; they must fail before the implementation tasks below.

- [X] T007 [P] [US1] In `pjsua-safe/tests/sdp_offer.rs` (or a new `pjsua-safe/tests/public_addr.rs` following that file's raw-UDP-capture pattern), add a `#[cfg(feature = "pjsip-linked")]` test that creates an `Endpoint` with `public_addr` set, places a call via an `Account` created with `Account::local`, captures the raw INVITE off the wire, and asserts the SDP `c=IN IP4 <addr>` line and the `Contact` header both carry the configured address
- [X] T008 [P] [US1] In `gsm-sip-bridge/src/config/mod.rs`'s test module, add cases: `[sip].public_addr` absent → `SipConfig.public_addr == None`; set to a valid IP literal (e.g. `100.111.26.23`) → parses to that `IpAddr` with no error

### Implementation for User Story 1

- [X] T009 [US1] In `gsm-sip-bridge/src/config/build.rs`'s `build_sip`, parse `raw.public_addr`: `None` stays `None`; a value that parses via `str::parse::<std::net::IpAddr>()` is used directly (no DNS); otherwise resolve via `(value.as_str(), 0).to_socket_addrs()` and take the first result — assign to `SipConfig.public_addr` (depends on: T002, T003)
- [X] T010 [US1] In `pjsua-safe/src/endpoint.rs`'s `Endpoint::create`, when `config.public_addr` is `Some`, apply it to `tp_cfg.public_addr` before `pjsua_transport_create` (near the existing `tp_cfg.port = config.local_port as u32` line) (depends on: T006)
- [X] T011 [US1] In `pjsua-safe/src/account.rs`, add a cached `public_addr: Option<std::net::IpAddr>` field to `Account`; in `Account::register` and `Account::local`, read it from the `&Endpoint` parameter (drop the `_` prefix — both currently ignore it) and apply it to `acc_cfg.rtp_cfg.public_addr` before `pjsua_acc_add` (depends on: T006, T010)
- [X] T012 [US1] Wire `config.sip.public_addr` into the `EndpointConfig` constructed at both call sites: `gsm-sip-bridge/src/vowifi/mod.rs` (~line 271, Agent B) and `gsm-sip-bridge/src/sip/mod.rs` (`endpoint_config()`, ~line 275) (depends on: T009, T010)

**Checkpoint**: User Story 1 is independently functional — T007/T008 pass;
`quickstart.md` steps 1-4 and 6 are executable against real hardware.

---

## Phase 4: User Story 2 - The setting lives in the bridge's normal config, not an ad hoc env var (Priority: P2)

**Goal**: `[sip].public_addr` is a first-class, documented `[sip]` field
with the same fail-fast validation discipline as every other SIP setting —
no environment variable involved.

**Independent Test**: Set the field in a config file and confirm it's
picked up with no environment variable; set a malformed value and confirm
the bridge refuses to start with a clear, specific error.

### Tests for User Story 2 ⚠️

- [X] T013 [P] [US2] In `gsm-sip-bridge/src/config/mod.rs`'s test module, add a case: `[sip].public_addr` set to a syntactically invalid value (e.g. empty string, or a string `to_socket_addrs()` rejects outright) → `try_parse` returns `Err(BridgeError::Config(..))` whose message names `sip.public_addr`
- [X] T014 [P] [US2] In the same test module, add a case for a well-formed hostname that fails to resolve (e.g. a reserved/invalid TLD such as `not-a-real-host.invalid`) → same error class as T013; if sandbox DNS behavior makes this unreliable in CI, note that in a test comment and keep the assertion narrow (per `research.md` Decision 5)

### Implementation for User Story 2

- [X] T015 [US2] Extend `build_sip`'s `public_addr` handling (T009) so a resolution failure or unparseable value returns `BridgeError::Config` in the same message style as the existing `sip.transport`/`sip.tls_verify` checks in `gsm-sip-bridge/src/config/build.rs`, naming `sip.public_addr` and the offending value (depends on: T009)
- [X] T016 [P] [US2] Add a `public_addr` row to the `[sip]` table in `docs/configuration.md` (key `public_addr`, type `string`, default *(unset)*, description covering IP-literal-or-hostname and the Tailscale/VPN use case), and add it to one of the existing `## Examples` TOML snippets

**Checkpoint**: User Stories 1 AND 2 both work independently — T007, T008,
T013, T014 all pass; `quickstart.md` step 7 is executable.

---

## Phase 5: User Story 3 - Inbound calls keep working audio after caller-ID rewrite (Priority: P1)

**Goal**: `Account::set_identity` — which rebuilds the account config from
PJSIP defaults on every inbound SIP-server-mode call — no longer silently
drops the configured public address.

**Independent Test**: Configure `public_addr`, construct an `Account`, call
`set_identity`, and confirm the address is still applied to
`acc_cfg.rtp_cfg.public_addr` afterward — not just at initial construction.

### Tests for User Story 3 ⚠️

- [X] T017 [P] [US3] Extend the T007 test (or add a new `pjsip-linked` test in the same file) to also call `Account::set_identity` after account creation and re-capture/re-verify a subsequent call's SDP/Contact still carries the configured `public_addr` — this must fail against the pre-T018 code (today, `set_identity` resets it)

### Implementation for User Story 3

- [X] T018 [US3] In `pjsua-safe/src/account.rs`'s `#[cfg(feature = "pjsip-linked")]` `Account::set_identity`, re-apply the cached `self.public_addr` to `acc_cfg.rtp_cfg.public_addr` in the same `acc_cfg` rebuild that already sets `acc_cfg.id`, before calling `pjsua_acc_modify` (depends on: T011)

**Checkpoint**: All three user stories are independently functional; T017
passes; `quickstart.md` step 5 is executable.

---

## Phase 6: Polish & Cross-Cutting Concerns

- [X] T019 [P] Add a `### Fixed` entry under `## [Unreleased]` in `CHANGELOG.md` describing the bug (silent audio over VPN/Tailscale, root cause `pj_gethostip()`'s default-route fallback) and the fix (`[sip].public_addr`), referencing GitHub issue #77
- [~] T020 Run `specs/050-sip-public-addr/quickstart.md`'s manual verification steps against real hardware where available (Tailscale-reachable softphone); record which steps were exercised live vs. only via the automated suite — **not run**: this session has no Tailscale/real-hardware access; deferred to the user per `quickstart.md`'s manual section
- [~] T021 Run the full pre-commit checklist from the repository root: `make format`, `make lint` (workspace-wide, including test targets), `make test` — plus `cargo test -p pjsua-safe --features pjsip-linked` explicitly, since the default `make test` may not enable that feature — all must be green before commit — **`make format`/`make lint`/`make test` all green**; the `--features pjsip-linked` run could not execute in this sandbox (no `libpjproject` via pkg-config — confirmed via `pjsua-sys/build.rs`'s own fallback path) — needs to run on the host/build environment that has real PJSIP linked, per this project's established convention (`memory: pjsip-linked build runs on host directly`)

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies — start immediately.
- **Foundational (Phase 2)**: Depends on Setup. Blocks every user story —
  the config/`EndpointConfig` types must exist before any story's tests or
  implementation can reference them.
- **User Story 1 (Phase 3)**: Depends on Foundational only. This is the MVP
  — it alone closes the core bug (GitHub issue #77's reported symptom).
- **User Story 2 (Phase 4)**: Depends on Foundational. Its implementation
  task (T015) extends T009 (from US1), so in practice build US1 first — but
  US2's own tests (T013/T014) and doc task (T016) are independently
  meaningful and independently testable once T009 exists.
- **User Story 3 (Phase 5)**: Depends on Foundational and on T011 (US1's
  `Account.public_addr` field and its use in `register`/`local`) — T018
  re-applies a field T011 introduces. Independently testable once T011
  exists, without needing US2's validation work.
- **Polish (Phase 6)**: Depends on all three user stories being complete.

### Within Each User Story

- Tests (T007/T008, T013/T014, T017) are written first and must fail before
  their corresponding implementation tasks.
- US1's implementation order is: config parsing (T009) → transport-level
  apply (T010) → account-level apply (T011) → wiring into the two real
  call sites (T012) — each depends on the one before it.

### Parallel Opportunities

- T002, T003, T004 (Phase 2) touch three different files in two different
  crates — run in parallel.
- T007 and T008 (Phase 3 tests) touch different files — run in parallel.
- T013, T014, and T016 (Phase 4) touch independent files — run in parallel.
- Phase 4 (US2) and Phase 5 (US3) can be staffed in parallel once Phase 3's
  T011 lands, since T015/T016 and T017/T018 touch disjoint concerns (config
  validation vs. `set_identity`) even though T018 shares a file
  (`pjsua-safe/src/account.rs`) with T011 — sequence T018 after T011 lands,
  not after all of US2.

---

## Parallel Example: Phase 2 (Foundational)

```bash
Task: "Add public_addr: Option<String> to RawSip in gsm-sip-bridge/src/config/raw.rs"
Task: "Add public_addr: Option<std::net::IpAddr> to SipConfig in gsm-sip-bridge/src/config/mod.rs"
Task: "Add public_addr: Option<std::net::IpAddr> to EndpointConfig in pjsua-safe/src/endpoint.rs"
```

## Parallel Example: User Story 1 tests

```bash
Task: "pjsip-linked SDP/Contact capture test in pjsua-safe/tests/sdp_offer.rs"
Task: "Config parse tests for public_addr in gsm-sip-bridge/src/config/mod.rs"
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Phase 1: Setup (T001).
2. Phase 2: Foundational (T002-T006) — blocking.
3. Phase 3: User Story 1 (T007-T012).
4. **STOP and VALIDATE**: run T007/T008, and `quickstart.md` steps 1-4/6
   against real hardware if available. This alone closes GitHub issue #77's
   reported symptom.

### Incremental Delivery

1. Setup + Foundational → foundation ready, nothing user-visible yet.
2. User Story 1 → the core fix; deployable/demoable on its own.
3. User Story 2 → validation/documentation hardening around the same field;
   no behavior change to US1's fix, just safety and discoverability.
4. User Story 3 → closes the specific regression class the issue's
   root-cause analysis flagged as the sharp edge (`set_identity`); without
   it, US1 works for outbound calls and simple inbound calls, but a
   SIP-server-mode inbound call with caller-ID rewrite would still lose
   audio.
5. Polish → changelog, docs, full pre-commit gate.

---

## Notes

- [P] tasks touch different files with no unfinished dependency between them.
- Every implementation task cites the exact existing line/function it
  modifies, taken from the current tree (verified during `/speckit-plan`'s
  research phase) — re-verify line numbers haven't drifted if this feature
  is picked up much later.
- T005's fixture sweep is mechanical and compiler-enforced: the build simply
  will not succeed until every `EndpointConfig` literal is updated.
- Commit after each task or logical group, per the constitution's Frequent
  Atomic Commits principle — natural commit boundaries are: Foundational
  (T002-T006), US1 (T007-T012), US2 (T013-T016), US3 (T017-T018), Polish
  (T019-T021).
