# Research: A Hung Modem No Longer Costs a Modem Line Its Registration

Decisions for [plan.md](plan.md). Paths are relative to `gsm-sip-bridge/src`; line numbers are from `a2ef631`, where every file cited is the same as on `main` (`5334fa8`).

## Findings

### F1. The live stall was in the open

2026-11-28 14:07–14:09 UTC, modem USB/IP traffic dropped at the router for 90 s, device left attached:

- watchdog: `activity="sms-sweep" phase="sms-sweep" stalled_secs=94 budget_secs=90 last_at_command=AT+CMGL=4 last_at_command_age_secs=114`.
- `record_last_at_command` (`modules/at_commander.rs:30`) runs in `send` (`:398`) for every command. It does not run for the open's sync (`Request::Sync`, `:296`). So the stalled pass, entered 94 s before the report, sent no command; the `AT+CMGL=4` was the previous pass's.
- Before its first command a pass does, in `sweep_modem_storage` (`volte/sms.rs:984`): `modem_lock.lock_timeout(budget.lock_wait())` (≤ 20 s), `open_with_retry` (`:897`: up to 4 opens, 1.8 s of sleeps, an out-of-step port returned at once), and inside each open `open_session` (`at_commander.rs:317`) then `start_worker` (`:285`, sync bounded by `recv_timeout(timeout + WORKER_GRACE)` = 7 s).
- Every one of those is bounded except `open_session`, which calls `serialport::new(..).open()` on the caller's thread. That is `open(2)`, then `ioctl(TIOCEXCL)`, `flock`, `tcgetattr`/`tcsetattr`, line-control `ioctl`s. The kernel logged `vhci_hcd: the urb (seqnum 733080) was already given back` at 14:09:12, when the connection was finally torn down.

**Confirmed live** with this feature, from `/proc/<agent>/task/*/stack` of the stranded `at-open-ttyUSB0`, on two repeats (2026-11-28). The open never gets past `open(2)` itself, in one of two places:

1. In the driver (third repeat, every thread's stack taken):

   ```
   usb_kill_urb ← usb_start_wait_urb ← usb_control_msg ← usb_wwan_send_setup ← usb_wwan_dtr_rts ← serial_port_dtr_rts ← tty_port_block_til_ready ← tty_port_open ← serial_open ← tty_open ← … ← __x64_sys_openat
   ```

   Opening a USB serial tty raises DTR (`tty_port_block_til_ready`), which `usb_wwan` sends as a synchronous USB control transfer. On a `vhci_hcd` device whose TCP connection is silent the transfer times out, and its cancellation (`usb_kill_urb`) waits for an unlink that has to travel over the same dead connection. No other thread of the agent was in the driver or held the port: there was no `at-port-ttyUSB0` worker.

2. Behind the tty's lock (the 180 s repeat, only the open's stack taken):

   ```
   tty_lock_interruptible ← tty_open ← chrdev_open ← … ← __x64_sys_openat
   ```

   The tty still existed, and something held its lock. The kernel holds it through `tty_release` while the driver closes the port, which drops DTR through the same kind of control transfer, so a close stuck as in (1) is the likely holder; it was not captured.

Either way the caller's thread never returns from `open(2)`, which is why the stalled pass of 14:07 never reached its first command. The design bounds the whole `open_session`, so it covers both.

### F2. Every open goes through `open_session`

`grep -rn 'AtCommander::open\(_with_timeout\)\?(' --include=*.rs .` (production only): 25 sites, among them `ims/mod.rs:1174` (agent registration, refresh challenge, startup), `volte/sms.rs:898` (sweep and signal poll), `vowifi/usim_bridge.rs:551`, `vowifi/plmn.rs:121`, `vowifi/imsi.rs:22`, `vowifi/ims_mode.rs:119,150,163`, `vowifi/discovery.rs:718`, `modules/discovery/{probe.rs:147,sim.rs:89}`, `modules/{worker.rs:160,pool/mod.rs:722,pool/restart_cycle.rs:396}`, the `volte/` and `commands/volte.rs` paths, `ims/agent/mod.rs:322`, `bin/gsm_echo.rs:54`.

All of them reach `open_with_timeout` (`at_commander.rs:248`), which calls `open_session` (`:249`). The only other caller of `open_session` is `try_reopen` (`:602`). Fixing `open_session` covers every opener.

### F3. Open-retry loops and what they retry

