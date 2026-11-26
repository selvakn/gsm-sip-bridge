# Implementation Plan: A Slow SIM Answer No Longer Costs a Modem Line Its Registration, or Corrupts the Next Session on the Port

**Branch**: `164-modem-csim-port-sync` | **Date**: 2026-11-25 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `specs/164-modem-csim-port-sync/spec.md`

## Summary

Fixes #237 in three parts. Design decisions D1–D11 are in [research.md](research.md).

1. **Sync before trusting a modem AT port.**
   - One `Session::sync(bound)` runs on the port's worker thread: on every open and reopen (as a new `Request::Sync`), and before the next command after any timeout or desync, where it replaces `discard_pending` and the bare-`AT` `resync`.
   - It sends `AT+CMEE?`, waits for a `+CMEE:` line and `OK`, then sends `AT+CSCS?` and waits for a `+CSCS:` line and `OK`, discarding everything else.
   - It is bounded by the session's own timeout.
   - A failed sync on open is a new typed `BridgeError::PortOutOfStep`, whose text still reads `AT command timeout`. The three open-retry loops do not retry it, and `open_modem_waiting` moves to a clock deadline.
2. **A 20 s budget for the SIM's AKA.**
   - A new `ApduTransport::transmit_aka_apdu` hook is used by `authenticate` for AUTHENTICATE and its GET RESPONSE.
   - `AtCommander` overrides it with `csim_within(.., AKA_RESPONSE_BUDGET)`, and `vowifi-usim-bridge` picks the same budget for a relayed `INS=0x88` and its `61XX` chase.
   - Card-reader lines keep the default.
   - The derived watchdog and sweep budgets count the sync and the AKA budget, and two sweep constants move: 30 → 35 s and 60 → 72 s.
3. **Logging.**
   - One debug line per AKA run with the open, select and authenticate times.
   - One warning per sync that discarded stale bytes, from the sync itself.

## Technical Context

**Language/Version**: Rust 1.99.1 (`rust-toolchain.toml`), crate `gsm-sip-bridge`
**Primary Dependencies**: existing only (`serialport` 4.10.1 through `AtCommander`). No new crates
**Storage**: N/A
**Testing**: `cargo test` through `make test`. The sync is tested against real fds (`UnixStream` pairs with `spawn_modem` scripts in `at_worker`, with real blocking and timeouts). The open path goes through a test-only constructor that uses the same worker spawn and `Request::Sync`. AKA budgets are tested with a slow socket modem. Live on the local EC200U rig (quickstart).
**Target Platform**: Linux, every process that opens a modem AT port (agent, supervisor subprocesses, `vowifi-usim-bridge`, CS pool)
**Project Type**: telecom bridge, a single binary with subcommands
**Performance Goals**:
- an open with nothing stale costs ≤ 100 ms more than today (SC-003; two round trips of ~12 ms each measured);
- discovery probing of a non-AT port costs no more than today (800 ms bound, unchanged).

**Constraints**:
- the sync writes no modem setting (FR-004);
- the error text read by classifiers is unchanged (FR-008);
- card-reader lines are unchanged apart from a debug line (FR-010);
- no new unbounded read on a caller thread.

**Scale/Scope**: five source modules (`modules/at_worker.rs`, `modules/at_commander.rs`, `modules/usim.rs`, `vowifi/usim_bridge.rs`, `ims/mod.rs`), small edits in `ims/registration_auth.rs`, `volte/sms.rs`, `error.rs` and `ims/agent/watchdog.rs` (test), plus docs.

## Constitution Check

