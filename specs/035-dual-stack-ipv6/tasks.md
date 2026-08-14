---
description: "Task list for Dual-Stack IPv6 for the Cellular-Internet Sidecar"
---

# Tasks: Dual-Stack IPv6 for the Cellular-Internet Sidecar

> **REVISED after hardware testing (2026-08-13).** The tasks below were completed
> against the original *two-session* design (separate `ip-type=6` alongside
> `ip-type=4`). On-hardware testing on Jio showed that design cannot work — a second
> session to the same APN is refused (`multiple-connection-to-same-pdn-not-allowed`)
> and there is no `ip-type=8` — so dual-stack was reworked into a **single IPv4v6
> bearer** (provision the profile `pdp-type=IPv4v6`, dial by `profile-index`, read
> both families from one session). The task text below is kept as a historical
> record; the current design is in research.md R1, data-model.md, and spec.md FR-001.

**Input**: Design documents from `/specs/035-dual-stack-ipv6/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/

**Tests**: INCLUDED — the constitution mandates integration-first TDD, and the plan
calls for extending the real sidecar test harness (scripted `qmicli`/`ip` stubs;
the modem is the only mock). Write the test tasks first and confirm they FAIL
before the matching implementation.

**Organization**: Grouped by user story (US1–US4) in priority order. All logic lives
in two sourced shell files (`internet-entrypoint.sh`, `internet-lib.sh`), so most
implementation tasks are sequential (same files); test files and docs are parallel.

## Path Conventions

Single-project shell sidecar. All paths are repo-relative under
`docker/cellular-internet/`.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Wire new config and test scaffolding before behavior changes.

- [X] T001 [P] Register the two new test scripts (`ipv6_lifecycle_test.sh`, `ipv6_hook_test.sh`) in the `test-scripts` target in `Makefile` (the existing `shellcheck -x docker/cellular-internet/tests/*.sh` glob already covers them for `lint`).
- [X] T002 Add the new configuration variables with defaults to the config block in `docker/cellular-internet/internet-entrypoint.sh`: `INTERNET_ENABLE_IPV6` (default `1`), `INTERNET_IPV6_HOOK` (default empty), `INTERNET_IPV6_HOOK_TIMEOUT` (default `10s`), `INTERNET_IPV6_RETRY_MAX` (default `5m`).

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Shared helpers used by multiple stories. MUST complete before US1–US4.

**⚠️ CRITICAL**: No user story work can begin until this phase is complete.

- [X] T003 Add an `is_global_v6()` POSIX helper to `docker/cellular-internet/internet-lib.sh` that returns 0 only for a global unicast IPv6 address (rejects `fe80::/10` link-local, `::1`, and `fc00::/7` ULA) via prefix string checks. Used by status (US1) and the hook (US4).
- [X] T004 Extend `write_status()` in `docker/cellular-internet/internet-lib.sh` to persist and merge the new fields `ipv6`, `ipv6_prefix`, `ipv6_state`, `ipv6_since` (honoring `STATUS_IPV6`/`STATUS_IPV6_PREFIX`/`STATUS_IPV6_STATE`/`STATUS_IPV6_SINCE` exported overrides, mirroring the existing `STATUS_IPV4` pattern), per `contracts/status-file.md`. `state` MUST stay derived from IPv4 only.

**Checkpoint**: Shared v6 helpers + status schema ready.

---

## Phase 3: User Story 1 - Inbound reach-back over IPv6 (Priority: P1) 🎯 MVP

**Goal**: Bring up a global IPv6 address + default route on the WWAN interface from
the QMI-granted settings, and record it in the status file, so the CGNAT'd host is
reachable inbound over IPv6.

**Independent Test**: With the fake modem granting an IPv6 address, `dial` applies a
`scope global` v6 address and an IPv6 default route to the iface and writes
`ipv6=<addr>` / `ipv6_state=up` to the status file.

### Tests for User Story 1 ⚠️ (write first, ensure they FAIL)

- [X] T005 [P] [US1] Create `docker/cellular-internet/tests/ipv6_lifecycle_test.sh` following the `wds_lifecycle_test.sh` pattern (scripted `qmicli`/`ip` stubs on PATH, `INTERNET_NO_MAIN=1` sourcing). Assert: a dual-stack dial parses the granted IPv6 address/prefix/gateway, issues `ip -6 addr add <addr>/<prefix>` and `ip -6 route replace default`, and writes `ipv6`/`ipv6_prefix`/`ipv6_state=up`/`ipv6_since` to the status file. Have the fake `ip` record its argv so v6 vs v4 calls can be distinguished.

### Implementation for User Story 1

- [X] T006 [US1] Add `apply_settings_v6()` to `docker/cellular-internet/internet-entrypoint.sh`: parse `IPv6 address:` (addr/prefix token), `IPv6 gateway address:`, and `IPv6 primary DNS:` from `qmi_wds --wds-get-current-settings`; validate global via `is_global_v6`; `ip -6 addr add "<addr>/<prefix>" dev "$iface"`; `ip -6 route replace default via "<gw>" dev "$iface"` (or on-link when no gw); echo the applied global address. Return nonzero (no side effects) when no global v6 is granted.
- [X] T007 [US1] Extend `setup_raw_ip()` (or add a small helper) in `docker/cellular-internet/internet-entrypoint.sh` to set `net.ipv6.conf.$iface.disable_ipv6=0` and flush any prior sidecar-added `scope global` v6 (`ip -6 addr flush dev "$iface" scope global`) before applying, so a changed prefix leaves no stale address.
- [X] T008 [US1] Modify `dial()` in `docker/cellular-internet/internet-entrypoint.sh` to bring up dual-stack when `INTERNET_ENABLE_IPV6=1`: keep the existing `ip-type=4` session and start a **separate** `ip-type=6` session (no combined `ip-type=8` — see research.md R1), capturing `V6_PKT_HANDLE`/`V6_WDS_CID`/`V6_MODE` with the same retained-CID discipline as v4 (`adopted` on `NoEffect`). After the v4 apply, call `apply_settings_v6`; on success set `V6_ADDR`/`V6_PREFIX`, write `ipv6`/`ipv6_state=up` status, and log `internet v6 up`. The v4 path and its status write remain exactly as today.

**Checkpoint**: US1 functional — a granted global v6 address is applied, routed, and observable.

---

## Phase 4: User Story 2 - Dual-stack: VoWiFi & IPv4-only keep working (Priority: P1)

**Goal**: Prove and preserve that the IPv4 path (address, route, DNS, VoWiFi, AT
port) is byte-for-byte unchanged when v6 is layered on or when v6 is disabled.

**Independent Test**: With `INTERNET_ENABLE_IPV6=0`, dial/teardown behavior is
identical to feature 032; with it on and the modem granting v4+v6, the v4 handle/cid
capture and teardown are unchanged.

### Tests for User Story 2 ⚠️

- [X] T009 [P] [US2] Extend `docker/cellular-internet/tests/wds_lifecycle_test.sh`: add IPv6 settings lines to the fake `qmicli --wds-get-current-settings` output and assert the existing v4 assertions (handle=`2264216040`, cid=`7`, teardown clears identity) still hold under dual-stack; add a case asserting `INTERNET_ENABLE_IPV6=0` performs no `ip -6` calls and leaves the v6 status fields empty.

### Implementation for User Story 2

- [X] T010 [US2] Guard all v6 logic in `docker/cellular-internet/internet-entrypoint.sh` behind `INTERNET_ENABLE_IPV6=1` and keep it in dedicated functions that receive only the iface and touch only `V6_*` vars + `ip -6`, so the v4 dial/apply/teardown code paths are unmodified in the disabled case (satisfies FR-003/FR-011). Confirm the AT port is never opened by any new code (QMI/`ip` only).

**Checkpoint**: US1 + US2 — v6 works and v4/VoWiFi is provably unchanged.

---

## Phase 5: User Story 3 - IPv6 best-effort, never blocks the bridge (Priority: P1)

**Goal**: Fold v6 (re)establishment into the existing supervise loop as a
non-gating, capped-backoff concern that never tears down/interrupts v4, never exits,
and never affects the healthcheck.

**Independent Test**: v6 ungranted ⇒ `ipv6_state=unavailable`, container stays
IPv4-healthy; v6 drops while v4 up ⇒ stale v6 flushed, backoff schedules a retry, v4
session untouched; healthcheck exit code independent of `ipv6_state`.

### Tests for User Story 3 ⚠️

- [X] T011 [P] [US3] Extend `docker/cellular-internet/tests/ipv6_lifecycle_test.sh`: (a) modem grants v4 but no v6 ⇒ `ipv6_state=unavailable`, v4 status/health unaffected; (b) a v6 drop flushes the `scope global` v6 and does NOT touch the v4 address/handle/cid; (c) the backoff gate defers the next attempt (assert `V6_NEXT_RETRY`/`V6_RETRY_INTERVAL` grow and reset on success); (d) source `internet-healthcheck.sh` logic and assert its exit code never changes with `ipv6_state`.

### Implementation for User Story 3

- [X] T012 [US3] Add a `v6_teardown_cleanup()` to `docker/cellular-internet/internet-entrypoint.sh`: flush the sidecar-added `scope global` v6 address + default route for the iface, and in `dual-session` mode stop the v6 WDS client using the same retained-CID discipline as `teardown()` (never clear `V6_WDS_CID` after a failed stop; drop it when the modem is gone/unreachable). MUST NOT touch `PKT_HANDLE`/`WDS_CID` or the v4 address. Invoke it on redial and from the TERM/INT trap alongside the v4 teardown.
- [X] T013 [US3] Add the best-effort v6 supervisor step into the `main()` loop in `docker/cellular-internet/internet-entrypoint.sh`, AFTER the unchanged v4 probe/redial logic: if no global v6 is currently applied and the monotonic `V6_NEXT_RETRY` deadline has passed, attempt `apply_settings_v6` (re-dialing a v6 session if needed), update status, and on failure set `V6_RETRY_INTERVAL = min(2×, INTERNET_IPV6_RETRY_MAX)` and `V6_NEXT_RETRY = now + V6_RETRY_INTERVAL`; reset `V6_RETRY_INTERVAL` to the probe-interval floor on success. This step MUST NOT call the v4 `teardown`, MUST NOT `ip addr flush` the v4 address, and MUST NOT `exit`.
- [X] T014 [US3] Verify `docker/cellular-internet/internet-healthcheck.sh` still gates solely on `session_established` (IPv4) + `probe_dns` — no change to its logic — and that on a v6 drop it continues to exit 0 while v4 is reachable (FR-004). Update the status write there only if needed to preserve the v6 fields via the merge in T004.

**Checkpoint**: US1–US3 — v6 is best-effort and provably cannot block VoWiFi/the bridge.

---

## Phase 6: User Story 4 - Address-change hook (Priority: P2)

**Goal**: When the global v6 address first appears or changes, invoke an optional
operator hook with the new address, isolated from the supervise loop; never on
unchanged, never on loss, never when unset.

**Independent Test**: A recording hook is called once on appear, once on change to a
new address, not on an unchanged re-observation, not on loss, and a failing/slow
hook does not disturb the sessions or the loop.

### Tests for User Story 4 ⚠️ (write first, ensure they FAIL)

- [X] T015 [P] [US4] Create `docker/cellular-internet/tests/ipv6_hook_test.sh`: configure `INTERNET_IPV6_HOOK` to a stub that appends its `$1` to a file; assert it fires once on first global address, once when the address changes, NOT on unchanged, NOT on loss (v6 → unavailable), and NOT when `INTERNET_IPV6_HOOK` is unset; assert a hook that exits nonzero / sleeps past `INTERNET_IPV6_HOOK_TIMEOUT` does not fail or stall the caller (backgrounded + `timeout`).

### Implementation for User Story 4

- [X] T016 [US4] Add `notify_v6_hook()` to `docker/cellular-internet/internet-entrypoint.sh`: when `INTERNET_IPV6_HOOK` is set and executable, `V6_ADDR` is global, and it differs from the success-marker file (`${INTERNET_STATUS_FILE}.v6notified`), run `( timeout "$INTERNET_IPV6_HOOK_TIMEOUT" "$INTERNET_IPV6_HOOK" "$V6_ADDR" && printf %s "$V6_ADDR" > marker ) &` (backgrounded; marker advances only on hook exit 0, so a failed hook is retried on the next tick). Log one warning if the path is missing/not executable. Do nothing on unchanged, on loss, or when unset.
- [X] T017 [US4] Call `notify_v6_hook` from the v6-up/change path (both the initial `dial` success and the supervise re-establish in T013), after the status write, so it fires on appear/change only.

**Checkpoint**: All user stories independently functional.

---

## Phase 7: Polish & Cross-Cutting Concerns

- [X] T018 [P] Document the new variables (`INTERNET_ENABLE_IPV6`, `INTERNET_IPV6_HOOK`, `INTERNET_IPV6_HOOK_TIMEOUT`, `INTERNET_IPV6_RETRY_MAX`) in `.env.example` with comments matching `contracts/sidecar-config.md`.
- [X] T019 [P] Add an "IPv6 reach-back (dual-stack)" section to `docs/ec20-internet-plus-vowifi.md`: the one-off `qmicli --wds-start-network="ip-type=8"` carrier-capability check, enabling dual-stack, the hook, and host-firewall responsibility (from `quickstart.md`).
- [X] T020 Run `make format && make lint && make test` and fix any failures (rustfmt/clippy are unaffected; the new work is the shell tests + `shellcheck -x`). Do not commit on any failure.
- [X] T021 Note the remaining `VERIFY-ON-HW` item in the docs: the exact `IPv6 address:` label emitted by `--wds-get-current-settings` on the real EC20/EC25 + installed libqmi (the sidecar parses `IPv6 address: <addr>/<prefix>`). The session strategy is settled (always a separate `ip-type=6` session — research.md R1), so only the label parse needs on-hardware confirmation; if it differs it is a one-line change.

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: no dependencies.
- **Foundational (Phase 2)**: depends on Setup — BLOCKS all user stories (shared `is_global_v6` + status schema).
- **US1 (Phase 3)**: depends on Foundational. The MVP slice.
- **US2 (Phase 4)**: depends on US1 (asserts the v4 path survives the US1 dial changes).
- **US3 (Phase 5)**: depends on US1 (needs `apply_settings_v6`/`v6` state) and the US2 guard.
- **US4 (Phase 6)**: depends on US1 (needs `V6_ADDR`) and US3 (fires from the supervise re-establish).
- **Polish (Phase 7)**: depends on US1–US4.

### Within Each User Story

- Test task (write first, confirm FAIL) → implementation tasks.
- Same-file implementation tasks run sequentially (they edit `internet-entrypoint.sh`).

### Parallel Opportunities

- T001 (Makefile) ∥ T002 (entrypoint config) — different files.
- New test files T005, T009, T015 are independent files ([P]); implementation edits to `internet-entrypoint.sh` are NOT parallel with each other.
- Docs T018 ∥ T019 — different files.

---

## Implementation Strategy

### MVP First (US1)

1. Phase 1 Setup → Phase 2 Foundational → Phase 3 US1.
2. STOP and validate: fake-modem dial applies a global v6 address + route and records it. This is the reach-back MVP.

### Incremental Delivery

1. Foundation ready.
2. US1 → reach-back address up (MVP).
3. US2 → v4/VoWiFi provably unchanged.
4. US3 → best-effort + backoff, bridge never blocked.
5. US4 → address-change hook for DDNS.
6. Polish → docs, env, full `make` gate, HW verification.

## Notes

- [P] = different files, no dependency. Same-file tasks are sequential by design.
- The modem is the only mock (constitution-sanctioned); dial/teardown/hook logic
  under test is the real thing, per the existing 032 harness.
- Commit after each phase (green `make lint`/`make test`), per the constitution's
  Green-on-Commit + atomic-commit principles.
- Two `VERIFY-ON-HW` items (T021) are hardware confirmations; the implementation is
  written defensively so either outcome is a localized change.
