# Research: A Card-Reader Line Rides Out an Unreachable SIM

Decisions for #233, grounded in the code on `main` at `79c0a83` (after PR #234). Paths are relative to `gsm-sip-bridge/src`.

## Facts the design rests on

| # | Fact | Where |
|---|---|---|
| F1 | Every SIM-worker error, and the 600 s overdue case, goes to `fall_back_to_full`, which sets `force_renewal`. No retry, and no regard for remaining lifetime. | `ims/agent/registration_refresh.rs:1203-1229`, `:1435-1442` |
| F2 | The worker opens the card itself (`open(&job.cfg)`) and runs one AKA. `open` is `FnOnce` today. | `ims/registration_auth.rs:111-123` |
| F3 | Card errors travel from `PcscTransport` to both callers with `?` only, with no re-wrapping: `open_card` → `answer` → channel → `drive_challenge`; `register_session` → `register_checked` → `RegisterFailure::from`. A typed variant survives both paths. | `ims/mod.rs:1096-1150`, `:1176-1181`, `:1311`; `ims/session.rs:342-350`, `:362-366` |
| F4 | All PC/SC failures are built in 5 `map_err` sites in `PcscTransport`, plus two final `connect` errors. | `modules/pcsc_card.rs:112`, `:117`, `:123`, `:186`, `:220`, `:234`, `:263` |
| F5 | A full renewal's failure goes `on_renewal_failure` → `cause_of` → `None` (a card error is `NotPcscf`) → `schedule_renewal_retry` → `count_renewal_failure`, which returns `Err` (exit) at 6. | `ims/agent/mod.rs:4485-4505`, `:2510-2520`, `:2483-2497`; `ims/agent/pcscf_pool.rs:89-102` |
| F6 | The refresh is driven from exactly two places, through one function: the loop's `begin_pass`, and the ring wait's `RingRefresh::tick`. | `ims/agent/mod.rs:2756`; `registration_refresh.rs:639-647`, `:704-719` |
| F7 | `on_idle_tick` leaves a due renewal to the in-place path whenever `in_place_refresh_possible` holds (no `force_renewal`, no `refresh_fallback`, cached auth, not lapsed or busy). It only clears `registration_refresh` and `challenged` before a full renewal. | `ims/agent/mod.rs:4296-4326`; `registration_refresh.rs:689-699` |
| F8 | No code matches on `BridgeError` variants exhaustively. The only `match` is a test of `Io`. Adding a variant breaks nothing. | `error.rs:75` |
| F9 | Text classifiers that see these errors: `map_registration_error` (`auth`/`aka`/`challenge` → AuthFailed, `timeout`/`timed out` → Timeout, else Rejected) and `classify_register_failure` (`network is unreachable`, `connect to … failed`, `timed out waiting for a sip response`, …). None matches PC/SC wording, so today a PC/SC failure reports `Rejected` / `NotPcscf`. | `ims/session.rs:39-48`; `ims/mod.rs:244-264` |
| F10 | The full renewal's card access is synchronous on the dispatch thread, under the renewal watchdog phase (360 s budget). | `ims/agent/mod.rs:4344`, `ims/agent/watchdog.rs:207` |
| F11 | A PC/SC transaction serialises the card for every client, and a command that hangs inside it holds everyone off (measured for #232: other clients wait rather than fail). | `specs/161-pcsc-leave-card/research.md` G/I/J |
| F12 | `vowifi-status` prints `last_refresh: {result} {time}` verbatim from `LoopState::last_refresh`. Values today: `ok`, `rejected <status>`, `timeout`, `error`. `docs/observability.md:173` lists the first three only. | `vowifi/mod.rs:2800`; `ims/agent/mod.rs:2881` |

## D1: A typed `BridgeError::CardUnreachable(String)`

**Decision**: add a variant, `#[error("IMS error: SIM card unreachable: {0}")] CardUnreachable(String)`, and `BridgeError::is_card_unreachable()`. Add one helper in `modules/pcsc_card.rs`, `pcsc_failure(context, pcsc::Error) -> BridgeError`. It returns `CardUnreachable(format!("{context}: {e}"))` for the variants below, and `Ims(...)` with the same text for every other variant. All 5 `map_err` sites use the helper. The two final `connect` errors ("no real PC/SC reader found", "no PC/SC reader has a card matching IMSI …") become `CardUnreachable`.

| Spec condition (FR-001) | `pcsc::Error` variants |
|---|---|
| card reset / removed / absent | `ResetCard`, `RemovedCard`, `NoSmartcard` |
| card unpowered / unresponsive | `UnpoweredCard`, `UnresponsiveCard` |
| reader unavailable (gone, renamed, link lost) | `ReaderUnavailable`, `UnknownReader`, `CommError`, `CommDataLost` |
| no readers | `NoReadersAvailable` |
| smart-card service stopped / unavailable / restarted | `NoService`, `ServiceStopped`, `Shutdown`, `InvalidHandle` (a handle from before a pcscd restart) |
| another program holds the card exclusively | `SharingViolation` |
| no reader holds this IMSI | the final `connect` error |

Everything else (`Timeout`, `InvalidParameter`, `ProtoMismatch`, `NotTransacted`, …) stays `Ims`: a bug or a protocol problem, not an outage. A card's own answer (`AUTHENTICATE failed: SW=…`, MAC failure, AUTS) is built in `modules/usim.rs` and is never touched.

**Wording check (FR-003)**: the new prefix `IMS error: SIM card unreachable: ` plus the existing pcsc texts contain none of `auth`, `aka`, `challenge`, `timeout`, `timed out`, `network is unreachable`, `connect to `, `send failed`, `recv failed` or `connection closed`. So `map_registration_error` still says `Rejected` and `classify_register_failure` still says `NotPcscf` for every condition, as today. A test pins this, comparing the old `Ims` text with the new typed text for every mapped variant. The `PC/SC transmit failed:` and `PC/SC transaction begin failed:` wording inside is kept.

**Alternatives rejected**:
- Match on the message (`contains("smart card has been reset")`). Rejected: that is exactly the fragile classifier the issue rules out.
- Carry the `pcsc::Error` in the variant. Rejected: it ties `error.rs` to the `pcsc` crate for no gain, because only the category is acted on.
- A wrapper type inside `Ims`. Rejected: callers would need downcasting.

`pcsc_list` builds its own errors for a table and is not on any registration path, so it is left alone. `vowifi-plmn` and the agent's start-up PLMN read go through `PcscTransport`, so their message for an unreachable card gains the `SIM card unreachable:` prefix. Nothing parses those (`grep -rn 'contains(\|starts_with(' src/supervise src/vowifi | grep -i 'pc/sc\|smart card'` finds only `runner.rs:993`, a test of its own context string).

## D2: Retry the same challenge inside the worker

**Decision**: on a card-reader line, `registration_auth::answer` loops. It opens the card and runs the AKA. If the result is `CardUnreachable`, the worker is not cancelled, and another interval still fits before the window's end, it logs `the SIM is out of reach; asking it again` (attempt n), sleeps `retry_interval`, and goes again. Otherwise it returns the result. A modem line runs exactly one attempt, as today.

- The window (30 s) and interval (2 s) come from a `CardTiming` held in `ReauthEnv`, so tests use milliseconds (FR-022). Parsing the challenge happens once, before the loop.
- `open` becomes `Fn` (it is already an `Arc<dyn Fn>` in `ReauthEnv`).
- **Loop side**: `Reauthentication` carries its own deadline. For a card-reader line it is `retry_window + attempt_bound` (30 + 15 s). For a modem line it stays `WORK_TIMEOUT` (600 s). A worker that is still inside a card call at the deadline is abandoned (D3).
- **Why it is safe**: pcscd reports a reset or removal before the command reaches the card. If the card did run the AKA and the link dropped before the answer came back, the card's SQN has moved on, and the same RAND/AUTN gives a sync failure. That goes to the existing resync path and its `MAX_RESYNCS` limit (F1, `registration_refresh.rs:1230-1256`). A resync's fresh challenge is a new `Reauthentication`, so it gets its own window (FR-008).
- **Why 30 s**: in #232 the card was back after about 2 s. The S-CSCF waits for the answer on its reg-await-auth timer, which is far longer than 30 s, as we read TS 24.229. This has not been measured on a carrier: no local carrier challenges refreshes. If the answer comes too late, the network's own rejection takes the existing path (a 401/403 to the answer goes to full registration).

**Alternatives rejected**:
- Retry from the dispatch loop by re-spawning workers. Rejected: more state on `LoopState` for the same behaviour, and the loop would need to re-parse.
- Ask the network again (a new in-place REGISTER) instead of the card. Rejected: it spends an authentication vector per attempt (the spec's Assumptions).

## D3: A hung attempt uses up the window; no parallel attempts

**Decision**: attempts are sequential on one worker thread. A command that hangs keeps that thread inside pcscd, and because of F11 any second attempt would block behind it in pcscd anyway. So the window simply runs out. The loop then abandons the worker (its `cancelled` flag is set, and its late result is dropped) and enters waiting-for-card (D4).

- The abandoned worker's `done` flag (set by a drop guard when the thread ends, including on panic) goes with the waiting state. No local check starts until it is set (FR-015).
- **Full registration**: its card access stays synchronous (F10). A card that hangs there is bounded by the existing renewal watchdog, as today. Making the full registration's AKA asynchronous is a different, larger change, and the issue does not ask for it. Recorded in the spec's Clarifications.

## D4: The waiting-for-card state

**Decision**: `LoopState.awaiting_card: Option<AwaitingCard>`, driven only from `drive_registration_refresh`, right after the `challenged` branch (F6). Both drivers, the loop and the ring wait, therefore run it, and calls are never held up. Each pass runs these steps in order:

1. **Something else took over**: `force_renewal || refresh_fallback`. Clear the state, and let that path run. This is the single exit check for every `force_renewal` setter: P-CSCF failover (`mod.rs:2539`), Gm liveness (`ping.rs:306`), qualifying endpoint failure (`mod.rs:4026`), reg-event deregistration (`mod.rs:2365`), `needs_full_registration` (`registration_refresh.rs:1052`), zero lifetime (`:913`), and swap failure (`:965`). New setters are covered without being listed.
2. **Margin or lapse, idle only** (D5): set `force_renewal`, clear the state, and log `registering in full: the card is still out and the registration expires soon` (or `has lapsed`).
3. **A check is outstanding**: poll it.
   - `Ok`: clear the state, log `the SIM is reachable again; refreshing in place`, and fall through to the ordinary "refresh due" code, which sends one fresh in-place REGISTER on this same pass.
   - `CardUnreachable`: schedule the next check.
   - Any other error, meaning the card answered but wrongly: clear the state and `fall_back_to_full`, today's handling for a card error.
   - Past its `check_bound` (15 s): abandon it, keep its `done` flag, and schedule the next check.
4. **No check outstanding**: start one if `now >= next_check_at` and the previous worker is done.

The check runs `reauth_env.open_card(cfg)` on a worker and drops the result at once. On a reader line that is `PcscTransport::connect`, which identifies the card by IMSI inside a transaction and releases it leaving the card (#234). It sends nothing over the network and runs no AKA.

- **Backoff**: the first check at `RETRY_INITIAL_BACKOFF` (5 s, the existing renewal constant), then `next_backoff` (the existing helper) doubling, capped at a new `CARD_CHECK_MAX_BACKOFF` (60 s). It has its own cap rather than `RETRY_MAX_BACKOFF` (120 s), so a card that comes back is noticed within a minute.
- **The refresh doesn't re-fire (FR-011)**: the early return in step 4 keeps the ordinary "refresh due" branch from running while the state exists. `on_idle_tick` still sees `in_place_refresh_possible == true`, so it leaves the renewal alone.
- **Clearing in the full-renewal block**: as defence in depth, the block that clears `registration_refresh` and `challenged` before a full renewal (`mod.rs:4324-4326`) also clears `awaiting_card`.
- **Shutdown**: `LoopState` drops, workers are cancelled and never joined, and their threads end on their own.

**Alternatives rejected**:
- A flag plus timers spread over `LoopState`. Rejected: an `Option<struct>` makes "every exit clears it" one assignment and keeps the latching-state class (learnings Part 1) to one place.
- Resending the in-place refresh on a backoff. Rejected in the spec.

## D5: The margin yields to a call

**Decision**: the safety margin is `REFRESH_FLOOR` (60 s, `ims/mod.rs:920`). It forces a full registration only when `!busy()`, the same predicate `in_place_refresh_possible` uses for a lapse. During a call the line keeps waiting. If the card comes back, the in-place refresh goes out even if the binding has lapsed, which the existing rule allows while busy (`registration_refresh.rs:697-698`). A full registration mid-call would be deferred by `MaintenancePolicy` anyway, and setting `force_renewal` would block the in-place refresh that can still save the binding.

## D6: Budget exemption at the full-renewal failure site

**Decision**: extract the `Err(failure)` tail of the full renewal (`mod.rs:4485-4505`) into `LoopState::on_full_renewal_failure(target, failure, routed, pcsc_reader)`.

- **Card-reader line with a `CardUnreachable` failure**: it calls a new `schedule_card_retry()`. That sets `next_renewal_attempt = now + backoff` and steps the ladder with `next_backoff`. It does **not** call `count_renewal_failure` or `on_renewal_failure`, so there is no budget count and no P-CSCF mark. It logs `the SIM is out of reach; retrying the registration without counting toward the restart budget`.
- **Anything else**: `on_renewal_failure`, exactly as today.

Status and observability before it stay as they are: `Failed`, `last_failure` (whose text now carries `SIM card unreachable`, FR-018), `set_registered(false)` and `set_tunnel_up(false)`. A later success clears the ladder through `on_renewal_success` as today. With interleaved failures, only the non-card ones count (FR-017). No cap (Clarification Q1).

`pre_renewal` hook failures and the modem-lock timeout are not card failures and keep `schedule_renewal_retry`.

## D7: Status and logs

- `last_refresh` becomes `awaiting-card` when the state is entered (a single token, like `timeout`). Any later outcome overwrites it.
- `next_refresh` is unchanged. It shows the refresh's due time, which is in the past while waiting, and it already does that during a backoff.
- `docs/observability.md:173` gains `error` and `awaiting-card`.
- **The five log lines (FR-020)**: retrying the challenge (`attempt`), giving up the challenge and waiting (`window_secs`), the card back (`waited_secs`), a full registration because of the margin or a lapse (`reason`), and the budget-exempt retry (`retry_in_secs`). They carry no IMSI. The existing `connected to PC/SC reader` / `read IMSI from SIM` lines still appear on each successful open, as today.

## D8: Test seams

- `CardTiming` in `ReauthEnv`, defaulting to production values, plus a `CardTiming::fast()` test helper in milliseconds.
- `registration_auth::scripted::card_after(failures, replies)`: an `open` that returns `CardUnreachable` for the first `failures` opens, then the scripted SIM. It also counts opens (for FR-015). A transmit-level reset is covered by a `Sim` reply list entry that fails.
- `on_full_renewal_failure` is tested directly. The full renewal calls the real `attempt_renewal`, which CI cannot drive (no SIM). Mocking justification goes at the site, per constitution I.
- `pcsc_failure` is tested over every `pcsc::Error` variant listed in D1, plus a sample of the unmapped ones.
- Hardware-gated `#[ignore]` tests on the reader host:
  - another holder resets the card (the bare `Card` drop from #234's control test), and our next transmit is `CardUnreachable`;
  - `connect` for an IMSI that no reader holds is `CardUnreachable`.

## D9: The network side keeps the registration

An unanswered re-authentication leaves the existing registration in place: on reg-await-auth expiry for an already-registered identity, TS 24.229 §5.4.1.2.1 has the S-CSCF consider the authentication failed and leave the identity registered. That is what makes "keep the registration and refresh again later" correct. #232's own log shows the registration still valid 10 minutes after the challenge.

## Constants (`grep -rn 'MAX_\|_BACKOFF\|WORK_TIMEOUT\|REFRESH_FLOOR' src/ims`)

| Need | Reused or new | Value |
|---|---|---|
| challenge retry window | new `CARD_RETRY_WINDOW` | 30 s |
| interval between attempts | new `CARD_RETRY_INTERVAL` | 2 s |
| bound past the window before abandoning, and the bound on a check | new `CARD_ATTEMPT_BOUND` | 15 s |
| first check | reuse `RETRY_INITIAL_BACKOFF` | 5 s |
| check backoff step | reuse `next_backoff` | ×2 |
| check backoff cap | new `CARD_CHECK_MAX_BACKOFF` | 60 s |
| safety margin | reuse `REFRESH_FLOOR` | 60 s |
| modem challenge timeout | unchanged `WORK_TIMEOUT` | 600 s |
| resync limit | unchanged `MAX_RESYNCS` | 2 |
| renewal budget | unchanged `MAX_CONSECUTIVE_RENEWAL_FAILURES` | 6 (not counted for card-unreachable on reader lines) |
