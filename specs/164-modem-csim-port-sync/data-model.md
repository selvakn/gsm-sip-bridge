# Data Model: Modem Port Sync and the AKA Budget

All state is in memory, per process. Nothing is persisted or reported over IPC.

## Port session (`at_worker::Session`)

| Field | Type | Change | Meaning |
|---|---|---|---|
| `port` | `Box<dyn ReadWrite + Send>` | unchanged | The open serial port (or a test stream) |
| `rx` | `Vec<u8>` | unchanged | Bytes read but not yet taken as a line |
| `desynced` | `bool` | meaning narrowed | Set by any timeout or read error. **Cleared only by a successful sync.** While set, the next command syncs first |
| `timeout` | `Duration` | unchanged | The port's response timeout, which is also the sync bound (D3) |
| `label` | `String` | **new** | The port path for logs (`"stream"`/`"test"` in tests) |

### States

```text
            open / reopen
                 │
                 ▼
           ┌───────────┐  sync ok   ┌──────────┐
           │ unsynced  │──────────▶│ in step  │◀──────────────┐
           └───────────┘            └──────────┘               │
                 │ sync fails           │ command times out,   │ sync ok
                 ▼                      │ overflows, read error │
         open fails:                    ▼                      │
         PortOutOfStep           ┌────────────┐  next command  │
         (worker exits           │  desynced  │────── sync ────┘
          after its bound)       └────────────┘
                                        │ sync fails
                                        ▼
                         command fails "AT command timeout";
                         stays desynced; AtCommander → Suspect
```

## Sync exchange

| Step | Sent | Accepted as ours | Everything else |
|---|---|---|---|
| 0 | - | - | `rx` cleared, counted as stale |
| 1 | `AT+CMEE?\r\n` | a line starting `+CMEE:`, then `OK` | discarded and counted (stale replies, echoes, `ERROR`, URCs) |
| 2 | `AT+CSCS?\r\n` (only after step 1) | a line starting `+CSCS:`, then `OK` | discarded and counted (including our own late `+CMEE:` after a stale one) |

- **Bound:** one deadline, `now + timeout`, across both steps.
- **Result:** `Ok(discarded_bytes)` or `Err(Discovery("AT command timeout"))`.
- **Context label in the warning:** `open` (via `Request::Sync`), `resync` (via `Request::Resync`), `after-timeout` (in `send_command_within`).

## Worker requests (`at_worker::Request`)

| Variant | Change | Handled by |
|---|---|---|
| `Command { cmd, budget, reply }` | unchanged (syncs first when `desynced`) | `send_command_within` |
| `ReadLine { reply }` | unchanged (no sync) | `read_line_raw` |
| `Resync { reply }` | now backed by `sync(timeout)` | `try_resync` |
| `Sync { reply }` | **new** | `start_worker` (open and reopen) |

## Errors (`error::BridgeError`)

| Variant | Display | Built by | Read by |
|---|---|---|---|
| `Discovery("AT command timeout")` | `discovery error: AT command timeout` | worker timeout, `dispatch` timeout, failed in-session sync | `contains("timeout")` classifiers (unchanged) |
| **`PortOutOfStep(detail)`** (new) | `discovery error: AT command timeout: {detail}` | `AtCommander::start_worker` only (open and reopen sync failed or timed out) | `is_port_out_of_step()` in the three open-retry loops; `contains("timeout")` classifiers, which read it as a timeout |
| `Ims("AT+CSIM failed: …")` | unchanged | `usim::csim_within` on `ERROR`/`+CME ERROR` | `sim_recovery::has_csim_failure`, `usim_bridge` SIM reset (unchanged) |

## Budgets

| Name | Value | Applies to |
|---|---|---|
| `DEFAULT_TIMEOUT` | 5 s (unchanged) | every command, and the sync of a default open |
| `PROBE_TIMEOUT` | 800 ms (unchanged) | discovery opens and their sync |
| **`AKA_RESPONSE_BUDGET`** | `{ max_lines: MAX_RESPONSE_LINES (256), timeout: 20 s }` | AUTHENTICATE and its GET RESPONSE on a modem: agent AKA (via `transmit_aka_apdu`) and bridge relay (INS `0x88` and its `61XX` chase) |
| `PER_MESSAGE_PORT_BUDGET` | 30 → **35 s** | one sweep message's port work |
| `SWEEP_PASS_BUDGET` | 60 → **72 s** | one sweep pass (still ≥ 20% under `Phase::SmsSweep` = 90 s) |
| `Phase::Renewal` | 360 s (unchanged) | derived worst case 214 → 269 s |

## SIM timings (`ims::OpenTimings`, carried in `OpenedCard.opened`)

| Field | Type | Meaning |
|---|---|---|
| `open` | `Duration` | Opening the port or reader (including the sync) and reading identities |
| `select` | `Option<Duration>` | Modem: EF_DIR walk plus SELECT MF/AID. Card reader: `None` (it selects inside the AKA) |

The AKA duration is measured around `run_aka` at each call site. All three are logged in one debug line per AKA run, with the outcome `success`, `sync-failure` or `error`.
