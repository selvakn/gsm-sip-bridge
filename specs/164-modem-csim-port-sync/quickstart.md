# Quickstart: Verifying Modem Port Sync and the AKA Budget

## 1. Build and test

```bash
make format && make lint && make test
```

Tests that matter most:
- `at_worker` sync tests: a stale reply on open, a lone stale `OK`, a still-running command, stale replies from an earlier sync, echo, a silent port, `ERROR` replies, nothing stale;
- the `AtCommander` open-sync tests (through `start_for_test`);
- the AKA budget tests (AUTHENTICATE answered after 6 s succeeds; another APDU answered after 6 s fails at 5 s);
- the bridge relay test (relayed INS `0x88` answered after 6 s);
- the derived-budget tests in `ims/agent/watchdog.rs` and `volte/sms.rs`.

## 2. Live: the reopen scenario on the local EC200U

The local rig (`ttyUSB0`, Vodafone) runs the bridge container `gsm-sip-bridge-pr228-reader` with a VoLTE line on that port. The scripts in `tmp/reopen-test/` take the same exclusive lock as the bridge, so they never interleave with it.

1. Leave a stale reply on the port: run `python3 -I tmp/reopen-test/reopen_test.py`'s orphan step (write `AT+CSIM=10,"80F2000000"`, close at once).
2. Within 1 s, open the port through the bridge's own code: `gsm-sip-bridge vowifi-plmn --modem /dev/ttyUSB0` (or any subcommand that opens the port) on the branch build, with `RUST_LOG=info`.
3. Expect:
   - one `discarded stale bytes before the AT channel was in step … context=open` warning;
   - the command succeeds with correct output.

   Before the fix, the same sequence reads `+CSIM: 4,"FFFF"` as the answer to the first command.
4. Repeat with `AT+COPN` and a 10 s gap: the warning shows ~21 KB discarded, and the command still succeeds.

## 3. Live: the open costs no more than 100 ms (SC-003)

Time `vowifi-plmn --modem /dev/ttyUSB0` 20 times on `main` and on the branch, with nothing stale. Expect the median difference to be ≤ 100 ms (two ~12 ms round trips).

## 4. Live: discovery is not slower

Restart the branch container with discovery on. Check that `scan_modules`/AT-probe timings in the log are unchanged for the non-AT ports (`ttyUSB1`–`ttyUSB4` on the EC200U): each fails within its 800 ms bound.

## 5. Live: the AKA budget (reporter's EC20)

No SIM here answers slower than 5 s, so SC-002 is checked on the reporter's line:

1. Run the branch build with `RUST_LOG=info,gsm_sip_bridge::ims=debug`.
2. On each challenged refresh, read `SIM challenge timings … authenticate_ms=…`.
3. A refresh whose `authenticate_ms` is between 5000 and 20000 should show `refreshed in place` (or the equivalent success line) and no `registering in full`.

Before deploying there, ask the reporter to confirm that the EC20 answers both sync queries:

```text
AT+CMEE?   → +CMEE: <n>   OK
AT+CSCS?   → +CSCS: "<cs>" OK
```

They can also run `tmp/reopen-test/hold_time.py` against their port, to see whether the EC20 holds an abandoned reply as the EC200U does (the fix does not depend on it).

## 6. Live: VoWiFi EAP-AKA still works through the bridge

On a modem-backed VoWiFi line (local rig, VoWiFi config), bring the tunnel up from scratch (`docker compose up --force-recreate`). Expect IKE_AUTH with EAP-AKA to complete as before. `RUST_LOG=…,gsm_sip_bridge::vowifi::usim_bridge=trace` shows the relayed `0x88` APDU.

## Results (2026-11-25)

Local rig: EC200U on `ttyUSB0`, no SIM in the modem. The line's SIM is in the PC/SC reader. Branch build from `git archive HEAD` (image `gsm-sip-bridge:pr164`). Times are UTC (IST = UTC + 5:30).

- **§2, reopen (SC-001): verified live.** The orphan step (write `AT+CSIM=10,"80F2000000"`, close at once) was followed within 1 s by `vowifi-plmn --modem /dev/ttyUSB0`:
  - On `main`, 3 out of 3 runs failed with `no IMSI in response`: the stale CSIM reply was taken as the answer to `AT+CIMI`.
  - On the branch, 3 out of 3 runs logged `discarded stale bytes before the AT channel was in step … discarded_bytes=25` (27 in one run) `context="open"`, and `AT+CIMI` got its own reply: `+CME ERROR: 10`, since the modem has no SIM. That is the same failure as an open with nothing stale, on both builds.
  - With `AT+COPN` orphaned and a 10 s gap, `main` failed with `exceeded 256 lines`. The branch discarded `32601` bytes and the command succeeded. That is more than the ~21 KB this document expected, because the modem's whole operator list was still queued.
