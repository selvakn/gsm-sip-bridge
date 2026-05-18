# Research: GSM Modem Resiliency & CLI Utilities

**Feature**: 009-gsm-resiliency-cli | **Date**: 2026-05-17

## 1. EC20 AT Commands for Diagnostics

### IMEI Query

- **Decision**: Use `AT+CGSN`
- **Rationale**: Standard 3GPP command universally supported on EC20. Returns a single line with the 15-digit IMEI. `AT+GSN` is an alias but `AT+CGSN` is preferred in Quectel documentation.
- **Response format**: `<IMEI>\r\nOK` — parse the first non-empty non-"OK" line.
- **Alternatives considered**: `AT+QGSN` (Quectel proprietary) — unnecessary when `AT+CGSN` works.

### Phone Number Query

- **Decision**: Use `AT+CNUM`
- **Rationale**: 3GPP standard subscriber number query. EC20 returns `+CNUM: "","+91XXXXXXXXXX",145` or `+CNUM: "","","unknown"` if the number is not provisioned on the SIM.
- **Response format**: Parse the second quoted field from the `+CNUM:` response line.
- **Fallback**: If `+CNUM` returns `ERROR` (SIM not ready, no SIM, PIN locked), return `Unknown`. Never block startup.
- **Alternatives considered**: `AT+QNUM` — not on all firmware variants.

### Network Type Query

- **Decision**: Use `AT+QNWINFO`
- **Rationale**: Quectel-proprietary but EC20-specific and returns the exact access technology string ("FDD LTE", "WCDMA", "GSM", etc.) rather than a numeric code that needs a lookup table. More actionable than `AT+CREG?` stat field.
- **Response format**: `+QNWINFO: "<act>","<oper>","<band>",<channel>` — parse the first quoted field.
- **Mapping**:
  | `<act>` substring | Display string |
  |-------------------|----------------|
  | `LTE` | `4G/LTE` |
  | `WCDMA` or `UMTS` or `HSPA` | `3G/UMTS` |
  | `GSM` or `GPRS` or `EDGE` | `2G/EDGE` |
  | (empty or error) | `No Signal` |
- **Alternatives considered**: `AT+CREG?` stat field (0/1/2/5) — less precise, does not distinguish 3G vs 4G. `AT+COPS?` act field — numeric, requires mapping table.

### Network Mode Query & Set

- **Decision**: Use `AT+QCFG="nwscanmode"`
- **Rationale**: EC20 standard mechanism for network mode preference. Persists to modem NVRAM (applies across modem power cycles, but we also store it in our DB for explicit re-application).
- **Get**: `AT+QCFG="nwscanmode"` → `+QCFG: "nwscanmode",<value>` — parse the integer.
- **Set**: `AT+QCFG="nwscanmode",<value>` → `OK`
- **Mode mapping**:
  | CLI `--mode` | AT value | Description |
  |--------------|----------|-------------|
  | `auto` | `0` | Automatic (default) |
  | `2g` | `1` | GSM only |
  | `3g` | `2` | WCDMA only |
  | `4g` | `3` | LTE only |
- **Post-set verification**: After `set`, immediately issue `AT+QCFG="nwscanmode"` (get) and confirm the value matches. Then issue `AT+COPS=0` to trigger re-registration in the new mode (optional but improves speed of mode switch).
- **Alternatives considered**: `AT+COPS=<oper>` manual operator select — too complex for mode switching. `AT+QNWPREFMODE` — not on EC20 firmware.

### Network Registration Status for Loss Detection

- **Decision**: Enable URC with `AT+CREG=1` and `AT+CEREG=1` during module init, AND poll `AT+CREG?` / `AT+CEREG?` every 30 s as fallback.
- **Rationale**: URC (`+CREG: 0` / `+CEREG: 0`) gives fast notification of network drop; periodic poll catches cases where URC is missed due to serial line glitch. 30 s poll interval is well within the 60 s detection timeout (FR-002 default).
- **Registration stat values**: 0=not registered, 1=registered home, 2=searching, 3=denied, 5=registered roaming. Values 0/2/3 after a configurable timeout = loss detected.
- **Alternatives considered**: Purely event-driven (URC only) — risky if URC is lost on noisy serial line. Purely poll-based — adds latency.

## 2. USB Hotplug Detection

