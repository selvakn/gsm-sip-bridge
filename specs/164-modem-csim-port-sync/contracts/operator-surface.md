# Operator Surface: Modem Port Sync and the AKA Budget

What this feature adds to the logs, and what it must leave exactly as it is. No metric, alert, status file, config key or CLI flag changes.

## New log lines

### Stale bytes discarded before the channel was in step (warning)

```text
WARN discarded stale bytes before the AT channel was in step port=/dev/ttyUSB0 discarded_bytes=27 context=open
```

| Field | Values |
|---|---|
| `port` | the port path |
| `discarded_bytes` | > 0: bytes read during the sync that were not this sync's own two replies |
| `context` | `open` (open or reopen), `after-timeout` (the next command after a timeout in the same session), `resync` (suspect-channel recovery) |

- Logged at most once per sync, and only when bytes were discarded.
- A sync that discards nothing logs at debug level only.
- Meaning: an earlier command on this port (from this process or another) was abandoned and its reply arrived late; or an unsolicited result code arrived during the sync's short window.
- Frequent `context=open` lines on one port point at another holder that times out and closes mid-command.

### SIM challenge timings (debug)

```text
DEBUG SIM challenge timings open_ms=1412 select_ms=1460 authenticate_ms=6230 outcome=success
```

| Field | Values |
|---|---|
| `open_ms` | time to open the port or reader (including the sync) and read identities |
| `select_ms` | modem: EF_DIR walk plus SELECT; card reader: absent |
| `authenticate_ms` | AUTHENTICATE plus its GET RESPONSE |
| `outcome` | `success`, `sync-failure` (the SIM answered with AUTS), `error` |

- One line per AKA run: refresh challenge, full registration, and the full registration's AUTS retry.
- It is debug level, so it is visible with `RUST_LOG=…,gsm_sip_bridge::ims=debug`.

## Error text that must not change

| Text | Where it comes from | Who reads it |
|---|---|---|
| `AT command timeout` | worker timeout, `dispatch` timeout, a failed in-session sync, and inside the new `PortOutOfStep` text | `contains("timeout")`: `modules/worker.rs`, `bin/gsm_echo.rs`, `ims/session.rs::map_registration_error` |
| `AT+CSIM failed: …` | an explicit `ERROR`/`+CME ERROR` to `AT+CSIM`, on any budget | `supervise::sim_recovery::has_csim_failure`, `vowifi-usim-bridge` in-place SIM reset |
| `re-authentication failed; registering in full reason=the SIM could not answer the challenge: …` | the refresh fallback | operators (unchanged wording; the reason now appears only after 20 s, not 5 s) |

## Changed error text

| Before | After | When |
|---|---|---|
| (an open never failed on a responsive port lock) | `discovery error: AT command timeout: <port> did not answer its sync within <N> ms` | opening a port whose modem does not complete the sync within its bound |

## Behaviour an operator can notice

- **A slow SIM.** A modem line's refresh challenge now waits up to 20 s for the SIM's answer (per command) before falling back to a full registration. `re-authentication failed … AT command timeout` now means the SIM took more than 20 s.
- **An unresponsive modem.** It fails at the open (the sync) rather than at the first command, after the same bound. The open-retry loops do not retry it: the SMS sweep tries again next interval, and the agent's open fails at once instead of waiting 30 s.
- **The CS pool's fallback restart** is not sent to a modem that fails the sync. The existing warning `could not open modem port for fallback restart` is logged instead.
- **The SMS sweep** gives one message's port work up to 35 s (was 30 s), and a pass up to 72 s (was 60 s).
