# Tasks: Security-agreement headers on every Gm request

**Input**: `specs/082-sec-agree-all-gm-requests/` (spec.md, plan.md, research.md, data-model.md)
All paths are under `gsm-sip-bridge/src/ims/`. Each phase ends with `make format`, `make lint`, `make test` green.

## Phase 1: Shared helper (US3 safety net)

- [x] T001 Add `pub(crate) fn sec_agree_headers(Option<&str>) -> String` in `sip_client.rs`, with unit tests: `None` gives `""`, `Some` gives the three lines in order, CRLF-terminated.
- [x] T002 Switch `build_invite` (`call.rs`) and `build_message` (`sip_client.rs`) to the helper. Existing tests must pass unchanged.

## Phase 2: Fold the echo into the Gm state (FR-006)

- [x] T003 In `mod.rs`, replace the `gm_state` tuple with `GmSa { endpoints, proposal, theirs, security_verify }`; update `gm_server_addr`, `cleanup`, `reconnect_transport`, `register_session`, and the `origination.rs` test fixture. Remove the standalone `security_verify` field; keep `security_verify()` reading from `GmSa`.

## Phase 3: Requests sent from the session (US2)

- [x] T004 `OptionsRequest` gains `security_verify`; `send_gm_ping` passes it. `build_options` uses the helper.
- [x] T005 `unregister` passes the helper's lines through `extra_headers`.
- [x] T006 `SubscribeParts` gains `security_verify`; `build_subscribe` uses the helper; its caller passes `session.security_verify()`.

## Phase 4: In-dialog requests (US1)

- [x] T007 `ByeRequest` and `UpdateRequest` (`sip_client.rs`) gain `security_verify`; builders use the helper. `DialogInfo` (`agent/call.rs`) snapshots the echo from the session in `from_invite` and `from_uac_response` and passes it from `build_bye_for` and `build_update_for`.
- [x] T008 `AckParts` (ACK, BYE, PRACK) and `CancelParts` (`call.rs`) gain `security_verify`; `build_in_dialog_request` and `build_cancel` use the helper. Update every call site in `agent/origination.rs`, `agent/call.rs`, `call.rs`.

## Phase 5: Tests (US4, FR-007, FR-008)

- [x] T009 Pure test: a synthetic multi-entry `Security-Server` list yields an echo containing every entry (extract the join into a small tested function if needed).
- [x] T010 Every builder: with `Some(list)` the full block appears before the header terminator; with `None` none of `sec-agree`, `Security-Verify` appears (US3, FR-004).
- [x] T011 Session-level tests with a real loopback listener: `send_gm_ping` and `unregister` put the full list on the wire. A `DialogInfo` built from a session with a `GmSa` yields BYE and UPDATE carrying it; one built from a new session with a different list carries the new list (FR-008).

## Phase 6: Release and live verification

- [x] T012 Add the release note per the project convention (bold only the feature name).
- [ ] T013 Prime the local Vodafone rig, place a test call, capture Gm traffic, confirm the headers and no new 4xx (FR-009).
- [ ] T014 Jio verification on the Pi (arm64 build). Needs the user's go-ahead before deploying, because it changes a running remote deployment.

## Dependencies

T001 → T002 → T003 → (T004, T005, T006 | T007 → T008) → T009–T011 → T012 → T013 → T014.