- **Decision**: Leverage the existing serial read-error path for disconnect detection; add a periodic USB rescan for reconnect detection.
- **Rationale**: When a USB device is unplugged, the serial port returns an I/O error on the next read. The existing `run_module_loop` already propagates this error, causing the worker to exit and `CardPool` to detect the module failure via `JoinSet::join_next()`. This is inherently within 1–2 s of the unplug event (faster than the 5 s FR-001 requirement). For reconnect, `scan_modules()` already reads `/sys/bus/usb/devices` — adding a dedicated rescan task on a short interval (5 s) is sufficient and avoids the complexity of `rusb` hotplug callbacks.
- **Alternatives considered**: `rusb` async hotplug callback — requires `libusb` with hotplug support compiled in, adds OS-level callback complexity; YAGNI given the existing approach already meets the timing requirement. `inotify` on `/sys/bus/usb/devices` — works but adds an `inotify` crate dependency; poll at 5 s is simpler and sufficient.

## 3. Control Socket Protocol

- **Decision**: Newline-framed JSON over Unix domain socket. Each command is one JSON object ending with `\n`; each response is one JSON object ending with `\n`.
- **Rationale**: Zero custom framing complexity; human-readable for debugging with `nc -U /tmp/gsm-sip-bridge.sock`; `serde_json` (already a dep) handles serialization. Sequential (one command per connection) avoids concurrency complexity on both sides.
- **Socket path default**: `/tmp/gsm-sip-bridge.sock` (writable without root; configurable via `[control] socket_path` in TOML).
- **Command set**:
  ```json
  {"cmd": "card_restart", "slot": 0}
  {"cmd": "set_mode",     "slot": 0, "mode": "4g"}
  {"cmd": "get_mode",     "slot": 0}
  {"cmd": "list_slots"}
  ```
- **Response format**:
  ```json
  {"ok": true}
  {"ok": false, "error": "slot 5 not found"}
  {"ok": true, "mode": "4g"}
  {"ok": true, "slots": [{"slot": 0, "state": "Ready", "phone": "+91...", "network": "4G/LTE"}]}
  ```
- **Timeout**: CLI waits up to 35 s for response (covers 30 s `card restart` + 5 s headroom).
- **Alternatives considered**: HTTP REST on localhost — adds `axum` router overhead for management traffic; fine for metrics but overkill here. Signals + shared file — not request/response; can't return errors.

## 4. Schema Migration Strategy

- **Decision**: Additive migration with explicit version check and in-place upgrade.
- **Current version**: `"1"` in `meta` table.
- **New version**: `"2"`.
- **Migration logic** in `schema::init_schema`:
  1. Run all `CREATE TABLE IF NOT EXISTS` statements (safe for v1 and v2).
  2. Read `meta.schema_version`.
  3. If `"1"`: run upgrade script (currently a no-op because new tables use `IF NOT EXISTS`), update `meta` to `"2"`.
  4. If `"2"`: proceed.
  5. If anything else: error (as before).
- **New tables**:
  ```sql
  CREATE TABLE IF NOT EXISTS card_slots (
      slot         INTEGER PRIMARY KEY,
      imei         TEXT    NOT NULL UNIQUE,
      usb_serial   TEXT    NOT NULL DEFAULT '',
      registered_at TEXT   NOT NULL
  );

  CREATE TABLE IF NOT EXISTS card_mode_prefs (
      slot  INTEGER PRIMARY KEY REFERENCES card_slots(slot),
      mode  TEXT    NOT NULL CHECK (mode IN ('2g','3g','4g','auto'))
  );
  ```
- **Alternatives considered**: Embedded migration framework (e.g., `refinery`) — adds a crate dep for two migrations; manual approach is simpler per Principle V.

## 5. Exponential Backoff Design

- **Decision**: Per-slot backoff state: `attempt: u32`, next delay = `min(initial * 2^attempt, cap)`. Implemented inline in `CardPool` loop, no external crate.
- **Defaults** (from `[resilience]` config):
  - `initial_backoff_sec = 5`
  - `max_backoff_sec = 120`
  - `max_retries = 10`
- **Give-up**: After `max_retries` consecutive failures, slot state → `GivenUp`. Emit `tracing::error!` with slot + IMEI. Stop retrying until a `card_restart` command resets the counter.
- **Manual override**: `card restart` sets `attempt = 0`, `state = Initializing`, and triggers an immediate re-init (does not wait for next backoff deadline).
- **Alternatives considered**: `tokio-retry` crate — adds dep; simple `2^n` math inline is 5 lines.

## 6. Startup Diagnostics Format

- **Decision**: One `tracing::info!` line per slot before the "card pool running" message; also printed to stdout via `eprintln!` when the process has a terminal attached (detected by `std::io::stderr().is_terminal()` — part of Rust stdlib since 1.70).
- **Format**:
  ```
  [Slot 0] +91XXXXXXXXXX  4G/LTE
  [Slot 1] Unknown        No Signal
  [Slot 2] No SIM         No SIM
  ```
- **Structured log fields**: `slot`, `phone_number`, `network_type`, `imei`.
- **Alternatives considered**: A dedicated table print — adds formatting code without much user value for 1–8 rows.
