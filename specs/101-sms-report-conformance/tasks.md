# Tasks: SMS Delivery Report Conformance

**Input**: `specs/101-sms-report-conformance/` (spec.md, plan.md, research.md, data-model.md, contracts/, quickstart.md)
**Tests**: included. The constitution requires them and the spec's SC-001 to SC-005 are test-shaped.

Before EVERY commit: `make format && make lint && make test` (CLAUDE.md). One commit per phase checkpoint. Use only synthetic numbers (`+919000000000`) in tests and docs.

Format: `[ID] [P?] [Story] Description`. `[P]` = different files, no dependency on an unfinished task.

## Phase 1: Setup

- [ ] T001 Confirm TS 24.229 §5.1.2A.1.1's wording (preloaded P-CSCF route, then Service-Route) from the spec text and record the clause quote or the corrected clause number in `specs/101-sms-report-conformance/research.md` R1, replacing "recalled, to be confirmed". If the clause says something that changes research R1/R2, stop and revise the plan.

## Phase 2: Foundational (blocks all stories)

- [ ] T002 Add `in_reply_to: Option<&'a str>` to `MessageRequest` in `gsm-sip-bridge/src/ims/sip_client.rs` and emit `In-Reply-To: {value}\r\n` in `build_message` after `Call-ID` when `Some` (contracts/delivery-report-message.md rule 1). Update every `MessageRequest { .. }` construction (the three test helpers around `a_delivery_report`, and `ims/session.rs`) to pass the new field.
- [ ] T003 Add the setting `sms_report_pcscf_route` (default `true`) to `VowifiConfig` and `VolteConfig` in `gsm-sip-bridge/src/config/mod.rs`, and to the raw TOML mirror in `gsm-sip-bridge/src/config/raw.rs` (`[vowifi]` `bool` default `true`; `[volte]` `Option<bool>` inheriting `[vowifi]`'s value, exactly as `sms_delivery_report` does). Extend the existing config tests (`sms_delivery_report_is_on_unless_switched_off` and the inheritance test) with the same cases for the new key.
- [ ] T004 Plumb the value to the dispatcher: add `sms_report_pcscf_route: bool` to the agent config struct beside `sms_delivery_report` in `gsm-sip-bridge/src/ims/agent/mod.rs` (~line 735) and to `DispatchParams`; set it from the VoWiFi config at `ims/agent/mod.rs` (~line 386 and the `sms_delivery_report` literals ~lines 810, 981) and from `app_config.volte` in `gsm-sip-bridge/src/volte/carrier_agent.rs` (~line 232). Fix any other struct literal the compiler flags.

**Checkpoint**: `make format && make lint && make test` green; no behaviour change yet. Commit: `refactor(ims): carry In-Reply-To and a P-CSCF-route setting toward the SMS report sender (#140)`.

## Phase 3: User Story 1 - The report names the SMS it acknowledges (P1) 🎯 MVP

**Goal**: every report carries `In-Reply-To` = the inbound Call-ID.
**Independent test**: feed an inbound SMS, inspect the report bytes.

- [ ] T005 [US1] Change `send_sms_delivery_report` in `gsm-sip-bridge/src/ims/session.rs` to take `in_reply_to: Option<&str>` and pass it into `MessageRequest`. Return `Option<String>`: the report's Call-ID on a successful write, `None` on send failure (used by US4).
- [ ] T006 [US1] Pass `req.header("Call-ID")` from all three call sites in `gsm-sip-bridge/src/ims/agent/mod.rs`: `acknowledge()` (~line 1737), the non-SMS-DELIVER TPDU branch (~line 1331) and the undecodable-TPDU branch (~line 1360). When the inbound request has no Call-ID, send without it and `tracing::warn!` (spec edge case).
- [ ] T007 [P] [US1] Builder tests in `gsm-sip-bridge/src/ims/sip_client.rs`: `In-Reply-To` present and exactly equal to the value given (use a Call-ID with an `@` and dots); absent when `None`; every other header and the body identical between the two (SC-003 for this field).
- [ ] T008 [US1] Dispatch-level test in `gsm-sip-bridge/src/ims/agent/mod.rs` (follow the nearest existing `handle_message`/`acknowledge` test): two inbound SMS with different Call-IDs produce two reports whose `In-Reply-To` values each match their own message; a redelivery of the same Call-ID produces the same value and the dedupe behaviour is unchanged. Also cover the RP-ERROR and non-deliver-TPDU paths.

**Checkpoint**: `make format && make lint && make test`. Commit: `feat(ims): SMS delivery report carries In-Reply-To (TS 24.341 §5.3.2.4) (#140)`.

## Phase 4: User Story 2 - The report is routed through the P-CSCF (P1)

**Goal**: `Route: <P-CSCF;lr>` precedes the Service-Route entries, behind the setting.
**Independent test**: build the report for no / two Service-Route entries; setting on / off.

- [ ] T009 [US2] Add a `pcscf_route_header(&RegisteredSession) -> String` helper in `gsm-sip-bridge/src/ims/session.rs` beside `service_route_headers`: host and port from `session.pcscf_addr`, with the port replaced by `gm_state.theirs.port_s` when a Gm SA exists; formatted with `format_sip_addr` (brackets IPv6); returns `Route: <sip:{addr};lr>` (research R2).
- [ ] T010 [US2] In `send_sms_delivery_report` (`gsm-sip-bridge/src/ims/session.rs`), when the new `pcscf_route: bool` argument is true, build `route_headers` as the P-CSCF header first, then `service_route_headers(session)` in order; when false, today's list. Pass `p.sms_report_pcscf_route` from the three call sites in `ims/agent/mod.rs` (acknowledge() gets the flag as a parameter).
- [ ] T011 [P] [US2] Tests in `gsm-sip-bridge/src/ims/session.rs` / `sip_client.rs` as the helper's visibility allows: with no Service-Route -> exactly one `Route`, the P-CSCF; with two entries -> P-CSCF first, then both in order; Gm SA present -> protected server port, absent -> `pcscf_addr` port; IPv6 P-CSCF is bracketed; setting off -> today's list exactly; security headers (`Require`, `Proxy-Require`, `Security-Verify`) unchanged in all cases.
- [ ] T012 [US2] SC-003 byte-diff test: build one report before-shape (setting off, `in_reply_to: None`) and one after-shape (setting on, `Some(..)`) from the same inputs and assert the only differences are the added `Route` and `In-Reply-To` lines.

**Checkpoint**: `make format && make lint && make test`. Commit: `feat(ims): route the SMS delivery report via the P-CSCF first (TS 24.341 Annex B.6) (#140)`.

## Phase 5: User Story 4 - A rejected report is visible (P2)

**Goal**: one warning per final non-2xx answer; none for 1xx or 2xx.
**Independent test**: feed a `500` and a `202` to a sent report.

- [ ] T013 [US4] Add the bounded in-flight set (data-model.md: `VecDeque<String>`, cap 32, oldest evicted) to the dispatch state in `gsm-sip-bridge/src/ims/agent/mod.rs`; insert the `Some(call_id)` returned by `send_sms_delivery_report` at each call site.
- [ ] T014 [US4] In the response handler in `gsm-sip-bridge/src/ims/agent/mod.rs` (the `received response outside an active transaction` path, ~line 3440), before the generic line: if the response's `Call-ID` is in the set and its CSeq method is `MESSAGE`, then 1xx -> leave it and return; 2xx -> remove, `debug!`, return; `>=300` -> remove and `warn!` once with the report Call-ID, status, reason and any `Warning` header text (FR-006, FR-012), return. No resend (FR-007).
- [ ] T015 [US4] Tests in `gsm-sip-bridge/src/ims/agent/mod.rs`: `500` with the #140 `Warning` text -> exactly one warning containing `500` and the Call-ID; `202` -> no warning; `100` then `500` -> one warning total; a duplicate `500` after removal -> no second warning; 33 reports in flight -> the oldest entry is evicted without panic.

**Checkpoint**: `make format && make lint && make test`. Commit: `feat(ims): warn when the network rejects an SMS delivery report (#140)`.

## Phase 6: Docs

- [ ] T016 [P] Document `sms_report_pcscf_route` in `docs/configuration.md`: one row under `[vowifi]` (beside `sms_delivery_report`, ~line 277) and one under `[volte]` (~line 355), per contracts/config.md, including that off restores Service-Route-only routing. Check whether a sample config under the repo (`grep -rn sms_delivery_report --include=*.toml .`) needs the key; the earlier grep found none outside docs.
- [ ] T017 [P] Add a `RELEASE_NOTES.md` entry in the project's style (bold only the summary, plain-text description): the report now carries `In-Reply-To` and a P-CSCF-first `Route` per TS 24.341 §5.3.2.4 / Annex B.6, the setting, and an explicit statement that this closes two conformance gaps found while investigating #140 and is **not** a claimed fix for T2.

**Checkpoint**: `make format && make lint && make test`. Commit: `docs: SMS delivery report conformance setting and release note (#140)`.

## Phase 7: Live verification (merge gate, User Story 3)

Hardware and rig steps; follow quickstart.md. Do not start before Phases 3-5 are green.

- [ ] T018 [US3] Build the Pi image (`arm-build-200` pipeline, next `jio-gmN` tag) and deploy to the remote Jio Pi by recreating the container (not restarting). Send 5 SMS in a row to the line. Record in `specs/101-sms-report-conformance/quickstart.md` (new "Results" section): each report's answer (`202`?), SMS forwarded, no new warnings. Capture one report and check it against contracts/delivery-report-message.md.
- [ ] T019 [US3] Run the same on the local Vodafone rig (pjsip-linked build on the host): 5 SMS, each report answered `2xx` or no worse than before, delivery unchanged. Record results.
- [ ] T020 [US3] Rollback check on Jio: set `sms_report_pcscf_route = false`, recreate, send 1 SMS; the report has `In-Reply-To` and no P-CSCF `Route` (User Story 3 scenario 4).
- [ ] T021 [US3] Decision gate: if T018 or T019 shows any regression or any non-2xx where there was none, implement FR-010 (add the P-CSCF `Route` only when the registration returned no `Service-Route`) in `send_sms_delivery_report`, add tests for it, update T011/T012 expectations and the docs, then repeat T018-T020.
- [ ] T022 Final `make format && make lint && make test`. Open the PR referencing #140; leave the issue open. Post to #140 (a human reply, not a bot) a summary and a request that the reporter run the build and post the report and its answer (SC-006). Do not claim a fix.

## Dependencies and order

- T001 first. Phase 2 blocks everything. Phases 3, 4, 5 each depend on Phase 2; Phase 4 and Phase 5 both modify `send_sms_delivery_report`'s signature, so do them in order 3 -> 4 -> 5 rather than in parallel (Phase 3's T005 already fixes the return type Phase 5 needs).
- T007 and T011 are `[P]` with their phase's implementation tasks only in the sense that they touch different files; each needs its phase's code to compile.
- Phase 6 can run alongside Phase 5. Phase 7 needs Phases 3-5. T022 needs everything.
- MVP: Phases 1-3 alone ship `In-Reply-To`. Phase 4 is the one needing the live gate.
