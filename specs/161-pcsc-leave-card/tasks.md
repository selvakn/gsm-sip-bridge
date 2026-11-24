# Tasks: Stop the Bridge Resetting a Shared Card-Reader SIM

**Input**: Design documents from `specs/161-pcsc-leave-card/` (spec.md, plan.md, research.md, data-model.md, quickstart.md)
**Prerequisites**: plan.md (Blast Radius filled with grep output), research.md decisions D1–D6

**Tests**: requested. The spec's SC-001 to SC-004 are test-defined, and the constitution makes TDD the default. Write each story's tests first and watch them fail (or, for hardware-gated tests, watch the control fail) before implementing.

**Paths**: crate root is `gsm-sip-bridge/` (so source is `gsm-sip-bridge/src/...`), repo root for docs. Use `CARGO_TARGET_DIR=$PWD/target-161` for every cargo/make command so another worktree's build cannot clobber this one.

**Never** put a real IMSI/MSISDN in code, tests or docs. Tests use the synthetic `001010000000001` or read the IMSI off the card at run time.

## Format: `[ID] [P?] [Story] Description`

## Phase 1: Setup

- [X] T001 Confirm the branch is `161-pcsc-leave-card` with no upstream (`git rev-parse --abbrev-ref @{u}` must fail), then run `CARGO_TARGET_DIR=$PWD/target-161 make test` from the repo root to record a green baseline. Note the test count in the PR description.

---

## Phase 2: Foundational (blocks US1 and US2)

**Purpose**: the one type that owns the card's release disposition (research D1).