- **§3, open overhead (SC-003): verified live.** Over 20 opens with nothing stale, `Session::sync` (`AT+CMEE?` + `AT+CSCS?`) took a median of 2.0 ms (min 1.9, max 2.4), with no warnings. `main` was not timed for comparison: the whole added cost is the sync, and 2 ms is far below the 100 ms bound.
- **§4, discovery: verified live.** A non-AT port still fails discovery within its 0.8 s per-command bound (2.8 s worst case with the worker grace), the same as before. The AT port is found and synced as usual.
- **Branch image on the line (FR-010): verified live.** The image was deployed at 16:44:27Z.
  - The card-reader line answered its challenge (`SIM challenge timings open_ms=28 authenticate_ms=135 outcome="success"`) and registered at 16:44:33Z.
  - It refreshed in place at 17:34:34Z (`granted_expires_secs=3600`) and its reg-event subscription refreshed at 17:39Z.
  - No `discarded stale bytes` warning was logged in the first hour.
- **§5, the AKA budget (SC-002): tests only.** No SIM here answers slower than 5 s: the modem SIM below takes about 135 ms for AUTHENTICATE. Before the fix ships to them, the reporter is asked to:
  - confirm that their EC20 answers `AT+CMEE?` and `AT+CSCS?`;
  - run with `RUST_LOG=info,gsm_sip_bridge::ims=debug` and send the `SIM challenge timings` lines from a slow refresh.
- **§6, EAP-AKA through the relay: verified live with a SIM in the modem** (next section).

### With a SIM in the modem (2026-11-25, later)

The SIM was moved from the PC/SC reader into the EC200U. The modem has SIM hot-swap detection off (`AT+QSIMDET?` → `0,0`), so `AT+CFUN=0` then `AT+CFUN=1` made it read the SIM. The line was switched to a modem-backed VoWiFi line (`modem_port = "/dev/ttyUSB0"`).

- **§2 repeated with a real IMSI: verified live.** Orphan `AT+CSIM=10,"80F2000000"`, then `vowifi-plmn --modem /dev/ttyUSB0`:
  - `main` failed 3 out of 3 times with `no IMSI in response`.
  - The branch read `404 043` 3 out of 3 times, each time logging `discarded_bytes=127 context="open"`. The reply is longer now because the SIM answers STATUS with its real FCP.
  - `main` read `404 043` with nothing stale.
- **§6, EAP-AKA through `vowifi-usim-bridge`: verified live.** The ePDG tunnel came up with `EAP method EAP_AKA succeeded` through the relay. The IMS registration that followed went through the modem: `SIM challenge timings open_ms=2 select_ms=1526 authenticate_ms=137`. The line registered 13 s after the container was created.
- **The #237 failure itself, through the line: reproduced on `main`, fixed on the branch.** The IMS agent was stopped with `SIGTERM`. The supervisor restarts it after 5 s, and the restarted agent opens the modem port to answer its challenge. Right after the stop, `AT+CSIM=10,"80F2000000"` was written to the port and the port closed.
  - **`main` (1 run):** the restarted agent opened the port 14 s later and still got the stale reply. It failed `SELECT 3F00 failed: SW=6A83`, the error in the report, and exited. The supervisor logged four agent exits, one of them the stop, and the line registered again 36 s after the stop.
  - **Branch:**
    - **Run 1:** discarded 127 bytes on open; the open, including the discard, took 55 ms. It registered 8 s after the stop with no failed attempt.
    - **Run 2:** discarded 127 bytes, then was stopped again while answering its challenge (run 3).
    - **Run 3:** this stop left the agent's own SELECT unanswered on top of the stale STATUS reply. The next agent discarded 178 bytes and registered 8 s after the stop.
- **No warning when nothing is stale.** No `discarded stale bytes` warning appeared on any open without a stale reply: two container starts, each with the agent's challenge open and the relay's sessions.

**Found along the way (pre-existing; this branch doesn't change either):**

- **With the host's pcscd (`pcscd = "external"`) on this desktop, the line could not register again after an agent restart.** After the first registration the relay started another USIM session, and pcscd never sent it `PowerOff`. So the relay kept the modem port, and the restarted agent waited 30 s (`modem serial busy (vowifi-usim-bridge is probably mid-EAP); waiting`), exited and restarted, over and over. The host pcscd's other clients were Firefox, Cursor and Claude Desktop, and charon was not connected, so a desktop PC/SC client holding the virtual card is the likely cause (not proven). With the container's own pcscd (`managed`, `vpcd_port = 25963`) the relay lets go of the port after EAP-AKA, and every restart above registered.
- **The container's own pcscd cannot start while the host's pcscd holds a USB reader.** It logs `Failed adding USB device: … OMNIKEY`, and the supervisor reads that as a vpcd bind failure: `FATAL: pcscd's vpcd reader never came up … (DriverBindFailed)`. For these runs the empty OMNIKEY was unbound from USB.
