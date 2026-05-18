# Tasks: GSM Modem Resiliency & CLI Utilities

**Branch**: `009-gsm-resiliency-cli` | **Date**: 2026-05-17 | **Plan**: [plan.md](./plan.md)

## Task List

### T01 — Add `[resilience]` and `[control]` config sections

**Files**: `gsm-sip-bridge/src/config/mod.rs`

Add `ResilienceConfig` and `ControlConfig` structs, parse from TOML with documented defaults, wire into `AppConfig`. Add "resilience" and "control" to `TOP_LEVEL_SECTIONS`.

Defaults:
- `resilience.initial_backoff_sec = 5`
- `resilience.max_backoff_sec = 120`
- `resilience.max_retries = 10`
- `resilience.network_loss_timeout_sec = 60`
- `resilience.network_poll_interval_sec = 30`
- `control.socket_path = "/tmp/gsm-sip-bridge.sock"`

Tests: parse a TOML string with and without `[resilience]` section; verify defaults applied; verify out-of-range values rejected.

---

### T02 — Extend AT commander with diagnostic and mode commands

**Files**: `gsm-sip-bridge/src/modules/at_commander.rs`

Add methods:
- `query_imei() -> BridgeResult<String>` — `AT+CGSN`, parse first non-OK line
- `query_phone_number() -> BridgeResult<String>` — `AT+CNUM`, parse `+CNUM:` line, return `"Unknown"` on error
- `query_network_type() -> BridgeResult<NetworkType>` — `AT+QNWINFO`, parse act field, map to `NetworkType` enum
- `query_network_mode() -> BridgeResult<NetworkMode>` — `AT+QCFG="nwscanmode"`, parse integer
- `set_network_mode(mode: NetworkMode) -> BridgeResult<NetworkMode>` — set + verify

Add `NetworkType` enum (FourGLte, ThreeGUmts, TwoGEdge, NoSignal, NoSim, Unknown) and `NetworkMode` enum (Auto, Gsm, Wcdma, Lte) with `Display` and `FromStr` impls.

Tests: use `AtCommander::from_stream` with in-memory pipe; test each response format including error cases.

---

### T03 — Add `card_slots` and `card_mode_prefs` DB tables (schema v2)

**Files**: `gsm-sip-bridge/src/store/schema.rs`, `gsm-sip-bridge/src/store/slots.rs` (new), `gsm-sip-bridge/src/store/mod.rs`

- Update `SCHEMA_SQL` to add the two new tables with `CREATE TABLE IF NOT EXISTS`
- Bump `SCHEMA_VERSION` to `"2"`
- Add migration: if version is "1", update to "2" (additive; no data change needed)
- Create `store/slots.rs` with functions:
  - `lookup_slot(conn: &Connection, imei: &str) -> BridgeResult<Option<u32>>`
  - `assign_slot(conn: &Connection, imei: &str, usb_serial: &str) -> BridgeResult<u32>`
  - `get_mode_pref(conn: &Connection, slot: u32) -> BridgeResult<Option<NetworkMode>>`
  - `set_mode_pref(conn: &Connection, slot: u32, mode: NetworkMode) -> BridgeResult<()>`
- Add `StoreCommand` variants: `UpsertSlot`, `SetModePref`
- Add synchronous query helpers on `StoreHandle` for `lookup_slot` and `get_mode_pref` (needed at startup before async context)

Tests: `tempfile` SQLite; test v1→v2 migration; test slot assignment, lookup, mode pref CRUD.

---

### T04 — Add control protocol types

**Files**: `gsm-sip-bridge/src/control/mod.rs` (new), `gsm-sip-bridge/src/control/protocol.rs` (new)

Define:
```rust
pub enum ControlCmd { CardRestart { slot: u32 }, SetMode { slot: u32, mode: NetworkMode }, GetMode { slot: u32 }, ListSlots }
pub enum ControlResp { Ok, OkMode { mode: NetworkMode }, OkSlots { slots: Vec<SlotInfo> }, Err { error: String } }
pub struct SlotInfo { pub slot: u32, pub state: String, pub phone: String, pub network: String }
```

Derive `serde::{Serialize, Deserialize}` on all types. Use `#[serde(tag = "cmd")]` for `ControlCmd` and `#[serde(tag = "ok", ...)]` for `ControlResp`. Expose `read_cmd` / `write_resp` helpers that do newline-framed JSON I/O on a `BufRead + Write`.

Tests: round-trip serialization for each variant.

---

### T05 — Add control socket server

**Files**: `gsm-sip-bridge/src/control/server.rs` (new)