- [X] T002 Add `pub(crate) struct SharedCard(Option<pcsc::Card>)` to `gsm-sip-bridge/src/modules/pcsc_card.rs`:
  - `pub(crate) fn connect(ctx: &Context, reader: &CStr) -> Result<SharedCard, pcsc::Error>` calls `ctx.connect(reader, ShareMode::Shared, Protocols::ANY)`.
  - `Deref`/`DerefMut<Target = Card>` use `self.0.as_ref()/as_mut().expect("SharedCard used after release")`. The value is only `None` inside `Drop`.
  - `impl Drop` does `if let Some(card) = self.0.take() { if let Err((card, _)) = card.disconnect(Disposition::LeaveCard) { std::mem::forget(card) } }`.
  - Doc comment: the `pcsc` crate's own `Card::drop` hard-codes `SCARD_RESET_CARD` (pcsc 2.9.0 `lib.rs:1668`), which breaks every other program's session on the card (#232). `forget` after a failed disconnect leaks one `Context` reference and is safe, because pcscd's cleanup leaves the card (research K/L).
  - No `unsafe`.

**Checkpoint**: `SharedCard` compiles. No call site uses it yet.

---

## Phase 3: User Story 1 - The bridge never disturbs another program's card session (P1) 🎯 MVP

**Goal**: every card connection the bridge opens is released leaving the card (FR-001, FR-008).

**Independent Test**: the hardware-gated tests below on the reader host, plus quickstart §3 scenarios B and the reader listing (10/10 `transmit ok`).

### Tests for User Story 1

- [X] T003 [US1] In a new `#[cfg(test)] mod hardware` inside `gsm-sip-bridge/src/modules/pcsc_card.rs`, add a helper `first_real_reader(ctx) -> CString` (it uses `real_reader_candidates`, and the test panics with a clear message if there is none) and a helper `holder(ctx, reader) -> Card`. The holder is a bare `Card` held by the test, connected shared, with one `SELECT MF` (`00A40004023F00`) sent inside a transaction.
- [X] T004 [US1] Add `#[ignore = "needs a PC/SC reader with a SIM; run with --ignored on the reader host"] fn bare_card_drop_resets_other_handles()` (the control) in `gsm-sip-bridge/src/modules/pcsc_card.rs`:
  - Open the holder on its own `Context`.
  - On a second `Context`, `connect` a bare `Card` and drop it.
  - Assert the holder's next `transmit` fails with `pcsc::Error::ResetCard`.
  - This proves the test rig can see the bug.
- [X] T005 [US1] Add `#[ignore = …] fn shared_card_drop_leaves_other_handles_working()` in `gsm-sip-bridge/src/modules/pcsc_card.rs`: the same setup as T004, but the second handle is a `SharedCard`. Assert the holder's next transmit succeeds (SW `9000` or `61xx`). Run it before T006 and confirm it fails to compile or fails. It passes after T006 only if T002 is right.
- [X] T006 [US1] Add `#[ignore = …] fn shared_card_released_by_panic_leaves_the_card()` in `gsm-sip-bridge/src/modules/pcsc_card.rs`: connect a `SharedCard` inside `std::panic::catch_unwind`, then panic. Assert the holder still transmits fine. This is the "every exit path" part of FR-001.

### Implementation for User Story 1

- [X] T007 [US1] Change `PcscTransport { card: Card }` to `PcscTransport { card: SharedCard }` in `gsm-sip-bridge/src/modules/pcsc_card.rs`. `connect()`'s `ctx.connect(...)` becomes `SharedCard::connect(&ctx, &reader_cstr)` (the error arm logs exactly as today). `with_transaction` and `transmit_apdu` keep working through `Deref`.
- [X] T008 [P] [US1] In `gsm-sip-bridge/src/modules/pcsc_list.rs:147` replace `ctx.connect(&reader_cstr, ShareMode::Shared, Protocols::ANY)` with `crate::modules::pcsc_card::SharedCard::connect(&ctx, &reader_cstr)`. Keep the `Err(pcsc::Error::NoSmartcard)` arm and every row message unchanged. Drop now-unused imports (`ShareMode`, `Protocols`) only if clippy says so.
- [X] T009 [US1] Verify no bare `Card` survives: `grep -rn 'ctx.connect(\|\.connect(&' gsm-sip-bridge/src` must show only `SharedCard::connect`'s own body. Run T004–T006 on the reader host (`CARGO_TARGET_DIR=$PWD/target-161 cargo test -p gsm-sip-bridge hardware -- --ignored --test-threads=1`); all pass.

**Checkpoint**: US1 complete. Quickstart §3 scenario B and the reader-listing loop give 10/10.

---

## Phase 4: User Story 2 - One line's search for its card does not disturb other lines' cards (P1)

**Goal**: readers rejected by `PcscTransport::connect` are released leaving the card (FR-002).

**Independent Test**: T010 on the reader host, plus quickstart §3 scenario E (10/10).

### Tests for User Story 2

- [X] T010 [US2] Add `#[ignore = …] fn connect_for_another_imsi_leaves_the_card()` in `gsm-sip-bridge/src/modules/pcsc_card.rs`:
  - Open the holder.
  - Call `PcscTransport::connect("001010000000001")` and assert it is `Err` and the message contains `no PC/SC reader has a card matching`.
  - Assert the holder's next transmit succeeds.

### Implementation for User Story 2

- [X] T011 [US2] In `PcscTransport::connect`'s candidate loop (`gsm-sip-bridge/src/modules/pcsc_card.rs`), confirm every non-returning arm (IMSI mismatch, probe error, transaction-begin error) lets its `SharedCard` drop at the end of the iteration. No early `continue` may hold a bare `Card`. Add a one-line comment at the loop head saying rejected candidates are released leaving their card, because another line may be mid-AKA on it (#232). Run T010; it passes.

**Checkpoint**: US1 and US2 together are the first commit (T012).

- [X] T012 [US1] [US2] Commit 1. Run `make format`, `make lint`, `make test` (all with `CARGO_TARGET_DIR=$PWD/target-161`); all must pass. Then commit `fix(pcsc): release every card connection without resetting the card (#232)`. The body explains the crate's `Card::drop` resets, and gives the repro evidence (scenarios B/E) and the sibling-line case. End it with the `Co-Authored-By` line from the session's attribution reminder.

---

## Phase 5: User Story 3 - A challenge answer cannot be split by another program (P2)

**Goal**: on a card-reader line, USIM selection, AUTHENTICATE and GET RESPONSE run in one PC/SC transaction. The AID comes from the identifying probe. Modem lines are unchanged (FR-003 to FR-006, research D2/D4).

**Independent Test**: T013–T015 in CI, T022 on the reader host.

### Tests for User Story 3

- [X] T013 [P] [US3] In `gsm-sip-bridge/src/modules/usim.rs` tests (next to `QueuedTransport`, `:464`), add `select_and_authenticate_selects_then_authenticates_in_order`. Script replies for SELECT MF (`9000`), SELECT AID (`9000`), AUTHENTICATE (`6135`) and GET RESPONSE (a success `DB…9000` body). Assert the recorded APDUs are, in order: `00A40004023F00`, a SELECT-by-AID with the given AID, `008800812210…`, `00C0000035`. If `QueuedTransport` does not record sent APDUs yet, extend it to.
- [X] T014 [P] [US3] In `gsm-sip-bridge/src/modules/usim.rs` tests, add `select_and_authenticate_stops_when_select_fails`: SELECT AID answers `6A82`. Assert the result is `Err`, and no `0088…` APDU was sent.
- [X] T015 [P] [US3] In `gsm-sip-bridge/src/ims/registration_auth.rs` `scripted` tests (next to `Probe`, `:328`), add `run_aka_goes_through_the_transports_authenticate_hook`. Use a transport whose `transmit_apdu` panics and whose `authenticate_usim` override records the RAND/AUTN and returns a scripted `AkaResult::Success`. Assert `crate::ims::run_aka` returns it and the hook saw the challenge's RAND/AUTN. The comment at the mock site justifies it: no SIM in CI; this pins that AKA cannot bypass the hook.

### Implementation for User Story 3

- [X] T016 [US3] In `gsm-sip-bridge/src/modules/usim.rs`:
  - Add the provided trait method `fn authenticate_usim(&mut self, rand: &[u8; 16], autn: &[u8; 16]) -> BridgeResult<AkaResult> { authenticate(self, rand, autn) }` to `ApduTransport`. The trait doc should say the default authenticates against the already-selected ADF, and that `PcscTransport` overrides it to select and authenticate in one transaction.
  - Add `pub(crate) fn select_and_authenticate(t: &mut dyn ApduTransport, aid: &[u8], rand: &[u8; 16], autn: &[u8; 16]) -> BridgeResult<AkaResult> { select_usim(t, aid)?; authenticate(t, rand, autn) }`.
  - If `authenticate(self, …)` inside the default needs `Self: Sized`, call it through `self as &mut dyn ApduTransport`. Keep the trait object-safe.
  - T013/T014 pass.
- [X] T017 [US3] In `gsm-sip-bridge/src/ims/mod.rs` `run_aka` (`:1146`), replace `usim::authenticate(card, &rand_arr, &autn_arr)` with `card.authenticate_usim(&rand_arr, &autn_arr)`. T015 passes. Every existing registration and refresh test passes unchanged (SC-003).
- [X] T018 [US3] In `gsm-sip-bridge/src/modules/pcsc_card.rs`:
  - `read_transport_imsi` becomes `read_transport_identity(t) -> BridgeResult<(Vec<u8>, String)>`, returning `(aid, imsi)`.
  - `PcscTransport` gains `aid: Vec<u8>`, set from the matching candidate in `connect`.
  - The match arms compare the IMSI as today.
  - Tracing and error messages are unchanged.
- [X] T019 [US3] In `gsm-sip-bridge/src/modules/pcsc_card.rs`, override `authenticate_usim` in `impl ApduTransport for PcscTransport`: `let aid = self.aid.clone(); self.with_transaction(|t| usim::select_and_authenticate(t, &aid, rand, autn))`. Window-start failures keep the existing `PC/SC transaction begin failed: {e}` wording (FR-006).
- [X] T020 [US3] In `gsm-sip-bridge/src/ims/mod.rs` `open_card` (`:1091-1141`), run `discover_usim_aid` + `select_usim` and the `selected USIM application` log only on the modem branch (`!cfg.pcsc_reader`). The pcsc branch returns the card unselected. Update the `OpenedCard` doc (`:1081`) and the `open_card` doc (`:1089`): a modem line's USIM is selected at open, while a card-reader line selects inside each AKA's transaction (#232).
- [X] T021 [US3] Check the resync path: in `gsm-sip-bridge/src/ims/mod.rs` `register_session` (around `:1300` and the `MAX_RESYNC_ATTEMPTS` loop) and `gsm-sip-bridge/src/ims/registration_auth.rs` `answer`, every AKA goes through `run_aka`. Each resync is therefore a fresh `authenticate_usim` call, and so its own window (FR-004). `grep -rn 'usim::authenticate(' gsm-sip-bridge/src` must show only the trait default and `select_and_authenticate`.
- [X] T022 [US3] Add `#[ignore = …] fn authenticate_usim_runs_in_one_window_and_leaves_the_card()` in `gsm-sip-bridge/src/modules/pcsc_card.rs`:
  - Read the card's IMSI at run time through `crate::modules::pcsc_list`'s identity reader on a temporary `SharedCard` in a transaction. Never hard-code it.
  - `PcscTransport::connect(&imsi)`, then `authenticate_usim(&[0x11; 16], &[0x22; 16])`.
  - Assert it returns `Err` whose message contains `AUTHENTICATE failed` (the card rejects a bogus AUTN with a MAC failure; harmless, no SQN change), not a `transaction begin` or `transmit` failure.
  - Then open the holder and assert it transmits fine after the transport is dropped.
  - Run on the reader host; it passes.
- [X] T023 [US3] Correct the `with_transaction` doc comment in `gsm-sip-bridge/src/modules/pcsc_card.rs:139-145`. Remove the claim that AUTHENTICATE "depends on no prior selection surviving", and state that AUTHENTICATE needs the USIM ADF selected and its GET RESPONSE on the same channel, which is why `authenticate_usim` runs both in one transaction. Correct `gsm-sip-bridge/src/modules/usim.rs:281`'s `authenticate` doc the same way (it stays correct for the default; add the pointer).
- [X] T024 [US3] Commit 2. Run `make format`, `make lint`, `make test`, then commit `fix(pcsc): select the USIM and authenticate in one PC/SC transaction (#232)`. The body covers the latent wrong-selection and GET RESPONSE hazard, says modem lines are unchanged, and says the AID is discovered once inside the identifying probe. End it with the `Co-Authored-By` line.

**Checkpoint**: all stories complete.

---

## Phase 6: Polish & Cross-Cutting Concerns

- [X] T025 [P] Add one checklist line to `docs/greptile-review-learnings.md` Part 1, "Concurrency & shared devices": the `pcsc` crate's `Card` resets the card when dropped (`SCARD_RESET_CARD`), breaking every other program's session on that SIM. Every PC/SC connection goes through `SharedCard` (`modules::pcsc_card`), never a bare `Card` (#232).
- [X] T026 [P] Add a bullet to the top of `## Unreleased` in `RELEASE_NOTES.md`, bolding only the summary:
  - Summary: `**A card-reader line no longer breaks its own SIM answers**`.
  - Text: every time the bridge finished with the card in a PC/SC reader (a registration, a challenge answer, the start-up MCC/MNC read, `pcsc-list`, or another card-reader line looking for its own card) it reset the card, so whatever else was using it failed with "The smart card has been reset". On a refresh that made the line drop a registration that still held and register in full. The bridge now leaves the card as it found it, and answers a challenge in one exclusive exchange with the card. Modem-backed lines are unchanged.
  - Verification: verified with a two-process reproduction on a real reader. A challenged refresh is covered by tests only; the local carrier does not challenge refreshes. (#232; the rest of #232 is #233.)
  - Write in the repo's release-note voice. No identifiers.
- [X] T027 Run quickstart §2 (all hardware-gated tests) and §3 (scenarios B, E and the reader listing, 10 runs each, with the fixed binary at `target-161/debug/gsm-sip-bridge`). Record the counts in the PR description: expect 10/10, against the 0/10 baseline for B/E.
- [X] T028 Run quickstart §4. Build an image from `git archive HEAD` (not the working tree; `target-100/` is not dockerignored), point `tmp/pr228-reader/docker-compose.yaml` at it, `docker compose up -d`, and check `/tmp/ims-agent-0.out` for `registered` in about 5 s and no `smart card has been reset` (SC-004). Restore the compose file's image tag afterwards.
- [X] T029 Self-review the whole diff against the CLAUDE.md checklist and `docs/greptile-review-learnings.md` Part 1:
  - `grep -rn 'SharedCard\|authenticate_usim\|OpenedCard' gsm-sip-bridge/src`: every reader is accounted for in plan.md's Blast Radius table.
  - `grep -rn 'contains(\|starts_with(' gsm-sip-bridge/src/ims`: no classifier depends on changed wording.
  - Every guard or `take()` (`SharedCard::drop`): nothing fails after it.
  - No real identifiers anywhere in the diff.
  - Tick each task; record any narrowed requirement as a **Deviation** line in this file, never as done.
- [X] T030 Commit 3 (T025, T026, any doc fixes from T029), after `make format`, `make lint` and `make test`: `docs: record the PC/SC reset rule and release note (#232)`. End it with the `Co-Authored-By` line.
- [X] T031 Push with an explicit refspec (`git push -u origin 161-pcsc-leave-card:161-pcsc-leave-card`; never rely on tracking, see the memory notes), then open the PR with `gh pr create`:
  - Title: `fix(pcsc): stop resetting a shared card-reader SIM (#232)`.
  - Body: the summary, the repro table (before/after), the test counts, the hardware-test and live results from T027/T028, the out-of-scope pointer to #233, and `Refs #232` (not `Fixes`; #233 remains). End it with the PR attribution line.

---

## Dependencies & Execution Order

- **Phase 1 → Phase 2 → US1 → US2 → commit 1 (T012) → US3 → commit 2 (T024) → Polish → commit 3 → PR.**
- US1 and US2 both depend on T002. US2's code is the same loop T007 touches, so it runs after US1 rather than in parallel.
- US3 depends only on Phase 1 in principle: it touches `usim.rs` and `ims/mod.rs`, and in `pcsc_card.rs` the `aid` field and override. It is sequenced after commit 1 so each commit stays atomic and green.
- Inside US3: T013–T015 (tests, all [P]) → T016 → T017 → T018 → T019 → T020 → T021 → T022 → T023 → T024.
- T025 and T026 are [P] with each other and with T027/T028.

## Parallel Example: User Story 3

```text
T013  usim.rs                 select_and_authenticate order test
T014  usim.rs                 select failure stops before AUTHENTICATE   (same file as T013: write together)
T015  ims/registration_auth.rs run_aka routes through the hook
```

## Implementation Strategy

- **MVP = Phase 2 + US1 + US2 (commit 1).** This alone removes every reset the bridge causes, which is the observed root cause of #232. It is shippable on its own.
- **Increment = US3 (commit 2).** It hardens the challenge answer against programs outside the bridge and fixes the latent selection hazard.
- **Polish (commit 3) and PR.** `greploop` and `/code-review` run only when the maintainer asks.

## Notes

- Hardware-gated tests share one physical card: always `--test-threads=1`.
- `tmp/` is scratch and never committed. The repro tool stays there.
- If a hardware test can't run (no reader), say so in the PR. Do not tick T027/T028.

## Deviations (recorded during implementation)

None of these narrows a requirement. They correct details the tasks got wrong or make a test stronger.

- **T013**: the SELECT MF the code sends is `00A4000C023F00` (`P2=0x0C`, per `usim.rs`'s module doc), not `00A40004023F00`. The test pins what the code sends. The recording transport is a new `RecordingTransport` next to `QueuedTransport`, so existing tests keep their constructor.
- **T015**: the test lives in `registration_auth.rs`'s `mod tests` next to `Probe` (where `Probe` actually is), not in `mod scripted`.
- **T016**: inside the provided method, `Self` is `?Sized`, so it cannot be cast to `&mut dyn ApduTransport`. `usim::authenticate` is generic over `T: ApduTransport + ?Sized` instead. Every existing `&mut dyn` caller compiles unchanged, and the trait stays object-safe.
- **T022**: the IMSI is read with `PcscTransport::read_transport_identity` (the same discover/select/read sequence as `pcsc_list`'s private `read_card_identity`) inside a `SharedCard` transaction. The holder is opened **before** the AKA and kept across it, not opened afterwards: a connection opened after a reset cannot see that reset, so only a holder held throughout tests the claim.

## Review follow-up (PR #234)

- **Greptile P2 "Tests miss lost exclusive access"**: no test proved that the AKA window excludes other programs; removing `with_transaction` from the override still passed. The same gap existed at every PC/SC transaction site, so all three got a test. A `Competitor` thread on its own connection sends `SELECT MF` with no transaction, 50 ms apart from each of 20 rounds:
  - `authenticate_usim_window_excludes_a_competing_session`: every contended AKA gets the same answer as the uncontended one.
  - `connect_probe_excludes_a_competing_session`: every probe finds the card and the same AID.
  - `pcsc_list_read_excludes_a_competing_session`: every listing reads the card's IMSI.
  - Mutation check on the OMNIKEY 3x21: removing each site's transaction fails its own test 20/20 (AKA `SW=6985` instead of the MAC failure `9862`; the probe and listing fail to find the USIM). The other two tests stay green.
  - `with_transaction`'s other callers (the agent's and `vowifi-plmn`'s PLMN read) share the method the AKA test covers.
- The old `authenticate_usim_runs_in_one_window_and_leaves_the_card` is renamed to `authenticate_usim_reaches_the_card_and_leaves_it`, because it never proved the window.