| Loop | Where | On `PortOutOfStep` | On other errors |
|---|---|---|---|
| SMS sweep `open_with_retry_using` | `volte/sms.rs:905` | returns at once (`:912`) | 4 attempts, 0.3/0.6/0.9 s sleeps |
| `open_modem_waiting` | `ims/mod.rs:1050`, predicate at `:1175` | returns at once | until `MODEM_OPEN_MAX_WAIT` (30 s) on the clock |
| `usim_bridge::try_open_with_backoff` | `vowifi/usim_bridge.rs:210` | returns at once (`:216`) | `OPEN_RETRY_ATTEMPTS` with backoff |
| `AtCommander::try_reopen` | `modules/at_commander.rs:598` | **retried** (`Err(_) if attempt + 1 < REOPEN_ATTEMPTS`) | 20 attempts, 100 ms apart |
| `ims_mode::wait_for_reboot` | `vowifi/ims_mode.rs:132` | polls `modem_answers` until a deadline | same |

`try_reopen` is the one loop that would retry the new failure. It is bounded either way (one full open bound, then 19 immediate "earlier open outstanding" failures 100 ms apart), but FR-008 says no loop retries it, so it gets the same `is_port_out_of_step` early return.

### F4. Classifiers that read the open's errors

`grep -rn 'contains(\|starts_with(' --include=*.rs ims supervise modules volte vowifi | grep -i 'timeout\|serial\|lock\|step'`:

