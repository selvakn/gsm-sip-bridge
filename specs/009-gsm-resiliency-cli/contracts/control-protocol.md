# Control Protocol: CLI ↔ Daemon

**Feature**: 009-gsm-resiliency-cli | **Date**: 2026-05-17

## Transport

- **Mechanism**: Unix domain socket (SOCK_STREAM)
- **Default path**: `/tmp/gsm-sip-bridge.sock` (configurable via `[control] socket_path`)
- **Framing**: Newline-terminated JSON. One command per connection; daemon writes one response then closes the connection.
- **CLI timeout**: 35 seconds (covers 30 s `card restart` worst case + 5 s headroom)
- **Concurrency**: One command processed at a time; OS accept queue handles concurrent CLI invocations

## Commands (CLI → Daemon)

All commands are a single line of JSON followed by `\n`.

### `card_restart`

```json
{"cmd": "card_restart", "slot": 0}
```

Triggers teardown and re-initialization of the specified slot. Resets give-up state if present.

### `set_mode`

```json
{"cmd": "set_mode", "slot": 0, "mode": "4g"}
```

Valid `mode` values: `"2g"`, `"3g"`, `"4g"`, `"auto"`

Applies the network mode to the modem via AT command, persists to `card_mode_prefs` table. Fails if slot is not in `Ready` state.

### `get_mode`

```json
{"cmd": "get_mode", "slot": 0}
```

Returns the stored network mode preference for the slot. Returns `"auto"` if no preference has been set.

### `list_slots`

```json
{"cmd": "list_slots"}
```

Returns current state of all known slots (both active and recovering/given-up).

## Responses (Daemon → CLI)

All responses are a single line of JSON followed by `\n`.

### Success (no data)

```json
{"ok": true}
```

Used for: `card_restart` (after re-init completes).

### Success with mode

```json
{"ok": true, "mode": "4g"}
```

Used for: `get_mode`, `set_mode` (echoes the confirmed mode after AT verification).

### Success with slot list

```json
{
  "ok": true,
  "slots": [
    {"slot": 0, "state": "Ready",     "phone": "+91XXXXXXXXXX", "network": "4G/LTE"},
    {"slot": 1, "state": "Recovering","phone": "Unknown",       "network": "No Signal"}
  ]
}
```

Used for: `list_slots`.

### Error

```json
{"ok": false, "error": "slot 5 not found"}
{"ok": false, "error": "slot 0 is not in Ready state (current: Recovering)"}
{"ok": false, "error": "AT command timeout while applying mode"}
```

## Error Conditions

| Condition | Error message |
|-----------|---------------|
| Invalid slot number | `"slot <N> not found; valid slots: 0..=<max>"` |
| Slot not Ready for mode commands | `"slot <N> is not in Ready state (current: <state>)"` |
| AT command failure | `"AT command failed: <error>"` |
| Daemon not running | CLI-side only: prints `"daemon not running (socket not found at <path>)"` and exits 1 |
| CLI timeout | CLI-side only: prints `"timed out waiting for daemon response"` and exits 1 |

## CLI Exit Codes

| Exit code | Meaning |
|-----------|---------|
| 0 | Command succeeded |
| 1 | Command failed (daemon returned error, socket unreachable, timeout, invalid args) |
