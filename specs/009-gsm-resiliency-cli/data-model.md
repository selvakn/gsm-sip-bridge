# Data Model: GSM Modem Resiliency & CLI Utilities

**Feature**: 009-gsm-resiliency-cli | **Date**: 2026-05-17

## Database Schema Changes (v1 → v2)

### New Table: `card_slots`

Persists the IMEI→slot assignment so the same physical card always gets the same slot number.

```sql
CREATE TABLE IF NOT EXISTS card_slots (
    slot          INTEGER PRIMARY KEY,       -- 0-based slot index, assigned once
    imei          TEXT    NOT NULL UNIQUE,   -- 15-digit IMEI, stable hardware identity
    usb_serial    TEXT    NOT NULL DEFAULT '',  -- USB serial string (informational)
    registered_at TEXT    NOT NULL           -- ISO-8601 timestamp of first registration
);
```

- **Slot assignment rule**: `SELECT MAX(slot) + 1 FROM card_slots` (or 0 if empty). Capped at 7 (max 8 slots).
- **Lookup on plug-in**: `SELECT slot FROM card_slots WHERE imei = ?`. If found, reuse that slot. If not found, assign next available.
- **Uniqueness**: IMEI is globally unique per modem hardware; enforced at DB level.

### New Table: `card_mode_prefs`

Persists the operator's preferred network mode per slot, re-applied on every card initialization.

```sql
CREATE TABLE IF NOT EXISTS card_mode_prefs (
    slot  INTEGER PRIMARY KEY REFERENCES card_slots(slot),
    mode  TEXT    NOT NULL CHECK (mode IN ('2g','3g','4g','auto'))
);
```

- **Default**: No row = no preference stored = modem's current hardware setting is used.
- **On `set-mode`**: `INSERT OR REPLACE INTO card_mode_prefs(slot, mode) VALUES (?, ?)`.
- **On card init**: `SELECT mode FROM card_mode_prefs WHERE slot = ?` → if row exists, apply via AT command before declaring card ready.
- **On `get-mode`**: `SELECT mode FROM card_mode_prefs WHERE slot = ?` → return mode or `"auto"` (if no preference stored, the effective mode is auto).

### Schema Version Migration

| Version | Tables |
|---------|--------|
| v1 | `meta`, `calls`, `sms` |
| v2 | v1 + `card_slots`, `card_mode_prefs` |

Migration from v1→v2 is purely additive. On startup: if `meta.schema_version = '1'`, run the new `CREATE TABLE IF NOT EXISTS` statements and update `meta` to `'2'`. No data is lost.

## In-Memory State: `SlotState` enum

Each slot tracked in `CardPool` carries:

```
SlotState {
    slot: u32,
    imei: String,
    usb_serial: String,
    phone_number: String,          // "" = not yet queried, "Unknown" = query failed
    network_type: NetworkType,
    network_mode: Option<NetworkMode>,  // None = no stored preference
    state: LifecycleState,
    retry_count: u32,
    next_retry_at: Option<Instant>,
}
```

### `LifecycleState` enum

```
Initializing   → attempting first init after detection
Ready          → AT probe succeeded, worker running
Recovering     → worker exited or network lost; backoff retry in progress
GivenUp        → max retries exceeded; awaiting manual `card restart`
```

State transitions:
```
Initializing → Ready       (init succeeds)
Initializing → Recovering  (init fails, retry count < max)
Initializing → GivenUp     (init fails, retry count = max)
Ready        → Recovering  (worker exits / network loss detected)
Recovering   → Ready       (retry init succeeds)
Recovering   → Recovering  (retry fails, increment counter)
Recovering   → GivenUp     (retry fails, counter = max)
GivenUp      → Initializing (manual `card restart` issued)
```

### `NetworkType` enum

```
FourGLte      → display "4G/LTE"
ThreeGUmts    → display "3G/UMTS"
TwoGEdge      → display "2G/EDGE"
NoSignal      → display "No Signal"
NoSim         → display "No SIM"
Unknown       → display "Unknown"
```

### `NetworkMode` enum (operator preference)

```
Auto  → AT value 0
Gsm   → AT value 1   (CLI: "2g")
Wcdma → AT value 2   (CLI: "3g")
Lte   → AT value 3   (CLI: "4g")
```

## Control Protocol Types

See `contracts/control-protocol.md` for the full wire format. Rust types:

```
ControlCmd:
  CardRestart { slot: u32 }
  SetMode     { slot: u32, mode: NetworkMode }
  GetMode     { slot: u32 }
  ListSlots

ControlResp:
  Ok
  OkMode     { mode: NetworkMode }
  OkSlots    { slots: Vec<SlotInfo> }
  Err        { error: String }

SlotInfo { slot: u32, state: String, phone: String, network: String }
```

## Config Additions

### `[resilience]` TOML section

```toml
[resilience]
# Seconds before first retry after failure
initial_backoff_sec = 5
# Maximum backoff cap in seconds
max_backoff_sec = 120
# Give up after this many consecutive failures
max_retries = 10
# Seconds after which network registration loss is declared
network_loss_timeout_sec = 60
# Network registration poll interval in seconds
network_poll_interval_sec = 30
```

### `[control]` TOML section (new)

```toml
[control]
# Unix domain socket path for CLI↔daemon communication
socket_path = "/tmp/gsm-sip-bridge.sock"
```