| Principle | Status |
|---|---|
| I. Integration-first testing | Pass. The sync runs against real OS sockets with real timeouts (the existing `at_worker` test style, constitution note at `at_worker.rs:400`). The AKA budget is tested through the real `AtCommander` worker against a socket modem. The scripted SIM in `registration_auth::scripted` is existing hardware stand-in code and keeps its justification. No new mocks |
| II. Green on commit | Pass. `make format && make lint && make test` before each commit |
| III. Atomic commits | Pass. Commits: (1) `Session::sync` replaces the drain and resync; (2) sync on open and reopen, `PortOutOfStep`, retry loops, and the sweep and renewal derivations it changes; (3) AKA budget, the trait hook, the bridge, and the renewal derivation's AKA term; (4) logging; (5) docs, release notes and learnings. Each derived budget changes in the same commit as its cause. Each commit builds and passes on its own |
| IV. Makefile-driven | Pass. No new targets |
| V. Simplicity | Pass. One sync function for every path. It reuses `ResponseBudget`/`send_command_within`, the worker/dispatch wait, the `CardUnreachable` typed-error pattern and the existing classifiers. One new error variant, one new request, one new trait method. `open_unsynced` is deliberately not added (D8) |

Re-check after design: passes. No complexity to track.

## Blast Radius

Real `grep` output from `744348b`, paths relative to `gsm-sip-bridge/src`.

### State and invariants inventory

