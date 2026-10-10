# Tasks: Caller identity from `tel:` URIs

**Input**: `specs/083-tel-uri-caller-id/` (spec.md, plan.md, research.md, data-model.md, contracts/identity-parsing.md, quickstart.md)

**Tests**: required. SC-003 asks for one test per edge case, and the
constitution makes TDD the default. Every row of `contracts/identity-parsing.md`
becomes a test, referred to below as C1.n, C2.n and so on. Write each test
first and watch it fail before implementing.

Unless a path says otherwise, it is under `gsm-sip-bridge/src/ims/`. Each phase
ends with `make format`, `make lint` and `make test` green, followed by one
commit (constitution II and III). Use only synthetic numbers
(`+919000000000`/`…01`/`…99`).

## Phase 1: Setup

- [x] T001 Create `identity.rs` with a module doc comment citing RFC 3261 §25.1, RFC 3325 §9.1 and RFC 3966 §3. Register it as `pub(crate) mod identity;` next to `pub mod session;` in `mod.rs`.

## Phase 2: Foundational (blocks every story)

The shared grammar. These pieces carry no story label because US1, US2 and US3
all consume them.

- [x] T002 Move `split_route_list` and its two tests (`split_route_list_ignores_commas_inside_quoted_display_names`, `split_route_list_ignores_commas_inside_uris`) from `agent/call.rs` (~line 698 and ~811) into `identity.rs`. Rename it `pub(crate) fn split_header_values(value: &str) -> Vec<&str>` and rename the tests to match. In `agent/call.rs`, call `crate::ims::identity::split_header_values` and delete the local copy. This is a pure move: no behaviour changes, and the Record-Route tests in `agent/call.rs` must pass untouched.
- [x] T003 In `identity.rs`, write the failing tests for C2.1–C2.11. Each test calls `parse_name_addr(value, HeaderParams::…)` and asserts `display` and `uri` (or `None`).
- [x] T004 In `identity.rs`, add `pub(crate) enum HeaderParams { Allowed, None }`, `pub(crate) struct NameAddr<'a> { pub display: Option<String>, pub uri: &'a str }` and `pub(crate) fn parse_name_addr(value: &str, params: HeaderParams) -> Option<NameAddr<'_>>`.
  - **Quoted display name**: move the escape-aware loop from `header_display_name` (`session.rs` ~line 640) here, keeping its rules: `\`-escapes, an unterminated quote means `None`, CR/LF means no display name, an empty name means `None`.
  - **Token display name**: the trimmed text before `<`.
  - **name-addr**: the URI is the text between `<` and the next `>`.
  - **addr-spec**: the URI is the trimmed value, cut at the first `;` only when `params == Allowed` (research R2).
  - Return `None` unless the URI has a `:`.
  - Make C2 pass.
- [x] T005 In `identity.rs`, write the failing tests for C1.1–C1.23. Each test calls `uri_number(uri)`.
- [x] T006 In `identity.rs`, add a private `fn percent_decode(s: &str) -> Option<String>`. Hex is case-insensitive. A `%` not followed by two hex digits, or non-UTF-8 output, means `None`.
- [x] T007 In `identity.rs`, add `pub(crate) fn uri_number(uri: &str) -> Option<String>`, following data-model.md:
  - Split the scheme at the first `:` and compare it ignoring ASCII case.
  - **`tel`**: the raw part is the text before the first `;`. It is always a phone number.
  - **`sip`/`sips`**: the raw part is the text before the first `@`. No `@` means `None`. Drop the `:password` suffix and cut at the first `;`. It is a phone number if a `;user=phone` URI param exists (case-insensitive) or the raw part starts with `+` / `%2B`.
  - **Any other scheme**: `None`.
  - Then percent-decode. For a phone number, remove `-`, `.`, `(` and `)`. Finally require a non-empty result where every char is ASCII alphanumeric or one of `+*#-._~` (FR-008).
  - Doc-comment each rule with its RFC section. Make C1 pass.
- [x] T008 In `identity.rs`, add `pub(crate) struct Identity { pub number: String, pub display: Option<String> }` and `pub(crate) fn header_identity(req: &SipRequest, name: &str, params: HeaderParams) -> Option<Identity>`.
  - Collect `NameAddr`s from `req.headers_all(name)` → `split_header_values` → `parse_name_addr`.
  - **number**: the first `tel` value whose `uri_number` resolves; otherwise the first `sip`/`sips` value that resolves.
  - **display**: the first non-empty `display` among all values of this header (FR-010, FR-011).
  - Add unit tests for: a tel-preferred number, a name taken from the other value, and `None` when nothing resolves.

**Checkpoint**: the grammar is complete and tested. `session.rs` is untouched,
so production behaviour is unchanged. Commit.

## Phase 3: User Story 1 — caller number for `tel:`-only carriers (P1) 🎯 MVP

**Goal**: the issue's T2 INVITE yields `+919000000000`.
**Independent test**: C3.1, C3.6 and C3.8 pass through `extract_caller`.

