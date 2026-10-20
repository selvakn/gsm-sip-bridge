# Tasks: Q.850 Reason-based PBX reject detection

**Input**: `specs/099-pbx-reject-reason-header/` (spec.md, plan.md, research.md, data-model.md, contracts/, quickstart.md)
**Tests**: included. The constitution requires tests, and the spec's SC-004 asks for a table-driven classification test.

Before EVERY commit: `make format && make lint && make test` (CLAUDE.md). One commit per task group marked "commit".

## Phase 1: Setup

- [ ] T001 Spike R1 on a live path: with the privileged test container (`test/`) and `siptest` as the telephone side, answer the bridge's INVITE with `403` + `Reason: Q.850;cause=21` and confirm via a temporary `tracing` line in `on_call_state_cb` (`pjsua-safe/src/endpoint.rs`) that `disconnect_trigger`'s rdata path yields the response and `pjsip_msg_find_hdr_by_name(msg, "Reason")` finds it. Record the result in `specs/099-pbx-reject-reason-header/research.md` under R1 ("Verified live" or "Not delivered when ..."). Remove the temporary line. If the header is NOT delivered on this path, stop and revisit the design before Phase 2.

## Phase 2: Foundational (blocks all stories)

- [X] T002 [P] Add pure `parse_q850_cause(value: &str) -> Option<u16>` to `pjsua-safe/src/endpoint.rs` per research.md R3 (first param token `Q.850` case-insensitive; `cause=N`, N in 0..=127; quote-aware split so `text="cause=9"` is ignored; extra params ignored; anything else `None`).
- [X] T003 [P] Unit tests for `parse_q850_cause` in `pjsua-safe/src/endpoint.rs` (`#[cfg(test)]`): `Q.850;cause=21`, `Q.850 ;cause=21;text="Call Rejected"`, `q.850;CAUSE=17`, `SIP;cause=486` -> None, `Q.850;cause=999` -> None, `Q.850;cause=abc` -> None, `Q.850;text="cause=9";cause=21` -> 21, empty string -> None.
- [X] T004 Widen the per-call log entry in `pjsua-safe/src/endpoint.rs`: replace `Option<u16>` with `Disconnect { status: Option<u16>, q850: Option<u16>, response_seen: bool }` (data-model.md); keep token keying, the 64-entry bound and "entry present = disconnect seen". Update `record_in`, `status_in` and `disconnect_status` and the existing tests that use them.
- [X] T005 In `on_call_state_cb` (`pjsua-safe/src/endpoint.rs`), when the state is DISCONNECTED and the event's received message is a failure response (4xx-6xx), find `Reason` headers with `pjsip_msg_find_hdr_by_name` (iterate with the `start` cursor), read each `pjsip_generic_string_hdr.hvalue`, take the first Q.850 cause via `parse_q850_cause`, and record `Disconnect` with `response_seen` set whenever a received response was present. A null `rdata`/msg records `response_seen = false`. Reuse the rdata extraction already in `disconnect_trigger`; factor a shared helper rather than duplicating it.
- [X] T006 Expose it in `pjsua-safe/src/call.rs`: add `Call::last_disconnect() -> Option<Disconnect>`; keep `last_status()` as a wrapper over it; add a stub-only `set_last_disconnect(Disconnect)` (keep `set_last_status` working). Export `Disconnect` from the crate root. Unit test in `call.rs` guarded by `#[cfg(not(feature = "pjsip-linked"))]`, like the existing stub test.

**Checkpoint**: `make test` green; behaviour unchanged. Commit: `feat(pjsua-safe): capture the Q.850 cause of a failed final response (#99)`.

## Phase 3: User Story 1 - A reject is a reject, whatever status the PBX uses (P1)

**Goal**: cause 21 or 17 on a failure final response is a reject, 486 to the carrier.
**Independent test**: stub `Disconnect { status: 403, q850: 21 }` and `{ 503, 21 }` -> `Rejected`; `{ 503, None }` -> `Failed`.

- [ ] T007 [US1] Change `PbxOutcome::Rejected` to `{ status: u16, q850: Option<u16> }` in `gsm-sip-bridge/src/vowifi/mod.rs` and add a `q850()` accessor; update `reason()`/`status()` and every match.
- [ ] T008 [US1] Change `classify_pbx_disconnect` to take `&Disconnect` and apply contracts/decline-signaling.md rows 1, 3, 4 (cause 21/17 on a 400..=699 status first; 480/408 -> NoAnswer; else Failed). Keep the existing 486/600/603 rule working until T011 introduces the reject set.
- [ ] T009 [US1] Update `wait_for_pbx_answer` to read `pbx_call.last_disconnect()` and pass it to the classifier (`gsm-sip-bridge/src/vowifi/mod.rs`, `CallState::Disconnected` arm).
- [ ] T010 [US1] Append `:q850=<cause>` to the record string at the `format!("declined:{reason}:{status}")` site (`gsm-sip-bridge/src/vowifi/mod.rs`, near line 1721) for `pbx_rejected` whenever a cause was present (FR-010). A bare 603 stays `declined:pbx_rejected:603`.
- [ ] T011 [US1] Tests in `gsm-sip-bridge/src/vowifi/mod.rs`, extending `classify_pbx_disconnect_maps_every_status` into a table over (status, cause): `403+21`, `503+21`, `408+21`, `480+21`, `486+None`, `603+None`, `503+None`, `403+None` (-> Failed until US2/US3), cause 19 on 503 -> Failed, cause 21 with no status -> Failed, cause 17 on 486 -> Rejected carrying `q850 = Some(17)`. Add a `wait_for_pbx_answer` stub test (`#[cfg(not(feature = "pjsip-linked"))]`) for `403+21` -> `Rejected` and a record-string test for the `:q850=21` suffix.
- [ ] T012 [US1] When the outcome is not `Rejected`, add `q850` and `response_available` fields to the existing "PBX leg ended" log line, and log explicitly when no response was available (FR-011, FR-011a). Find the line via `grep -n "PBX" gsm-sip-bridge/src/vowifi/mod.rs` near the `wait_for_pbx_answer` call (~line 1698).