| Changed item | Readers / writers (grep) | Copies at construction | Invariants and comments | Decision |
|---|---|---|---|---|
| `Session.desynced` (`at_worker.rs`) | `grep -n desynced at_worker.rs`: `:120` (field), `:132` (new), `:161`–`:190` (`fill` sets it on every error), `:224`–`:226` (`send_command_within` drains), `:264`, `:273` (set on overflow and timeout), `:307` (`read_line_raw` partial line), `:350` (`resync` clears), tests `:557`–`:725` | none | Field doc: "the next command drains before it writes". `fill` doc: "every error path leaves `desynced` set" | Same writers. The reader at `:224` calls `sync(self.timeout)` instead of `discard_pending`. It is cleared only by a *successful* sync, and a failed one leaves it set and returns `Discovery("AT command timeout")`. The field doc is updated. `read_line_raw` does not sync (it sends no command, and the URC poll must not write) |
| `discard_pending`, `resync`, `RESYNC_TIMEOUT` | `discard_pending`: `:225`, `:349` only. `resync`: `:387` (worker `Request::Resync`), tests `:741`, `:747`, `:762`. `RESYNC_TIMEOUT`: `:352` only | - | `discard_pending` doc: "a handful of reads is enough to clear one abandoned response" (false for a still-running command: F3) | Removed. `Request::Resync` runs `sync(self.timeout)` (context `resync`). The tests `:729`–`:762` are rewritten to script the sync replies |
| `Request` enum (new `Sync`) | Matches: `at_worker.rs:368`, `:383`, `:386` (`run`). Constructed: `at_commander.rs:354` (Command), `:384` (ReadLine), `:507` (Resync) | - | "Each carries its own reply channel" | Add `Sync { reply: Sender<BridgeResult<()>> }`, handled in `run` with context `open`. `Resync` stays as the name for the suspect-recovery request (`try_resync`), now backed by the same sync |
| `Session::new` signature (adds `label`) | `at_worker.rs:407` (test `port()`), `at_commander.rs:274` (`open_session`), `:282` (`from_stream`) | - | - | `new(port, timeout, label)`. `open_session` passes the path, `from_stream` passes `"stream"`, and tests pass `"test"` |
| `AtCommander::open_with_timeout` (now syncs before returning) | All production callers in research F11 (25 sites), plus `try_reopen` (`at_commander.rs:534`) for the reopen | `Transport::Worker { state: Healthy }` (`:255`) | Doc `:236`–`:239`: "'the port is free again' is not something a surviving process can assume after a timeout" | After spawning, it sends `Request::Sync` and waits `timeout + WORKER_GRACE`. On failure it drops `tx` (the worker exits after its bounded sync and frees the port) and returns `PortOutOfStep`. `try_reopen` shares the same `start_worker(session, path, timeout)`, and returns `false` on a failed sync (then the channel is `Dead`, as today when the reopen fails) |
| `BridgeError` (new `PortOutOfStep`) | `grep -rn 'match .*BridgeError\|BridgeError::[A-Z][a-z]*(_) *=>' src`: no exhaustive match. Display read by `contains("timeout")` (`modules/worker.rs:253`, `:829`, `bin/gsm_echo.rs:154`, `ims/session.rs:43`) and `has_csim_failure` (`sim_recovery.rs:130`) | - | `CardUnreachable` doc: "Recognise it with `is_card_unreachable`, never by its text" | `#[error("discovery error: AT command timeout: {0}")]`. It contains `timeout`, so the four classifiers read it as before. It does not contain `AT+CSIM failed`. `is_port_out_of_step()`. Built only in `start_worker` |
| Open-retry loops | `ims/mod.rs:1043` `open_modem_waiting` (callers `:1122`, tests `:2313`, `:2338`, `:2355`); `volte/sms.rs:900` `open_with_retry`; `vowifi/usim_bridge.rs:190` `try_open_with_backoff` | - | `open_modem_waiting`: "Losing the race used to fail the agent outright" and "waits out a conflicting holder" | Each returns `is_port_out_of_step()` at once. `open_modem_waiting` gains `retryable` and a clock deadline. Its warning keeps "modem serial busy" for retried errors only |
| `usim::csim` (gains `csim_within`) | `usim.rs:42` (`AtCommander::transmit_apdu`), `vowifi/usim_bridge.rs:116` (`send_raw_apdu`) | - | `send_raw_apdu` doc: "the same primitive `usim::authenticate`/`select_usim` already use" | `csim` calls `csim_within(.., default budget)`. The error text is the same on both (`AT+CSIM failed: …`, `unexpected +CSIM reply`) |
| `ApduTransport` (new `transmit_aka_apdu`) | `impl`s: `usim.rs:40` (AtCommander), `pcsc_card.rs:283`, `:313`, `pcsc_list.rs:299`, `vowifi/plmn.rs:282`, `registration_auth.rs:377`, `:597`, `:613` (tests), `usim.rs:498`, `:522` (tests) | - | `authenticate_usim` doc: "the only way AKA reaches the card" | A default method, so no `impl` has to change. Only `AtCommander` overrides it. `authenticate` (`usim.rs:307`) calls it at `:313` and `:320` (AUTHENTICATE, GET RESPONSE). `PcscTransport::authenticate_usim` runs inside its transaction through `select_and_authenticate` → `authenticate` → the default → `transmit_apdu`, so it is unchanged |
| `OpenedCard` (new `opened: OpenTimings`) | Literals: `ims/mod.rs:1151` (production), `registration_auth.rs:426`, `:440`, `:441`, `:585` (tests). Destructured: `ims/mod.rs:1177`, `registration_auth.rs:312` | - | Doc: "A modem line's USIM application is selected at open" | `OpenTimings: Default`. Test literals add `opened: Default::default()`. The destructures take `opened` for the timing log |
| `derived_renewal_worst_case` (test) | `watchdog.rs:668`, `:689` | - | "if someone raises `DEFAULT_TIMEOUT` or the APDU count, this fails the build" | Adds the open sync, CIMI/CGSN and the AKA budget (D7). 269 s against 360 s |
| `PER_MESSAGE_PORT_BUDGET`, `SWEEP_PASS_BUDGET` | `volte/sms.rs:931`, `:1012`, `:1117`, tests `:1339`; `:927`, test `:1280` | `PassBudget` | "Pinned against the watchdog budget by `a_sweep_pass_cannot_outrun_its_watchdog_budget`" | 35 s and 72 s (D7). `open_worst` in the four sweep tests adds one sync bound |
| `spawn_fallback_restart` behaviour | `restart_cycle.rs:225`, `pool/dispatch.rs:319` | - | "`context` only labels the warning if the port will not open" | Unchanged code. A modem that fails the sync now hits the existing warning instead of being sent a reset (D8, deliberate) |
| `discard_pending` warning text | `grep -rn 'discarded a stale'`: only its own definition | - | - | Replaced by the sync's warning (D10) |

