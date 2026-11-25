# Research: A Slow SIM Answer No Longer Costs a Modem Line Its Registration, or Corrupts the Next Session on the Port

Paths are relative to `gsm-sip-bridge/src`. Line numbers are as of `744348b` (main after #236).

## Facts

- **F1. A new session trusts the channel.** `at_worker::Session::new` (`modules/at_worker.rs:128`) starts with `desynced: false` and an empty `rx`, and its first `send_command_within` writes at once. Nothing drains on open. `AtCommander::open_with_timeout` (`modules/at_commander.rs:240`) opens the port through `open_session` (`:265`, serialport: TIOCEXCL, flock, `tcflush(TCIOFLUSH)`), spawns the worker and returns. `try_reopen` (`:534`) builds a new session the same way.

- **F2. The abandoned reply reaches the next session (measured).** Local EC200U, `ttyUSB0`, 2026-11-25, `tmp/reopen-test/`. Commands were written, the port was closed, then reopened exactly as serialport does:
  - Small replies (`ATI`, `+CSIM` STATUS) arrived whole on an immediate reopen.
  - After 1 s, a lone `\r\nOK\r\n` survived for CSIM.
  - Large replies (`AT+COPN`, 23 KB) lost their first ~2 KB and were held on the device, then delivered within ~50 ms of a reopen even 10 s later.

  The flush runs before the device sends its held data, so it does not help.

- **F3. The in-session drain has the same hole.** After a timeout, `send_command_within` runs `discard_pending` (`at_worker.rs:322`) before the next write. It reads up to 8 times and stops at the first read that times out empty, then writes. A reply that the modem is still producing after that (a slow SIM, a long command) arrives after the drain and is read as the next command's answer. `resync` (`:348`, reached through `Request::Resync` from `AtCommander::try_resync` `:495`) is the same drain followed by a bare `AT` under `RESYNC_TIMEOUT` (2 s). A stale `OK` satisfies that `AT`, so it cannot detect an out-of-step channel either.

- **F4. The incident chain, walked through the code.** The challenge thread's `answer_once` (`ims/registration_auth.rs:303`) opens with `open_card` and answers. On the AT timeout its `AtCommander` drops and the port closes, while the SIM's reply is still due. The full registration's `open_card` (`ims/mod.rs:1096`) opens again, with configured IMSI/IMEI so the first command is an APDU, and runs `discover_usim_aid` (`modules/usim.rs:270`: SELECT 3F00, SELECT 2F00, READ RECORD 1..16) and then `select_usim` (`:288`).

  With the stale `DB…9000` read first, the replies go:
  - SELECT 3F00 gets the stale reply: passes;
  - SELECT 2F00 gets 3F00's `9000`: passes;
  - READ RECORD 1 gets `9000` with no data: treated as empty;
  - READ RECORD 2 gets record 1's data: the AID is "found";
  - `select_usim`'s SELECT 3F00 gets record 2's `6A83`.

  That is the reporter's error.

- **F5. Every APDU runs under the port's 5 s.** `usim::csim` (`usim.rs:78`) calls `at.send_command`, so the budget is `DEFAULT_TIMEOUT` (`at_commander.rs:14`). `authenticate` (`usim.rs:307`) sends AUTHENTICATE and, on `61XX`, a GET RESPONSE, both through `transmit_apdu`. `ApduTransport::authenticate_usim` (`usim.rs:35`) is the only way AKA reaches a card (`ims::run_aka`, `ims/mod.rs:1156`). `PcscTransport` overrides it (`modules/pcsc_card.rs:299`) to run inside one PC/SC transaction. `AtCommander` uses the default.

- **F6. A per-command budget already exists.** `send_command_within(cmd, ResponseBudget)` (`at_commander.rs:335`) passes the budget to the worker, and `dispatch` waits `budget.timeout + WORKER_GRACE` (`:420`). `BULK_LIST_BUDGET` (`sms/reader.rs:45`, 4096 lines, 30 s) is its only user. `MAX_RESPONSE_LINES` (256) is private to `at_worker`.

- **F7. `vowifi-usim-bridge` passes charon's APDUs through the same `csim`.** `forward_apdu` (`vowifi/usim_bridge.rs:132`) calls `send_raw_apdu` (`:114`, `usim::csim`), and on `61XX` chases a GET RESPONSE itself (`:141`). A client's literal GET RESPONSE after that is answered from the session cache without touching the modem (`synthesize_61xx_if_needed`, `:174`). So the only GET RESPONSE that reaches the modem for an AUTHENTICATE is the chase inside `forward_apdu`.

- **F8. The error text other code reads.**
  - `"AT command timeout"`: `at_worker.rs:276` and `at_commander.rs:441`. It is read by `contains("timeout")` in `modules/worker.rs:253`, `:829`, `bin/gsm_echo.rs:154` and `ims/session.rs:43` (`map_registration_error`).
  - `"AT+CSIM failed: …"` (`usim.rs:88`): read by `sim_recovery::has_csim_failure` (`supervise/sim_recovery.rs:130`) and by `usim_bridge`'s own SIM reset (`:429`).
  - `sim_recovery` does **not** match `AT command timeout`. The issue text says it does; the code says otherwise.
  - The open failure is `"failed to open serial {path}: {e}"` (`at_commander.rs:272`). Only a test fixture reads its wording (`ims/mod.rs:2320`).

- **F9. Open-retry loops retry every error and count sleeps, not the clock.**
  - `ims::open_modem_waiting` (`ims/mod.rs:1043`) retries any open error for `MODEM_OPEN_MAX_WAIT` (30 s), adding up only its sleeps (`waited += interval`).
  - `volte/sms.rs::open_with_retry` (`:900`) makes 4 attempts with 0.3/0.6/0.9 s sleeps.
  - `usim_bridge::try_open_with_backoff` (`:190`) makes 5 attempts with 0.5–2.0 s sleeps.

  Today a failed open is instant (a lock conflict), so the sums match the clock. Once the sync runs inside `open`, a failed open can take the whole sync bound.

- **F10. Derived budgets that count `DEFAULT_TIMEOUT` and the open cost.**
  - `watchdog::tests::derived_renewal_worst_case` (`ims/agent/watchdog.rs:668`) adds up `MODEM_OPEN_MAX_WAIT + 20 APDUs × DEFAULT_TIMEOUT + 2 × 32 s + 20 s` = 214 s, against `Phase::Renewal` = 360 s with a ≥20% margin required. It counts neither AUTHENTICATE nor its GET RESPONSE.
  - In `volte/sms.rs` tests, `PER_MESSAGE_PORT_BUDGET` (30 s, `:931`) must be ≥ `MODEM_LOCK_TIMEOUT` (20 s) + `open_worst` (1.8 s of sleeps) + `DEFAULT_TIMEOUT` = 26.8 s, and `SWEEP_PASS_BUDGET` (60 s, `:927`) must be ≥ 2 × `PER_MESSAGE_PORT_BUDGET` and leave ≥20% under `Phase::SmsSweep` (90 s). The listing step (`:1314`) and the signal poll (`:1263`) have the same shape.

- **F11. Who opens a modem port** (`grep -rn 'AtCommander::open' src`, production only):

  | Site | Purpose | Shared port |
  |---|---|---|
  | `ims/mod.rs:1126` | `open_card` (registration, refresh challenge) | yes |
  | `ims/agent/mod.rs:322` | start-up PLMN read | yes |
  | `volte/sms.rs:900` | SMS sweep, signal poll | yes |
  | `vowifi/usim_bridge.rs:531` | EAP-AKA relay | yes |
  | `vowifi/plmn.rs:121`, `vowifi/imsi.rs:22`, `vowifi/discovery.rs:718`, `vowifi/ims_mode.rs:119,150,163` | identity, mode | yes |
  | `volte/mod.rs:199,343,384,399`, `volte/pcscf.rs:706`, `volte/pani.rs:73`, `volte/carrier_agent.rs:125`, `commands/volte.rs:161,321,568` | VoLTE PDN, PLMN, P-CSCF | yes |
  | `modules/worker.rs:160`, `modules/pool/mod.rs:722`, `modules/pool/restart_cycle.rs:396`, `bin/gsm_echo.rs:54` | circuit-switched pool | usually own port |
  | `modules/discovery/probe.rs:147`, `modules/discovery/sim.rs:89` | discovery (800 ms, `PROBE_TIMEOUT`) | candidate ports, some not AT |

  All of them go through `open_with_timeout`. `from_stream` (the `Direct` transport) appears only in `#[cfg(test)]` code (`at_commander.rs:93`).

- **F12. Candidate sync queries, measured on the EC200U.**
  - `AT+CMEE?` gives `+CMEE: 1` / `OK`.
  - `AT+CSCS?` gives `+CSCS: "GSM"` / `OK`.

  Each round trip takes about 12 ms to the first byte (about 60 ms with the test's 20 ms polling). Neither needs the SIM. Neither is sent anywhere in the codebase (`grep -rhoE '"AT[A-Z+&]*[A-Z0-9]*[?=]?' src` lists every command sent; neither appears). Both are standard TS 27.007 read commands that the Quectel EC20/EC25 AT manuals list. Neither command's reply can look like a `+CSIM:` line, an `AT+CIMI` digits line, or any other reply the bridge parses.

- **F13. A fallback restart opens a fresh port only to send a reset.** `restart_cycle::spawn_fallback_restart` (`modules/pool/restart_cycle.rs:386`) opens and sends `AT+CFUN=1,1` (`reboot`, fire-and-forget) or `AT+CFUN=0`/`=1` (`radio_restart`). This is the one caller that writes without needing an answer.

- **F14. The refresh challenge runs off the dispatch loop.** `Reauthentication::spawn` uses `WORK_TIMEOUT` = 600 s (`registration_auth.rs:30`). A full registration (`register_session`) runs on the dispatch thread under `Phase::Renewal` and holds the in-process `modem_lock` (`ims/agent/mod.rs:991`, `:4443`). The challenge thread does not hold `modem_lock`, so during it the SMS sweep finds the port taken (TIOCEXCL) and retries next interval. That is the reporter's `Unable to acquire exclusive lock`.

- **F15. Learnings (Part 1) classes that apply.**
  - "A wait of N seconds is measured on the clock against a deadline, not as the sum of its sleeps, when its steps can block too" (F9).
  - "Every derived budget recomputed from real constants" (F10).
  - "New error string → check classifiers" (F8).
  - "Only one process holds a serial port at a time" (unchanged: the sync runs inside the holder's own session).
  - "Modem responses reject command echoes" (D2).
  - "No blocking on a thread that must keep dispatching" (the full registration already blocks there; D7 bounds the extra).

## Decisions

### D1. Where the sync runs: inside the worker, as one `Session::sync` used by every path

- **Decision:** `Session::sync(bound) -> BridgeResult<usize>` is the one implementation, and it returns the stale bytes it discarded. It runs:
  - in `open_with_timeout` and `try_reopen`, as a new `Request::Sync` sent to the freshly spawned worker before the commander is returned. The caller waits `bound + WORKER_GRACE`, as `dispatch` does;
  - in `Session::send_command_within` when `desynced` is set, replacing `discard_pending`;
  - for `Request::Resync` (`try_resync`), replacing the drain plus bare `AT`.

  `discard_pending`, `resync` and `RESYNC_TIMEOUT` are removed.
- **Rationale:** the sync reads the port, so it has the same unguarded-`read(2)` exposure every read has (`at_worker.rs` module doc). It must run on the worker, so that the caller stays bounded and a wedged worker is abandoned exactly as for a command. One function means one behaviour on every path (FR-001, FR-001a).
- **Alternatives:**
  - Syncing on the caller's thread before spawning the worker: rejected, because it reintroduces the 2026-08-16 unbounded read.
  - A separate drain-then-sync on reopen only: rejected, because FR-001a needs the in-session path too.

### D2. The sync exchange: `AT+CMEE?` then `AT+CSCS?`, each sent after the previous reply

- **Decision:**
  1. Clear `rx` and count its bytes as discarded.
  2. Write `AT+CMEE?`. Read lines, discarding each, until a line starting `+CMEE:` has been read followed by `OK`.
  3. Write `AT+CSCS?`. Read lines, discarding each, until a line starting `+CSCS:` has been read followed by `OK`. The session is now in step.

  Everything discarded counts as stale, except the lines accepted as this sync's own two replies. Echoed command lines (`AT+CMEE?`, `AT+CSCS?`) do not start with `+`, so they are discarded like anything else. `ERROR` and `+CME ERROR` are discarded too, because they cannot be told apart from stale replies. A modem that refuses both queries fails the sync, and with it the open, which the quickstart checks on both chipsets. The whole exchange, including both writes, runs against one deadline of `now + bound`.
- **Rationale:** replies come out in command order on a port (F2). A stale `+CMEE:` reply from one earlier abandoned sync, matched in place of ours, is followed by our own `+CMEE:`, which is discarded while we wait for `+CSCS:`, and then by our `+CSCS:`. A stale `+CSCS:` from an earlier sync abandoned after its first step arrives before our `+CMEE:`, so it is discarded while we wait for `+CMEE:`. Either way the session ends in step (Clarification Q2). Neither query writes anything (FR-004), and neither needs the SIM, so a SIM-less or locked modem still opens (discovery, CPIN probes).
- **Not covered:** two or more syncs in a row given up while the modem stays busy for longer than two bounds. Their stale `+CMEE:`/`+CSCS:` pairs can line up ahead of ours. This is recorded in spec Assumptions.
- **Alternatives:**
  - A unique marker written to an S-register: rejected (Clarification Q2: it writes modem settings shared across ports, including ModemManager's data port).
  - A single query plus a quiet window: rejected, because it has no ordering proof and costs the window on every open.
  - `AT` / `ATI`: rejected, because `AT` answers a bare `OK` that a stale `OK` imitates, and the bridge already sends `AT` (probe, `ims_mode`).

### D3. The sync bound: the session's own port timeout

- **Decision:** `bound = self.timeout`, the session's configured timeout: 5 s for `open`, 800 ms for discovery's `open_with_timeout(PROBE_TIMEOUT)`. It is the same for every path in D1. On failure, the session stays `desynced` and the error is `BridgeError::Discovery("AT command timeout")` (the existing text), so every classifier in F8 reads a sync failure as the timeout it is.
- **Rationale:** FR-003 asks for this. Discovery probing of a non-AT port costs no more than today: the bare `AT` probe would have waited the same 800 ms. A dead modem fails at the sync instead of at its first command, after the same bound.
- **Alternatives:**
  - The AKA budget (20 s): rejected. Every open of a dead modem would take 20 s, which breaks the SMS sweep and discovery bounds and the `timeout`-wrapped subprocesses.
  - `RESYNC_TIMEOUT` (2 s): rejected, because a still-running ordinary command can legitimately need the port's 5 s.

### D4. A failed sync on open is a typed error, so that retry loops do not retry it

- **Decision:** add `BridgeError::PortOutOfStep(String)`, displayed as `discovery error: AT command timeout: {0}`, and the predicate `BridgeError::is_port_out_of_step()`. It is built only where a sync fails: `open_with_timeout`/`try_reopen`, from the `Request::Sync` result or the worker wait running out. The in-session sync failure in `send_command_within` keeps returning today's `Discovery("AT command timeout")`, because that path's callers already handle it.

  The three open-retry loops (F9) return a `PortOutOfStep` error at once instead of retrying it. `open_modem_waiting` gains a `retryable: impl Fn(&E) -> bool` parameter (its tests use string errors), and it also changes to a clock deadline (`Instant::now() + max_wait`) instead of adding up its sleeps.
- **Rationale:**
  - Without this, a modem that does not answer costs each retry loop *attempts × (bound + grace)*: 4 × 7 s in the sweep, 5 × 7 s in the bridge, and about 4 × 7 s inside `open_modem_waiting`'s 30 s. That breaks every derived budget in F10.
  - The loops exist to outlast another *holder* of the port (their doc comments say so), not an unresponsive modem.
  - The text keeps `AT command timeout`, so `contains("timeout")` classifiers read it as before (F8), and it carries the `discovery error:` prefix like the open failure it replaces.
  - The CardUnreachable precedent (#233) is a typed variant whose text is shaped for the existing classifiers.
- **Alternatives:**
  - Classifying on the text in the loops: rejected (learnings: never by text).
  - Leaving the loops as they are and raising budgets: rejected, because it makes every unresponsive-modem case slower for no benefit.

### D5. The AKA budget: 20 s for AUTHENTICATE and its GET RESPONSE, through one trait hook

- **Decision:**
  - `usim::AKA_RESPONSE_BUDGET: ResponseBudget = { max_lines: 256, timeout: 20 s }`. The default line cap is made `pub(crate)` as `at_worker::MAX_RESPONSE_LINES`, so it is not copied.
  - A new trait method `ApduTransport::transmit_aka_apdu(&mut self, apdu)` defaults to `transmit_apdu`. `authenticate` (`usim.rs:307`) uses it for both the AUTHENTICATE and its GET RESPONSE.
  - `impl ApduTransport for AtCommander` overrides it with `csim_within(self, apdu, AKA_RESPONSE_BUDGET)`, where `csim_within` is today's `csim` with a budget and `csim` becomes `csim_within(.., default)`.
  - `PcscTransport` keeps the default, so card-reader lines are unchanged (FR-010).
- **Rationale:**
  - The incident's reply arrived after 5 s and no later than ~11 s (F4: by the reopen 6 s after the timeout); the line's previous challenge took 4.2 s end to end.
  - The registrar waits far longer (reg-await-auth, the same bound `CARD_RETRY_WINDOW`'s 30 s sits inside).
  - The refresh thread's outer bound is 600 s (F14).
  - The error text is unchanged: the worker's `AT command timeout`, `dispatch`'s `AT command timeout`, `csim`'s `AT+CSIM failed: …` (FR-008).
- **Alternatives:**
  - Overriding `authenticate_usim` on `AtCommander` with a copy of `authenticate`: rejected, because it copies the 61XX logic.
  - Raising `DEFAULT_TIMEOUT`: rejected, because it slows every other command's failure.

### D6. The bridge applies the AKA budget to an AUTHENTICATE it relays, and to the GET RESPONSE it chases

- **Decision:**
  - `send_raw_apdu` takes a `ResponseBudget`.
  - `forward_apdu` picks `AKA_RESPONSE_BUDGET` when `apdu[1] == 0x88` (with the ISO class check `apdu.len() >= 5`), and uses the same budget for the `61XX` chase it sends for that APDU.
  - Every other APDU, and the SELECT P2 retry, keeps the default.
  - The client's later literal GET RESPONSE is still served from the cache (F7), so it never reaches the modem.
- **Rationale:** FR-007 and Clarification Q3. One decision point, at the place where the bridge already knows the INS byte.
- **Alternatives:** tracking "the last relayed APDU was AUTHENTICATE" across vpcd messages: unnecessary, because the chase happens inside the same `forward_apdu` call.

### D7. Derived budgets: count the sync and the AKA budget, then adjust the two sweep constants that no longer fit

- **Decision:**
  - `derived_renewal_worst_case` adds:
    - one sync bound for the open (`DEFAULT_TIMEOUT`);
    - `AT+CIMI` and `AT+CGSN` (2 × `DEFAULT_TIMEOUT`), which it never counted;
    - AUTHENTICATE and GET RESPONSE (2 × `AKA_RESPONSE_BUDGET.timeout`).

    That gives 30 + 5 + 10 + 100 + 40 + 64 + 20 = 269 s. 360 s still leaves 34%, so `Phase::Renewal` is unchanged.
  - `volte/sms.rs` derivations: `open_worst` becomes the retry sleeps plus one sync bound (6.8 s), because D4 makes at most one attempt reach the sync. That gives:
    - `port_worst` = 20 + 6.8 + 5 = 31.8 s, so `PER_MESSAGE_PORT_BUDGET` goes from 30 to **35 s**;
    - `SWEEP_PASS_BUDGET` must be ≥ 2 × 35, so it goes from 60 to **72 s**, which still leaves 25% under the 90 s watchdog;
    - the listing step: 20 + 6.8 + 5 + 30 = 61.8 s (46% margin);
    - the signal poll: 36.8 s.
  - `Phase::SmsSweep` is unchanged.
- **Rationale:** learnings: derived from real constants, so the build fails rather than the watchdog killing a healthy line.
- **Not derived today and left alone:** `Phase::Startup` (420 s) has no derivation test. Start-up adds at most one sync per open (a few opens) and is far inside the budget.

### D8. The fallback restart needs the sync like everyone else

- **Decision:** `spawn_fallback_restart` keeps using `AtCommander::open`. If the sync fails, the open fails and the existing warning is logged (`could not open modem port for fallback restart`). This is recorded as a deliberate behaviour change.
- **Rationale:**
  - A modem that cannot answer two read-only queries within 5 s is not parsing `AT+CFUN=1,1` either, unless it is stuck in a long command, and then the reset would have queued behind it.
  - The slot's next restart cycle tries again.
  - An unsynced-open path would be a second way to open the port, used once.
- **Alternatives:** `open_unsynced` for reset senders: rejected for now (Principle V). Reconsider if a live case shows a modem that takes a reset but not a query.

### D9. Timing log: one debug line per AKA run

- **Decision:**
  - `OpenedCard` gains `opened: OpenTimings { open: Duration, select: Option<Duration> }`, filled in by `open_card`:
    - `open` covers the port or reader connection and identities;
    - `select` covers the modem's EF_DIR walk plus SELECT, and is `None` on a card-reader line, which selects inside its AKA.
  - A shared helper `log_aka_timings(&opened, aka: Duration, outcome)` writes `tracing::debug!(open_ms, select_ms, authenticate_ms, outcome, "SIM challenge timings")`, where `outcome` is `success`, `sync-failure` or `error`.
  - The helper is called after `run_aka` at both AKA sites: `registration_auth::answer_challenge` (refresh) and `ims/mod.rs:1310` (full registration, including its AUTS retry, which reports the same `open`/`select`).
- **Rationale:** FR-011. Debug only, so card-reader behaviour is unchanged (FR-010).

### D10. The stale-bytes warning comes from the sync itself

- **Decision:**
  - `Session` gains a `label` (the port path, or `"stream"` for tests).
  - `Session::sync` logs one `tracing::warn!(port = label, discarded_bytes, context, "discarded stale bytes before the AT channel was in step")`, where `context` is `open`, `after-timeout` or `resync`, only when it discarded anything. A sync that discards nothing logs at debug level.
- **Rationale:** FR-012, in one place for every path (D1). It replaces `discard_pending`'s warning, whose wording nothing depends on (`grep -rn 'discarded a stale'` finds only its own definition).

### D11. Tests: real sockets for the sync, the existing scripted SIM for the budget

- **Decision:**
  - Sync behaviour is tested in `at_worker` with the existing `UnixStream` pair and `spawn_modem` scripts (real fds and timeouts, constitution I). Cases:
    - a stale whole reply before open;
    - a stale lone `OK`;
    - a slow command still running (its reply delayed past our first write);
    - a stale `+CMEE:`, and a stale `+CSCS:`, from an earlier abandoned sync;
    - echo on;
    - a silent port (fails within the bound);
    - a modem answering `ERROR` to both queries (fails);
    - nothing stale (no warning, two exchanges only).
  - The open path is tested through a new `#[cfg(test)] AtCommander::start_for_test(stream, timeout)`, which goes through the same worker spawn and `Request::Sync` as `open_with_timeout`.
  - The AKA budget is tested with a `UnixStream` modem that answers AUTHENTICATE after 6 s (and a GET RESPONSE after 6 s), and answers another APDU after 6 s, which must fail at 5 s.
  - `from_stream` (the `Direct` transport, test-only) does not sync on construction. That keeps its 26 scripted test sites unchanged, but its in-session sync after a timeout does run, so tests that script a timeout followed by another command add the two sync replies. That is the behaviour change under test.

## Learnings classes checked against this design

| Class | Applies | Where handled |
|---|---|---|
| Wait measured on the clock, not summed sleeps | yes | D4: `open_modem_waiting` goes to a deadline; the other loops are attempt-bounded with instant failures |
| Derived budgets from real constants | yes | D7 |
| New error text vs. classifiers | yes | D3, D4 (`AT command timeout` kept) |
| Typed, never by text | yes | D4 (`is_port_out_of_step`) |
| Command echoes rejected | yes | D2 (`+`-prefixed lines only) |
| One holder per serial port | unchanged | the sync runs inside the holder's session |
| Blocking the dispatch thread | yes, bounded | the full registration already blocks there; the extra is at most 2 × 15 s on a slow SIM, counted in D7 |
| Card-reader `CardUnreachable` / PC/SC transaction | not touched | FR-010; `PcscTransport` keeps the default hook |
