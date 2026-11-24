# Implementation Plan: Stop the Bridge Resetting a Shared Card-Reader SIM

**Branch**: `161-pcsc-leave-card` | **Date**: 2026-11-24 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `specs/161-pcsc-leave-card/spec.md` (issue #232, PR 1 of 2; the resilience half is #233)

## Summary

- **The bug**: the `pcsc` crate's `Card` resets the card when dropped. Every PC/SC connection the bridge releases therefore breaks every other program's session on that SIM, which is the root cause of #232, reproduced on the real reader.
- **Fix 1**: release through a `SharedCard` wrapper that disconnects with `LeaveCard` on every path (D1).
- **Fix 2**: on a card-reader line, run USIM selection and AUTHENTICATE as one PC/SC transaction, through a provided `ApduTransport::authenticate_usim` hook that `PcscTransport` overrides (D2). The AID comes from the probe that already identifies the card (D4).
- **Unchanged**: modem lines, and every other transport. They keep the hook's default, which is today's code.

## Technical Context

**Language/Version**: Rust (workspace toolchain, `rust-toolchain.toml`)  
**Primary Dependencies**: `pcsc` 2.9.0 (pcsc-lite client). No new dependencies.  
**Storage**: N/A  
**Testing**: `make test` (nextest). Hardware-gated `#[ignore]` tests on the reader host (`cargo test -p gsm-sip-bridge pcsc -- --ignored`). Live repro in `tmp/issue232-repro`.  
**Target Platform**: Linux (container and native), pcsc-lite 2.4.x  
**Project Type**: single binary crate in a workspace (`gsm-sip-bridge/`)  
**Performance Goals**: the challenge-answer window lasts only as long as the card's answer (4 APDUs). One open saves about 6 APDUs.  
**Constraints**: modem lines unchanged byte-for-byte. No `unsafe` (`make lint` audits it). Error wording kept (FR-006).  
**Scale/Scope**: 2 `Card` creation sites, 1 trait, 1 AKA call site, 3 comment corrections. About 150 lines of code plus tests.

## Constitution Check

| Principle | Status |
|---|---|
| I. Integration-first testing | Pass, with justification. CI has no pcscd or reader, so CI tests use scripted transports, justified at the mock site. The real component is exercised by hardware-gated `#[ignore]` tests on the reader host and by the live repro. |
| II. Green-on-commit | Pass: `make format`, `make lint` and `make test` before every commit (CLAUDE.md). |
| III. Atomic commits | Pass: three commits, (1) `SharedCard`, (2) AKA window plus AID cache, (3) docs and release note. |
| IV. Makefile-driven | Pass: no new targets needed. Hardware tests are a documented `cargo test -- --ignored` invocation, like the existing `gm_ipsec` one. |
| V. Simplicity | Pass: a safe newtype and one provided trait method. No new layers. |

Post-design re-check: unchanged, pass.

## Blast Radius

### State and invariants inventory

Grep output (2026-11-24, `gsm-sip-bridge/src`):

```
$ grep -rn 'ctx.connect(' src
src/modules/pcsc_card.rs:77:            let mut card = match ctx.connect(&reader_cstr, ShareMode::Shared, Protocols::ANY) {
src/modules/pcsc_list.rs:147:        let mut card = match ctx.connect(&reader_cstr, ShareMode::Shared, Protocols::ANY) {

$ grep -rn 'impl.*ApduTransport for\|pub trait ApduTransport' src
src/ims/registration_auth.rs:168:    impl ApduTransport for Sim {
src/ims/registration_auth.rs:328:        impl ApduTransport for Probe {
src/modules/pcsc_list.rs:299:    impl ApduTransport for QueuedCard {
src/modules/usim.rs:23:pub trait ApduTransport {
src/modules/usim.rs:27:impl ApduTransport for AtCommander {
src/modules/usim.rs:464:    impl ApduTransport for QueuedTransport {
src/modules/pcsc_card.rs:160:impl ApduTransport for PcscTransport {
src/modules/pcsc_card.rs:176:impl ApduTransport for Transaction<'_> {
src/vowifi/plmn.rs:255:    impl ApduTransport for QueuedCard {

$ grep -rn 'run_aka(\|usim::authenticate(\|discover_usim_aid(\|select_usim(' src
src/ims/mod.rs:1137:    let aid = usim::discover_usim_aid(card.as_mut())?;
src/ims/mod.rs:1138:    usim::select_usim(card.as_mut(), &aid)?;
src/ims/mod.rs:1163:    usim::authenticate(card, &rand_arr, &autn_arr)
src/ims/mod.rs:1300:        let aka = run_aka(card.as_mut(), &challenge)?;
src/modules/pcsc_card.rs:130:        let aid = usim::discover_usim_aid(t)?;
src/modules/pcsc_card.rs:131:        usim::select_usim(t, &aid)?;
src/modules/pcsc_list.rs:69:    let aid = usim::discover_usim_aid(card)?;
src/modules/pcsc_list.rs:70:    usim::select_usim(card, &aid)?;
src/ims/registration_auth.rs:137:    match run_aka(card, challenge)? {
src/vowifi/plmn.rs:70:    let aid = usim::discover_usim_aid(card)?;
src/vowifi/plmn.rs:71:    usim::select_usim(card, &aid)?;
src/vowifi/usim_bridge.rs:383:            match usim::discover_usim_aid(serial) {

$ grep -rn 'OpenedCard' src   (non-comment)
src/ims/mod.rs:1083 (definition), :1091 open_card, :1141, :1167 register_session
src/ims/registration_auth.rs:66, :114, :118 (answer), :209/:214, :316 (test SIMs)
src/ims/agent/registration_refresh.rs:1089 (OpenCard type)

$ grep -rn 'with_transaction' src   (callers)
src/ims/agent/mod.rs:292 (agent startup PLMN), src/vowifi/plmn.rs:120 (vowifi-plmn CLI)
```

| Changed item | Readers | Copies made at construction | Invariant comments | Decision |
|---|---|---|---|---|
| `PcscTransport.card: Card` becomes `SharedCard` | `transmit_apdu` (`pcsc_card.rs:160`), `with_transaction` (`:150`), `connect` (`:77`) | none; `PcscTransport` is moved into `Box<dyn ApduTransport>` once | `pcsc_card.rs:139-145` ("Single-APDU work… does not need this"): **wrong, corrected** | update at construction; `Deref` keeps call sites unchanged |
| Candidate cards rejected inside `connect()` | the loop only | none | `pcsc_card.rs:84-94` (why the probe uses a transaction): still holds | released by `SharedCard` drop, leaving the card |
| `pcsc_list` card (`pcsc_list.rs:147`) | `read_card_identity` in a transaction | none | `:174-177`: still holds | `SharedCard::connect`; keeps matching `pcsc::Error::NoSmartcard` |
| New `PcscTransport.aid` | `authenticate_usim` override | none | none | set once from the matching probe; never changes (a swapped card fails the next APDU anyway) |
| `open_card` stops selecting on reader lines | `register_session` (`ims/mod.rs:1300` via `run_aka`), `registration_auth::answer` (`:118`, `:137` via `run_aka`) | `OpenedCard` destructured by both | `ims/mod.rs:1081` "opened and with its USIM application selected", `:1089` "and selects the USIM": **corrected**: selected for modem lines; a reader line selects inside each AKA window | no reader of `OpenedCard.card` needs the selection except AKA, which now selects itself |
| `run_aka` calls `card.authenticate_usim` | both callers above | none | `usim.rs:281` "against the currently-selected USIM ADF": kept for the default, plus a note that `PcscTransport` selects inside its window | the default is the old body, so modem and test transports are byte-identical |
| `ApduTransport` gains a provided method | 9 impls listed above | none | `usim.rs:17-22` trait doc: extended | the default needs no change in any impl |

**Implicit contracts**:

- **Error text**: `PC/SC transmit failed: …` and `PC/SC transaction begin failed: …` are kept. `grep -rn 'contains(\|starts_with(' src/ims src/supervise src/vowifi | grep -i 'pc/sc\|smart card\|transaction\|csim\|authenticate'` finds only `sim_recovery.rs:121` (`AT+CSIM failed`, modem) and `runner.rs:993` (a test of its own context message). No classifier depends on the wording.
- **Limits**: `MAX_RESYNC_ATTEMPTS` (`ims/mod.rs:89`) and `MAX_RESYNCS` (`registration_refresh.rs`) are unchanged. Each resync is a new `run_aka` call and so a new window (FR-004).
- **Ordering**: GET RESPONSE after `61xx` stays inside `authenticate`, and therefore inside the window. Lock order: one PC/SC transaction at a time per handle; no nested locks.
- **Not changed, deliberately**: `vowifi/usim_bridge.rs:383` is the vpcd *card side* for modem lines, over `AT+CSIM`, not a PC/SC client. `supervise/runner.rs` holds only a `Context` and creates no `Card`.

### Concurrency and ownership map

- **`SharedCard`**:
  - Owned by `PcscTransport`, owned by `OpenedCard.card` (`Box<dyn ApduTransport>`).
  - In `register_session` it lives for the function, then drops, leaving the card.
  - In a re-auth worker (`registration_auth::answer`) it lives on the worker thread. That thread always runs to completion and drops it, even when the dispatch loop has abandoned the attempt (`cancelled` only discards the result).
  - Agent startup PLMN read, `vowifi-plmn` and `pcsc-list` drop it at the end of scope.
  - Panic: the unwinding drop leaves the card. Process kill or abort: pcscd's cleanup leaves the card (research K/L).
  - Failed disconnect: `forget`, leaking one `Context` reference until exit.
- **PC/SC transaction (the AKA window)**:
  - Borrowed inside `with_transaction` and ended explicitly with `LeaveCard`. On panic, `Transaction::drop` also ends with `LeaveCard`.
  - Never held across a network round trip: the window covers 4 APDUs, and `register_session`'s SIP exchange happens outside it.
  - Other programs block, they don't fail, for the window's length (research G/I/J).
- **Threads**: none new. **Sockets / kernel objects**: none.

### Where else does this logic run?

- **AKA**: initial registration (`register_session`, which also serves full renewals and the `ims-register`/`ims-call`/`volte-*` CLIs, all through `open_card`) and background re-authentication (`registration_auth::answer`). Both reach the card only through `run_aka`, the single entry point that changes.
- **Card connections**: every reader-line PC/SC user goes through `PcscTransport::connect` or `pcsc_list`, the two sites. No copy of the logic exists elsewhere.
- **Other lines**: a sibling line's `connect()` probe is fixed by the same `SharedCard`.

### Situations checklist

| Situation | Answer |
|---|---|
| idle | unchanged flow; card released without a reset |
| in a call / ringing / origination pending | re-auth runs on the worker thread as today; the window blocks only other card users, never the dispatch loop |
| UDP and TCP | N/A (no signalling change) |
| error and rollback | window ended via `end(LeaveCard)` or `Transaction::drop`; card released via `SharedCard` drop |
| cancel (abandoned re-auth) | worker finishes and drops the card, leaving it; the result is discarded as today |
| shutdown | process exit; pcscd cleanup leaves the card (verified) |
| limits / off-by-one | no limits touched; resync counts unchanged |
| blocks the dispatch loop | no: AKA was already off-loop (worker) or in the already-blocking `register_session` |
| docs / release notes / repo rules | comments corrected (D6); learnings line; `## Unreleased` release-note bullet per `.cursor/rules/release-notes.mdc`; no identifiers in tests (synthetic IMSI `001010000000001`) |

## Project Structure

### Documentation (this feature)

```text
specs/161-pcsc-leave-card/
├── spec.md
├── plan.md              # this file
├── research.md          # measured facts and decisions D1–D6
├── data-model.md        # SharedCard / PcscTransport / hook shapes
├── quickstart.md        # hardware-gated tests + live repro (SC-001, SC-004)
├── checklists/requirements.md
└── tasks.md             # /speckit-tasks
```

No `contracts/`: the change is internal. No CLI, config, wire or API surface changes.

### Source Code (repository root)

```text
gsm-sip-bridge/src/
├── modules/
│   ├── pcsc_card.rs     # SharedCard; PcscTransport { card, aid }; authenticate_usim override;
│   │                    # select_and_authenticate; hardware-gated tests
│   ├── pcsc_list.rs     # SharedCard::connect at the listing site
│   └── usim.rs          # ApduTransport::authenticate_usim provided method; select_and_authenticate tests
└── ims/
    └── mod.rs           # open_card: no open-time selection on reader lines; run_aka → authenticate_usim
docs/greptile-review-learnings.md   # one checklist line
RELEASE_NOTES.md                    # Unreleased bullet
```

**Structure Decision**: existing single-crate layout. Changes stay inside the PC/SC transport module, the USIM helper trait and the one IMS AKA call site.

## Complexity Tracking

No violations.