**Implicit contracts with existing code.**
- `AT command timeout` stays the text for every timeout: the worker, `dispatch`, a failed in-session sync, and inside `PortOutOfStep`.
- `AT+CSIM failed:` stays on an explicit `+CSIM` error, so `sim_recovery` and `usim_bridge`'s SIM reset count exactly as today.
- `MAX_RESPONSE_LINES` (256) is reused for the AKA budget's line cap, made `pub(crate)` rather than copied.
- `WORKER_GRACE` (2 s) stays the slack on every worker wait, including the open's sync wait.

### Concurrency and ownership map

- **The sync on open.** It runs on the new worker thread, which owns the port. The caller holds `tx` and waits on a reply channel.
  - On success, the commander is returned as today.
  - On failure or timeout, the caller drops `tx`. The worker finishes its bounded sync (`self.timeout`), finds its reply receiver gone, returns to `recv()`, sees the channel closed and exits, dropping the port and its flock.
  - A worker wedged inside `read(2)` during the sync is leaked exactly as a wedged command is today: it holds the flock until the process exits, and the next open in this process fails with a lock error, which the retry loops wait out.
- **The in-session sync.** It runs inside `send_command_within` on the worker. No new thread or handle.
- **`try_reopen`.** It already drops the old `tx` and spawns a new worker. Now the new worker syncs before `try_reopen` reports success. A failed sync drops the new worker's `tx` the same way, and `ensure_usable` marks the channel `Dead`, as it does today when the reopen fails.
- **No new socket, kernel object or guard.** The challenge thread (`SimWorker`) is unchanged. It just waits up to 20 s on one APDU instead of 5 s.

### Where else does this logic run?

- **Every `AtCommander::open` caller** (F11) goes through `open_with_timeout`, so the open sync covers all of them with one change.
- **Every suspect recovery** goes through `ensure_usable` → `try_resync` (`Request::Resync`, now the sync) → `try_reopen` (now syncs).
- **Every in-session timeout** goes through `send_command_within`'s `desynced` check. `read_line_raw` is the only reader that does not sync, by design (it writes nothing).
- **AKA:** both sites (refresh `answer_challenge`, full registration `ims/mod.rs:1310`) reach the card through `run_aka` → `authenticate_usim` → `authenticate` → `transmit_aka_apdu`. The bridge's relay is the only other AUTHENTICATE sender, and D6 covers it.
- **The `Direct` transport** (test-only) does not sync on construction (it has no open), but syncs after an in-session timeout like the worker does.

### Situations checklist

| Situation | Behaviour |
|---|---|
| Idle line | A refresh challenge waits up to 20 s per AKA APDU on its own thread; signalling is unaffected (F14) |
| In a call | Unchanged: a refresh challenge does not block calls; a full registration already waits for the call |
| Ringing / origination pending | Not on the AT port path (VoWiFi/VoLTE media and signalling do not use the AT port) |
| UDP and TCP | Not applicable (AT port only) |
| Error and rollback | A failed open sync leaves no worker holding the port (after its bound) and returns a typed timeout. A failed in-session sync keeps `desynced` and the channel `Suspect` |
| Cancel | A refresh challenge abandoned at `WORK_TIMEOUT` (600 s) is unchanged. Its thread may now block 20 s instead of 5 s on one APDU, well inside that |
| Shutdown | Unchanged. Workers are abandoned at process exit as today |
| Limits vs. the existing path | The sync bound is the session timeout (5 s or 800 ms), the same budget the first command had. The AKA bound of 20 s is used only for INS `0x88` and its GET RESPONSE |
| Blocks the dispatch loop | Only on a full registration, which already blocks it. On a slow SIM this adds at most 2 × 15 s, counted in D7 (269 s against 360 s) |
| CS pool | The worker's port opens with a sync (~25 ms). A `RING`/`+CMTI` arriving inside that window is discarded and counted (spec edge case). After a timeout, the next command syncs first |
| Discovery | A non-AT port fails the sync within `PROBE_TIMEOUT` (800 ms), the same as the bare-`AT` probe today. An AT port costs two extra round trips |
| Docs, release notes and repo rules | A `RELEASE_NOTES.md` bullet under `## Unreleased` with only the summary in bold. `docs/operations.md` gets the new warning. No real identifiers in tests or docs (synthetic IMSI/IMEI only) |