Tokio task that:
1. Binds `UnixListener` at configured socket path (removes stale socket file on startup)
2. Accepts connections in a loop
3. Reads one `ControlCmd`, sends it to `CardPool` via `tokio::sync::mpsc::Sender<ControlCmd>`
4. Awaits response via `oneshot::Sender<ControlResp>` (embed in command enum)
5. Writes `ControlResp` and closes connection
6. Exits cleanly on shutdown signal

Expose `start_control_server(path, cmd_tx, shutdown_rx) -> tokio::task::JoinHandle<()>`.

Tests: in-process `UnixStream` pair; send `list_slots` command, verify response received.

---

### T06 — Extend `CardPool` with slot lifecycle management

**Files**: `gsm-sip-bridge/src/modules/mod.rs`, `gsm-sip-bridge/src/modules/card.rs`

This is the largest task. Changes:

1. Add `SlotState` struct (slot, imei, phone_number, network_type, network_mode, lifecycle_state, retry_count, next_retry_at)
2. Add `LifecycleState` enum (Initializing, Ready, Recovering, GivenUp)
3. Replace the `active`/`failed` Vec pattern with `slots: HashMap<u32, SlotState>`
4. On module discovery: query IMEI via AT, look up in DB, assign/restore slot
5. In `try_init_module`: query phone number + network type; apply stored network mode before returning Ok
6. Implement exponential backoff per slot: `backoff_delay(attempt, initial, cap) -> Duration`
7. Implement give-up: after `max_retries` consecutive failures, set `LifecycleState::GivenUp`, emit `tracing::error!`
8. Print startup diagnostics table after all modules initialized (before "card pool running")
9. Add `mpsc::Receiver<ControlCmd>` to the `CardPool::run` select loop
10. Handle `ControlCmd::CardRestart`: reset slot state → Initializing, trigger immediate re-init
11. Handle `ControlCmd::SetMode`: apply AT command + save to DB + verify, respond with OkMode or Err
12. Handle `ControlCmd::GetMode`: read from DB, respond with OkMode
13. Handle `ControlCmd::ListSlots`: collect SlotInfo for all slots, respond with OkSlots
14. Add periodic network registration poll: every `network_poll_interval_sec` seconds, send `AT+CREG?` to each Ready module; if registration lost for `network_loss_timeout_sec`, transition to Recovering

Tests: unit test `backoff_delay` function; unit test state transitions; integration test startup diagnostics output via tracing subscriber.

---

### T07 — Add `card` CLI subcommands

**Files**: `gsm-sip-bridge/src/cli.rs`, `gsm-sip-bridge/src/control/client.rs` (new)

In `cli.rs`:
- Change `Cli` to support an optional `#[command(subcommand)] command: Option<Commands>`
- Add `Commands::Card(CardArgs)` with subcommands:
  - `restart --slot <N>`
  - `set-mode --slot <N> --mode <2g|3g|4g|auto>`
  - `get-mode --slot <N>`
  - `list`

In `control/client.rs`:
- `send_cmd(socket_path: &str, cmd: ControlCmd) -> Result<ControlResp, String>` — blocking `UnixStream` connect + write cmd + read resp with 35 s timeout

In `main.rs`:
- If `cli.command` is `Some(Commands::Card(...))`, build `ControlCmd`, call `client::send_cmd`, print result, exit
- Otherwise run the existing daemon startup path (no change to daemon behavior)

Tests: `send_cmd` with in-process socket stub; invalid slot / invalid mode argument rejected by clap before socket call.

---

### T08 — Wire control server into daemon startup

**Files**: `gsm-sip-bridge/src/main.rs`

1. Create `mpsc::channel::<ControlCmd>()` (bounded, capacity 8)
2. Start control socket server task before `CardPool::run`
3. Pass `cmd_rx` into `CardPool::run`
4. Pass `cmd_tx` into `start_control_server`
5. Abort control server task on shutdown (alongside existing pool_handle and metrics_handle)

Tests: startup integration test: start daemon, send `list_slots` via control socket, verify response.

---

### T09 — Tests and pre-commit checklist

Run `make format && make lint && make test` and fix all issues. Ensure:
- All new public functions have at least one test
- `AT+CGSN`, `AT+CNUM`, `AT+QNWINFO`, `AT+QCFG` response parsing covered
- Schema v1→v2 migration covered
- Control protocol round-trip covered
- `cargo clippy -p gsm-sip-bridge -- -D warnings` passes
- `cargo fmt --check` passes

## Implementation Order

```
T01 → T02 → T03 → T04 → T05 → T06 → T07 → T08 → T09
```

Each task can be committed independently after `make test` passes.
