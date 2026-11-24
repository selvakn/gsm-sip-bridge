# Research: Stop the Bridge Resetting a Shared Card-Reader SIM

All findings below were measured on the local host: Ubuntu 26.04, host pcscd (pcsc-lite 2.4.1), HID OMNIKEY 3x21, Vodafone SIM. The tool is the two-process repro in `tmp/issue232-repro` (gitignored scratch, not shipped). `pcsc` crate 2.9.0, as locked in `Cargo.lock`.

## Measured facts

| # | Scenario (one program holds a shared connection, then sends a command) | Result |
|---|---|---|
| A | nobody else touches the card | ok |
| B | `gsm-sip-bridge vowifi-plmn --pcsc-imsi <this card>` runs meanwhile | **`The smart card has been reset, so any shared state information is invalid`** |
| C | another program connects and disconnects with `LeaveCard` | ok |
| D | another program connects and drops its `Card` (crate default) | **reset** |
| E | `vowifi-plmn --pcsc-imsi 001010000000001` (no such card) runs meanwhile | **reset** (the probe rejected this card, then dropped it) |
| G | holder inside a transaction; `vowifi-plmn` runs meanwhile | ok; `vowifi-plmn` waited until the transaction ended |
| I | holder inside a transaction; another program issues 8 `SCardReconnect(RESET)` | ok |
| J | holder inside a transaction; another program connects and transmits with no transaction | ok for both; the other program's connect waited ~2.5 s for the transaction to end |
| K | another program exits without disconnecting | ok (pcscd's cleanup leaves the card) |
| L | another program is SIGKILLed while connected | ok |

Code facts:

- `pcsc-2.9.0/src/lib.rs:1668`: `impl Drop for Card` calls `SCardDisconnect(handle, Disposition::ResetCard)`, hard-coded. `Card::disconnect(self, Disposition)` (`:1261`) is the only way to choose a different disposition. On failure it returns `Err((Card, Error))`, handing the card back.
- `pcsc-2.9.0/src/lib.rs:1732`: `impl Drop for Transaction` ends with `LeaveCard`. A panic inside a transaction is already safe.
- `Card` holds a `Context` clone (`:659`). `std::mem::forget(card)` therefore leaks one context reference until process exit, and nothing else.
- charon's `libstrongswan-eap-sim-pcsc.so` (from `gsm-sip-bridge:pr228b`) imports `SCardBeginTransaction`/`SCardEndTransaction`. All four `SCardDisconnect` calls pass `esi=0` (`SCARD_LEAVE_CARD`).
- The bridge creates PC/SC `Card`s in exactly two places: `src/modules/pcsc_card.rs:77` and `src/modules/pcsc_list.rs:147`. `src/supervise/runner.rs` holds only a `Context`.
- `open_card` (`src/ims/mod.rs:1137`) runs `discover_usim_aid` + `select_usim` with no transaction. `run_aka` (`:1163`) then sends AUTHENTICATE and, on `61xx`, a GET RESPONSE, also with no transaction. AUTHENTICATE (TS 31.102 §7.1.2.1) is addressed to the selected USIM ADF, and GET RESPONSE must follow on the same channel. Both are broken by an interleaved SELECT or a reset. The comment at `src/modules/pcsc_card.rs:143-145` says otherwise, and is wrong.

## Decisions

### D1: A `SharedCard` handle that always leaves the card

- **Decision**: add `pub(crate) struct SharedCard(Option<Card>)` in `src/modules/pcsc_card.rs`.
  - `SharedCard::connect(&Context, &CStr) -> Result<SharedCard, pcsc::Error>` keeps the raw `pcsc::Error`, so `pcsc_list` can still match `NoSmartcard`.
  - It implements `Deref`/`DerefMut<Target = Card>`.
  - `Drop` takes the card and calls `disconnect(Disposition::LeaveCard)`. If that fails, it `std::mem::forget`s the returned `Card`, so the crate's resetting `Drop` never runs.
  - Both `ctx.connect` sites go through it.
- **Rationale**: one owner of the disposition, enforced by type. Every exit path is covered, including early returns, `?`, panics, and readers rejected inside `connect()`'s loop, because they are all drops. Forgetting after a failed disconnect is safe: the handle is already unusable, and scenarios K/L show pcscd's cleanup leaves the card.
- **Alternatives rejected**:
  - Calling `disconnect(LeaveCard)` by hand at each site misses `?` and early-return paths, which is exactly the latching and exit-path class in the learnings doc.
  - Vendoring or patching the `pcsc` crate is a fork to maintain for a one-line behaviour, and dependabot bumps would silently undo it.
  - `ManuallyDrop<Card>` is equivalent but needs `unsafe` or a take dance. `Option` is plain safe code, and `make lint` runs an unsafe audit.

### D2: Answer the challenge through one transport hook

- **Decision**: add a provided method to `ApduTransport` (`src/modules/usim.rs`):
  `fn authenticate_usim(&mut self, rand: &[u8; 16], autn: &[u8; 16]) -> BridgeResult<AkaResult> { authenticate(self, rand, autn) }`.
  - `run_aka` calls `card.authenticate_usim(..)` instead of `usim::authenticate(card, ..)`.
  - `PcscTransport` overrides it: `with_transaction(|t| select_and_authenticate(t, &self.aid, rand, autn))`, where `select_and_authenticate` is `select_usim(t, aid)` followed by `authenticate(t, rand, autn)`.
- **Rationale**:
  - The default keeps every other transport (`AtCommander` and the test SIMs) byte-for-byte as before (FR-005, SC-003).
  - The override is the only place a card-reader line authenticates, so resyncs (each a new `run_aka` call) get their own window for free (FR-004).
  - GET RESPONSE chaining sits inside `authenticate`, so it is inside the window too.
- **Alternatives rejected**:
  - A generic `fn exclusive<T>(&mut self, f: impl FnOnce(&mut dyn ApduTransport) -> T)` is not object-safe, and `OpenedCard` holds `Box<dyn ApduTransport>`.
  - Downcasting to `PcscTransport` in `run_aka` would leak the transport type into IMS code.
  - A wrapper transport opened per AKA would add another layer for one call site (Constitution V).

### D3: Wording of new failure points

- **Decision**: failing to start the window reuses the existing `PC/SC transaction begin failed: {e}` wording (`src/modules/pcsc_card.rs:154`, `src/modules/pcsc_list.rs:183`). Transmit failures keep `PC/SC transmit failed: {e}`.
- **Rationale**: FR-006. No classifier in `src/ims`, `src/supervise` or `src/vowifi` matches either string (grep in plan.md). The only SIM classifier, `sim_recovery::has_csim_failure`, matches `AT+CSIM failed` on modem lines.

### D4: Discover the USIM application once, at open, inside the identifying window

- **Decision**:
  - `PcscTransport::connect` already runs `discover_usim_aid`/`select_usim`/`read_imsi` inside one transaction for each candidate. That probe now returns the AID as well as the IMSI, and the matching reader's AID is kept in `PcscTransport { card: SharedCard, aid: Vec<u8> }`.
  - `open_card` stops running its own `discover_usim_aid`/`select_usim` for a card-reader line. Modem lines keep them, unchanged.
- **Rationale**:
  - Removes the last multi-APDU sequence on a reader line that ran outside a transaction (FR-003, last sentence).
  - Saves about six APDUs per open. Over USB/IP that is a measurable part of #232's ~1.3 s connect-to-answer gap.
  - The application list does not change while the card stays in the reader. A swapped card fails the next command with "removed card", whatever we cached.
- **Alternatives rejected**: rediscovering inside every AKA window is more APDUs while other programs wait, and protects nothing extra.

### D5: Tests

- **Unit (CI)**:
  - `select_and_authenticate` against a recording `QueuedTransport`: the order is SELECT MF, SELECT AID, AUTHENTICATE, GET RESPONSE on `61xx`. It fails on a SELECT error without sending AUTHENTICATE.
  - `run_aka` routes through `authenticate_usim`: a test transport that overrides the hook and records the call, so nobody can quietly call `usim::authenticate` directly again.
  - Existing modem and scripted-SIM registration tests run unchanged (SC-003).
- **Hardware-gated (`#[ignore = "needs a PC/SC reader with a SIM; run with --ignored on the reader host"]`, the precedent is `src/ims/gm_ipsec.rs:1264`)**, real pcscd, no mocks:
  1. Dropping a `SharedCard` leaves another handle's next transmit working.
  2. `PcscTransport::connect("001010000000001")` fails with no match and leaves another handle working.
  3. A raw `Card` dropped the crate's way *does* break the other handle. This is the control, proving test 1 can detect the bug.
  4. `PcscTransport::authenticate_usim`, with a malformed challenge, gets a card error inside the window. This checks the window starts and ends and leaves another handle working. A real RAND/AUTN cannot be produced locally.
- **Live (quickstart.md)**: repro scenarios B and E with the fixed binary as the other program, 10 runs each (SC-001). A full card-reader registration on the `tmp/pr228-reader` rig (SC-004).
- **Mock justification (Constitution I)**: the CI tests use scripted transports because CI has no pcscd or reader. The hardware-gated tests cover the real component on the reader host.

### D6: Docs and repo rules

- **Decision**:
  - Correct the comments: `pcsc_card.rs:139-145` (`with_transaction`), `ims/mod.rs:1081/1089` (`OpenedCard`/`open_card`), `usim.rs:281` (`authenticate`).
  - Add one line to `docs/greptile-review-learnings.md` Part 1, "Concurrency & shared devices": the `pcsc` crate's `Card` drop resets the card, so always release through `SharedCard`.
  - Add a `## Unreleased` bullet to `RELEASE_NOTES.md`, following the `.cursor/rules/release-notes.mdc` rule (learnings Part 1 predates that rule). Bold only the summary.