- [x] T009 [US1] In `agent/mod.rs` tests (next to `extract_caller_falls_back_to_unknown_when_from_is_unparseable`, ~line 2839), add failing tests built with `SipRequest::try_parse` for:
  - C3.1, the issue's exact headers;
  - C3.6, host-only `From` → `"unknown"`;
  - C3.8, compact `f:`;
  - `sips:`/upper-case scheme variants.

  Update the existing `extract_caller_prefers_p_asserted_identity_over_from` (its `From` is host-only, which now yields no number; the assertion still holds) and check that `extract_caller_falls_back_to_unknown_when_from_is_unparseable` still holds.
- [x] T010 [US1] In `session.rs`, re-implement `extract_caller` as:

  ```rust
  identity::header_identity(req, "P-Asserted-Identity", HeaderParams::None)
      .or_else(|| identity::header_identity(req, "From", HeaderParams::Allowed))
      .map(|i| i.number)
      .unwrap_or_else(|| "unknown".to_string())
  ```

  Keep its existing doc comment, and add a sentence that `tel`/`sips` and multi-value PAI are accepted (RFC 3325 §9.1, issue #104). Make T009 pass.

**Checkpoint**: US1 is shippable alone. Commit.

## Phase 4: User Story 2 — the asserted identity always wins (P1)

**Goal**: PAI's number *and* name win for every valid PAI form. `From` is used
only when PAI yields nothing, and never mixed with it.
**Independent test**: C3.2–C3.5 and C3.7 pass through `extract_caller` plus
`extract_caller_name`.

- [ ] T011 [US2] In `agent/mod.rs` tests, fix the test that passed by accident: in `extract_caller_name_reads_the_quoted_display_name_from_p_asserted_identity` (~line 2871), change the `From` display name to `"Other Name"` and its number to `+919000000001`. Assert the name is still `Firstname Lastname` and `extract_caller` is `+919000000000`. Run it against the current code first and confirm it **fails** for the name. That proves the old pass was accidental.
- [ ] T012 [US2] In `agent/mod.rs` tests, add failing tests for C3.2, C3.3 (two separate `P-Asserted-Identity:` lines in the raw request), C3.4, C3.5 and C3.7, asserting both `extract_caller` and `extract_caller_name`. Add one test confirming `caller_name_for_onward_signaling` still returns `None` under `Privacy: id` with a `tel:` PAI (FR-012; `agent/inbound.rs` ~line 141, test in that file's test module).
- [ ] T013 [US2] In `session.rs`, delete `header_user_part` (kept until now because `extract_caller_name` still used it) and re-implement `extract_caller_name`:

  ```rust
  match identity::header_identity(req, "P-Asserted-Identity", HeaderParams::None) {
      Some(pai) => pai.display,
      None => identity::header_identity(req, "From", HeaderParams::Allowed).and_then(|f| f.display),
  }
  ```

  - Rewrite `header_display_name` as a thin wrapper: the first `parse_name_addr(...).display` of the header's first value. Keep it only while a caller remains; if `extract_caller_name` was its sole user, delete it and move its doc rationale (the `qdtext`/`<` explanation and the Nokia SBC note) onto `parse_name_addr`.
  - Update `extract_caller_name`'s doc: "the same header" now means the same header's values, never the other header.
  - Make T011 and T012 pass, along with every existing CNAP and `caller_identity_is_private` test.

**Checkpoint**: US1 and US2 both pass independently. Commit.

## Phase 5: User Story 3 — SMS delivery reports reach the gateway (P2)

**Goal**: a two-value PAI addresses the `sip` value. A bare `From` drops its
`;tag`. The live Jio form is byte-identical.
**Independent test**: C4.1–C4.7 pass through `header_uri`.

- [ ] T014 [P] [US3] In `agent/mod.rs` tests (next to `header_uri_keeps_the_whole_uri_from_a_bracketed_header`, ~line 3090), add failing tests for C4.1–C4.4 using `message_with_headers`. Keep C4.5, C4.6 and C4.7, which already exist as `header_uri_keeps_parameters_of_an_unbracketed_uri`, `header_uri_keeps_the_whole_uri_from_a_bracketed_header` and `header_uri_is_none_without_a_uri`, unchanged as the FR-014 guard.
- [x] T015 [US3] In `identity.rs`, add `pub(crate) fn header_uri_values<'a>(req: &'a SipRequest, name: &str, params: HeaderParams) -> Vec<NameAddr<'a>>`, and reuse it inside `header_identity`.
- [ ] T016 [US3] In `session.rs`, re-implement `header_uri(req, name)`:
  - Pick `HeaderParams::Allowed` when `name` is `From`/`To`/`Contact` (case-insensitive), otherwise `HeaderParams::None`.
  - Return the first URI whose scheme is `sip`/`sips` (case-insensitive), otherwise the first URI, as an owned `String`.
  - Update its doc comment: drop "parameters after the URI, if any, are the URI's own" as a general rule, and cite RFC 3261 §20.10 for `From` and RFC 3325 §9.1 for PAI.
  - Callers in `agent/mod.rs` (~lines 1147, 1176, 1538) are unchanged. Make T014 pass.

**Checkpoint**: delivery-report addressing is conformant, and the existing live
forms are unchanged. Commit.

## Phase 6: Diagnosing failures (FR-015, cross-story)

- [ ] T017 In `session.rs` tests, add failing tests for C5.1–C5.3, calling `unresolved_caller_diagnostic(&req)`. For C5.3, assert the returned text contains neither `'\r'` nor `'\n'`.
- [ ] T018 In `session.rs`, add `pub(crate) fn unresolved_caller_diagnostic(req: &SipRequest) -> Option<String>`.
  - It returns `None` when `extract_caller` resolves.
  - Otherwise it returns `format!("P-Asserted-Identity={:?} From={:?}", req.headers_all("P-Asserted-Identity"), req.headers_all("From"))`. `{:?}` escapes control characters, and an empty `Vec` shows absence.
  - Make T017 pass.
- [ ] T019 In `agent/mod.rs` INVITE dispatch (the function containing the re-INVITE / retransmission branch, ~line 2190), insert the warning immediately after the `if let Some(call) = self.active_call…` block's early returns and before the `occupant`/busy check (~line 2245):

  ```rust
  if let Some(raw) = unresolved_caller_diagnostic(req) {
      tracing::warn!(headers = %raw, "inbound call has no readable caller identity");
  }
  ```

  It must run once per new INVITE. Add `unresolved_caller_diagnostic` to the `use` list (~line 51).
- [ ] T020 In `handle_message` in `agent/mod.rs`, after the `if let Some(decoded) = &decoded { sender = … }` block (~line 1192) and before the Type-0 check, log the same warning ("SMS has no readable sender identity") only when `decoded.is_none()` and `unresolved_caller_diagnostic(req)` is `Some`.

**Checkpoint**: an unreadable identity leaves exactly one warning per request
(SC-007). Commit.

## Phase 7: Polish & release

- [ ] T021 [P] Grep for any remaining literal `split("sip:")`-style identity parsing in `gsm-sip-bridge/src/ims/` and `gsm-sip-bridge/src/vowifi/`. Confirm none remains outside `identity.rs`. The out-of-scope `pjsua-safe/src/call.rs` `parse_uri_user` stays as is.
- [ ] T022 [P] Add a release note to `RELEASE_NOTES.md` under the unreleased section, bolding only the summary. For example: "**Caller ID from `tel:` numbers** — incoming calls and SMS now show the caller's number when the carrier sends it as a `tel:` URI (e.g. T2), and the network-verified caller identity is used consistently on all carriers (#104)." Leave internal refactors out of the note.
- [ ] T023 Run the full `make format && make lint && make test`. Compare every contract row against a named test, and list any gaps in the PR description.
- [ ] T024 Live check on the Vodafone rig per `quickstart.md` step 1 (SC-005): the caller number and CNAP name on the PBX leg are the same as before.
- [ ] T025 Live check on the Jio Pi per `quickstart.md` step 2 (SC-006): the delivery-report `ipsmgw=` is unchanged. This needs the user's go-ahead first, because it changes a running remote deployment.
- [ ] T026 After release, comment on issue #104 asking the reporter to confirm on T2 (SC-001). This is outward-facing, so confirm the wording with the user first.

## Dependencies

```text
T001 → T002 → T003 → T004 → T005 → T006 → T007 → T008     (Phase 2, sequential: one file)
                                              │
            ┌─────────────────────────────────┼──────────────────────┐
            ▼                                 ▼                      ▼
   US1: T009 → T010          US2: T011 → T012 → T013     US3: T014 → T015 → T016
            │                     (needs T010's            (independent of US1/US2)
            │                      session.rs edit)
            └──────────────┬──────────────┘
                           ▼
                 FR-015: T017 → T018 → T019 → T020   (uses the final extract_caller)
                           ▼
                 Polish: T021, T022 [P] → T023 → T024 → T025 → T026
```

- US2 builds on US1's `extract_caller` change in the same file, so do it
  after US1.
- US3 touches `header_uri` only and can go in parallel with US1/US2 once
  Phase 2 is done. T014 is in a different test region and can be written
  meanwhile.

## Parallel opportunities

- **During US1/US2**: write T014 (US3's tests in `agent/mod.rs`), which
  touches a different test region.
- **Polish**: T021 (a read-only grep) and T022 (`RELEASE_NOTES.md`).
- Phase 2 is all in one file, so it stays sequential.

## Implementation strategy

1. **MVP**: Phases 1–3. The issue is fixed on T2, and `tel:` PAI is read
   everywhere.
2. **Add US2**, which closes the PAI-bypass and accidental-test gap on
   Jio/Vodafone.
3. **Add US3**, which hardens delivery-report addressing.
4. **Add FR-015**, then polish and live checks. Ship as one PR referencing
   #104, with one commit per phase.