**Checkpoint**: `make format && make lint && make test`. Commit: `feat(vowifi): a Q.850 reject cause makes the PBX leg a reject (#99)`.

## Phase 4: User Story 2 - A bare 403 is a failure unless the operator opts in (P1)

**Goal**: 403 is not a default; the reject set is `{486, 600, 603}` plus configuration, held in one place.
**Independent test**: `403`, no cause, empty extras -> `Failed`; with extras `{403}` -> `Rejected`.

- [ ] T013 [US2] Add `RejectStatuses` in `gsm-sip-bridge/src/config/mod.rs`: a newtype with `defaults()` (`486, 600, 603` only; a comment explains why 403 is not included), `with_extra(&[u16])`, and `contains(u16)`. Unit tests: defaults exclude 403, extras are added, defaults cannot be removed.
- [ ] T014 [US2] Make `classify_pbx_disconnect` take `&RejectStatuses` and apply contracts/decline-signaling.md row 2 in place of the hard-coded `486 | 600 | 603`; thread the parameter through `wait_for_pbx_answer` and its callers and tests in `gsm-sip-bridge/src/vowifi/mod.rs`. Use `RejectStatuses::defaults()` at call sites until T016 supplies config.
- [ ] T015 [US2] Tests in `gsm-sip-bridge/src/vowifi/mod.rs`: `403` with no cause -> `Failed` (record `declined:pbx_failed:403`); same with extras `{403}` -> `Rejected` (record `declined:pbx_rejected:403`); `403+21` -> `Rejected` in both.

**Checkpoint**: green. Commit: `feat(vowifi): hold the PBX reject statuses in one set, 403 not included (#99)`.

## Phase 5: User Story 3 - Operators can add reject statuses (P2)

**Goal**: `[sip].reject_statuses = [..]` extends the set from the next start.
**Independent test**: config with `reject_statuses = [410]`; `410` with no cause -> `486` path.

- [ ] T016 [US3] Add `reject_statuses: Option<Vec<u16>>` to `RawSip` and its `Default` in `gsm-sip-bridge/src/config/raw.rs`, and `reject_statuses: RejectStatuses` to `SipConfig` in `gsm-sip-bridge/src/config/mod.rs`.
- [ ] T017 [US3] In `gsm-sip-bridge/src/config/build.rs` (beside `parse_phone_codecs`), validate every value is in 400..=699 and error as `[sip].reject_statuses: <value> is not a failure status (expected 400-699)`; build the `RejectStatuses`. Unit tests: valid list, `[200]`, `[700]`, empty list, duplicates tolerated, defaults still present with `[]`. Note: strict parsing means a wrong type (a string, not a list) must already fail; add a test.
- [ ] T018 [US3] Use `config.sip.reject_statuses` where agent B starts (the same `config.sip` used at `gsm-sip-bridge/src/vowifi/mod.rs:308`) and pass it down to `wait_for_pbx_answer` instead of `defaults()`.
- [ ] T019 [P] [US3] Document in `docs/configuration.md`: a row for `reject_statuses` in the `[sip]` table (type, default, VoWiFi/VoLTE only, global, adds to 486/600/603, Q.850 cause 21/17 already counts) and a commented FreePBX example `# reject_statuses = [403]` in the production example, with a sentence on why 403 is not a default. Also update `docs/migrating-config-to-strict-parsing.md` only if it lists `[sip]` keys.

**Checkpoint**: green. Commit: `feat(config): [sip].reject_statuses extends the PBX reject set (#99)`.

## Phase 6: Polish and verification

- [ ] T020 [P] Add an `## Unreleased` bullet to `RELEASE_NOTES.md` (bold only the feature name; plain description after it), per the repo's release-notes rule. Mention `reject_statuses` and that a FreePBX reject is now recognised via Q.850.
- [ ] T021 Live verification per `quickstart.md` step 2 using `siptest` in the privileged container: `403+cause21` -> 486; `503+cause21` -> 486; `503` -> 480; `403` -> 480; `403` with `reject_statuses = [403]` -> 486. Confirm the record `declined:pbx_rejected:403:q850=21` in `vowifi-status` and no second INVITE within 60 s. Write the outcome in `specs/099-pbx-reject-reason-header/live-verification.md` (synthetic numbers only; no real MSISDNs).
- [ ] T022 Run the full pre-commit checklist (`make format && make lint && make test`). For any Greptile bot finding on the PR, fix the code and do not post a reply.

## Dependencies

- T001 -> Phase 2.
- Phase 2 (T002-T006) blocks all stories. T002/T003 are parallel with T004.
- US1 (T007-T012) needs Phase 2. US2 (T013-T015) needs US1's classifier change (T008). US3 (T016-T019) needs US2's `RejectStatuses` (T013).
- Polish needs all stories.

## Parallel examples

- T002 and T003 (same file, different regions; coordinate) alongside T004.
- T019 (docs) while T016-T018 are in progress.
- T020 (release notes) any time after US1.

## Implementation strategy

**MVP = Phase 2 + US1.** That alone fixes the reported FreePBX case (`403` + cause 21), with no config. US2 and US3 then make the reject set explicit and configurable. Ship US1 first if a quick fix is needed; the later phases do not change its behaviour.