### Addendum: class grep during implementation (T015)

Run on the branch after commit 1, paths relative to `gsm-sip-bridge/src`.

- `grep -rn 'serialport::new\|at_worker::Session::new\|Session::new(' src`: one `serialport::new` (`modules/at_commander.rs` `open_session`). `at_worker::Session::new` is called by `open_session` (real port), `from_stream` (`Direct`, test-only) and `start_for_test` (test-only), plus the `at_worker` test `port()`. The `Session::new()` hits in `vowifi/usim_bridge.rs` are that module's own vpcd `Session`, unrelated. Every real port session reaches `start_worker` through `open_with_timeout` or `try_reopen`.
- `grep -rn 'AtCommander::open' src`: 32 lines, 25 production call sites (F11) plus doc comments. Every one is `AtCommander::open` or `open_with_timeout`, so the open sync covers all of them with no per-site change.
- `grep -rn 'fn .*open.*retry\|OPEN_RETRY\|open_modem_waiting' src`: the three known loops (`ims/mod.rs` `open_modem_waiting`, `volte/sms.rs` `open_with_retry`, `vowifi/usim_bridge.rs` `try_open_with_backoff`), all changed. `try_reopen`'s `REOPEN_ATTEMPTS` loop retries only `open_session` (a lock error, instant) and then syncs once through `start_worker`, so it does not multiply the sync bound.
- **Discovery.** `probe_one_candidate` and `discovery/sim.rs` run the open inside `run_bounded(policy.probe_timeout())`, default 5 s. A non-AT port fails the open's sync within `PROBE_TIMEOUT` (800 ms, +2 s `WORKER_GRACE` if the worker wedges): 2.8 s worst case, the same as the old open plus bare-`AT` probe. An open failure was already mapped to `NotAtCapable` / `OpenFailed`, so no code change.
- **`spawn_fallback_restart`** (`modules/pool/restart_cycle.rs`): unchanged code. A modem that fails the sync now logs the existing `could not open modem port for fallback restart` warning instead of being sent the reset (D8).

## Project Structure

### Documentation (this feature)

```text
specs/164-modem-csim-port-sync/
├── spec.md
├── plan.md              # this file
├── research.md          # Phase 0: facts F1–F15, decisions D1–D11
├── data-model.md        # Phase 1
├── quickstart.md        # Phase 1: live checks on the local EC200U and for the reporter's EC20
├── contracts/
│   └── operator-surface.md   # log lines and error text this feature adds or must preserve
├── checklists/requirements.md
└── tasks.md             # Phase 2 (/speckit-tasks)
```

### Source Code (repository root)

```text
gsm-sip-bridge/src/
├── error.rs                       # BridgeError::PortOutOfStep, is_port_out_of_step()
├── modules/
│   ├── at_worker.rs               # Session::sync, label, Request::Sync; drain/resync removed
│   ├── at_commander.rs            # start_worker (open + reopen sync), start_for_test
│   └── usim.rs                    # csim_within, AKA_RESPONSE_BUDGET, transmit_aka_apdu
├── ims/
│   ├── mod.rs                     # open_modem_waiting (retryable, deadline), OpenTimings, timing log at full registration
│   ├── registration_auth.rs       # timing log at refresh AKA
│   └── agent/watchdog.rs          # derived_renewal_worst_case (test)
├── volte/sms.rs                   # open_with_retry, PER_MESSAGE_PORT_BUDGET, SWEEP_PASS_BUDGET
└── vowifi/usim_bridge.rs          # AKA budget on relayed INS 0x88, try_open_with_backoff
docs/operations.md
RELEASE_NOTES.md
```

**Structure Decision**: existing single crate; no new modules.

## Complexity Tracking

None.