- `modules/worker.rs:253`, `:829` read `contains("timeout")`; `map_registration_error` reads `timeout` too. `PortOutOfStep` displays as `discovery error: AT command timeout: …` (`error.rs:48`), which they already read as a timeout, by design (#237).
- `supervise/sim_recovery.rs::has_csim_failure` reads `AT+CSIM failed`; the new messages do not contain it.
- Tests in `ims/mod.rs:2384–2501` script `Unable to acquire exclusive lock…` and `No such file or directory` as opener errors; those texts come from `serialport` and are passed through unchanged (FR-009).

### F5. Budgets derived from the open

The open is counted as instantaneous today. Adding its bound (`timeout + WORKER_GRACE`, 7 s by default) to each derivation:

| Budget | Where | Derivation today | With the open's bound | Today's value → needed |
|---|---|---|---|---|
| Sweep per-message port work | `volte/sms.rs:946`, test `:1356` | lock 20 + sleeps 1.8 + sync 5 + one command 5 = 31.8 | + 7 = 38.8 | 35 → **40** |
| Sweep pass | `:941`, tests `:1296`, `:1356` | ≥ 2 × per-message | 2 × 40 = 80 | 72 → **80** |
| Watchdog `SmsSweep` | `ims/agent/watchdog.rs:221`, test `sms.rs:1296` (≥ 20 % over the pass) | 72 × 1.2 = 86.4 | 80 × 1.2 = 96 | 90 → **100** |
| Listing step | test `sms.rs:1318` | lock + open + `AT+CMGF`, `AT+CNMI` + `BULK_LIST_BUDGET` must fit `SmsSweep` | + 7 | recomputed in the test |
| Watchdog `Renewal` | `watchdog.rs:209`, test `:700` (≥ 20 %) | 433 s worst | + 7 = 440 s; 540 / 440 = 1.227 | 540, unchanged |
| Watchdog `Startup` | `watchdog.rs:204`, test `:721` | renewal + PLMN read = 463 s | + 7 + 7 for the PLMN open = 477 s; 600 / 477 = 1.258 | 600, unchanged |

The tests already recompute these from the constants; they get the open's bound as one more term, from one constant (D2).

## Decisions

### D1. Run the device open on a thread of its own, and wait for it with a bound

`open_session` spawns a short-lived thread (`at-open-<port>`) that runs `serialport::new(..).timeout(timeout).open()` and sends the result back on a channel. The caller waits with `recv_timeout(open_bound)`. A result in time is handled exactly as today (FR-009). No result in time is FR-003's failure; the thread is left to finish on its own.

- **Rationale**: the same pattern the AT worker already uses for a read that may never return (specs/039): bound the caller, strand the thread. It needs no knowledge of which syscall blocks.
- **Alternatives**:
  - *Open with `O_NONBLOCK` and set the port up ourselves*: `serialport` owns the open sequence (`TIOCEXCL`, `flock`, termios), and `O_NONBLOCK` does not make `open(2)` skip the tty lock it waited on live (F1). Replacing the crate's open is a large change for no guarantee.
  - *Interrupt the blocked syscall with a signal*: process-wide signal handling in a multi-threaded process, and the kernel may restart or ignore the interrupted call inside the USB stack.
  - *Change the watchdog to let the sweep run on stalled* (#239's proposal 2): does not free the modem lock or the port the stuck thread holds, so the line fails later instead (spec, Assumptions).

### D2. The bound is the port's timeout plus `WORKER_GRACE`, from one function

`fn open_bound(timeout: Duration) -> Duration { timeout + WORKER_GRACE }` in `at_commander.rs`, plus `pub(crate) const DEFAULT_OPEN_BOUND` for the derivations (F5). 7 s at the default 5 s timeout; 2.8 s for discovery's 800 ms probe.

- **Rationale**: the open's sync already waits `timeout + WORKER_GRACE` (`start_worker`, `:302`), so a failed open costs at most what a failed sync does. It covers the kernel's 5 s USB control-transfer timeout, so a device that is slow but answering still opens.
- **Alternative**: a short fixed bound (2 s). Rejected: a refresh challenge whose open is merely slow would then fall back to a full registration, the outcome this feature exists to avoid.

### D3. The failure is out of step, with a variant of its own for discovery

On timeout: `BridgeError::PortOpenStalled(format!("{} did not open within {} ms", path.display(), bound.as_millis()))`. While an earlier open is outstanding: `PortOpenStalled(format!("an earlier open of {} has not returned", path.display()))`.

- **Rationale**: every open-retry loop already stops on `is_port_out_of_step()` (F3), and every text classifier already reads it as a timeout and not as a SIM fault (F4). A new kind would have to be added to each of them.
- **Revised during implementation**: discovery's AT and SIM probes (`modules/discovery/{probe,sim}.rs`) wrap the open in their own `run_bounded(probe_timeout)` (5 s by default, specs/030) and count a probe that times out toward the port's quarantine, while a failed open "responded" and resets the streak. With the open's own bound (2.8 s for the probe's 800 ms timeout) shorter than `run_bounded`'s, a hung port would have turned from "timed out" into "responded" and never been quarantined. So the open's failures are a new variant `BridgeError::PortOpenStalled` with the same text, which `is_port_out_of_step()` also recognises (every loop and classifier unchanged), and which the probes map to their timeout outcome through `is_port_open_stalled()`. A port that opens but fails its sync stays "not AT-capable", as since #237.
- **Alternative**: reuse `PortOutOfStep` and tell the two apart by message text. Rejected: classifiers by text are what the learnings warn against.

### D4. Only an open the caller gave up on blocks later opens

A process-wide `Mutex<Vec<OpenAttempt>>` lists the outstanding opens, each with its port path and id. An entry is added **only** when a caller's wait times out, and removed by that open's thread when it finishes. A new open checks the map first and fails at once if its port is there (FR-005).

- **Rationale**: two threads of one process opening the same port at the same time is normal (the sweep and a refresh challenge; they use different locks). Today the loser fails with the port's exclusive lock and its loop waits it out. Marking every open in progress would turn that into a non-retryable failure and break the challenge path.
- **The race**: the caller's timeout and the thread's completion can cross. Both sides take the map's mutex and use a per-open `finished` flag that the thread sets under it. The caller, on timeout, inserts the entry only if `finished` is still false; if the thread has finished, the caller takes the result with `try_recv` and handles it as a result in time. The thread, on finishing, sets `finished` and removes the entry only if it carries its own id. No stale entry can remain, and a later open's entry is never removed by an earlier thread.
- **Opens already under way when the port hangs** passed the check before any entry existed, so each of them may time out and add its own entry. That is the only way a port gets a second entry. Their number is the number of concurrent opens of the port at that moment, and it does not grow afterwards, which is the leak FR-005 is about. Making the check and the spawn exclusive per port would serialize every healthy open behind a lock for no gain.

### D5. A late result is closed before the entry is removed

The thread sends its result; if the caller is gone, `send` hands the result back, and the thread drops it (closing the port and its `flock`) before it takes the map's mutex and removes the entry (FR-004, FR-006). A result sent just before the caller gave up sits in the channel and is dropped with the receiver; the caller, under the mutex, sees `finished` and takes it instead (D4), so that case never leaves a port open.

### D6. `try_reopen` stops on `PortOutOfStep`

`Err(e) if e.is_port_out_of_step()` breaks out of `try_reopen`'s loop with its existing warning (F3, FR-008).

### D7. Budgets count the open (F5)

The derivations in `volte/sms.rs` and `ims/agent/watchdog.rs` tests add `DEFAULT_OPEN_BOUND` once per open. `PER_MESSAGE_PORT_BUDGET` 35 → 40 s, `SWEEP_PASS_BUDGET` 72 → 80 s, `Phase::SmsSweep` 90 → 100 s; `Renewal` and `Startup` are re-checked by their tests and raised only if a test says so. The release note names every value that moved (FR-011).

### D8. The watchdog's policy is unchanged

No per-activity exception (FR-011). With D1, a sweep pass on a hung modem ends in about 7 s (one bounded open) and then in about 0 s (outstanding open) for as long as the hang lasts, far inside its 100 s budget.

### D9. Tests use real threads and real blocking, no mocks

The opener is injected as a closure in an internal `open_session_with(path, timeout, opener)`; production passes the `serialport` call. Tests pass a closure that blocks on a channel the test controls (released or never), or returns a `UnixStream` half as the port, so the thread, the timeout, the map and the late drop are all real (constitution I). The drop of a late port is observed through the other half of the `UnixStream` reading EOF.
