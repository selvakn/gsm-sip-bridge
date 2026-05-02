# Tasks: Multi EC20 Card Support

**Branch**: `004-multi-card-support` | **Plan**: [plan.md](plan.md) | **Spec**: [spec.md](spec.md)

## Task List

### Task 1: Extend DeviceInfo and discover_all_ec20()

**Files**: `src/device_discovery.h`, `src/device_discovery.cpp`, `tests/integration/test_device_discovery.cpp`

- Add `serial_number` and `usb_path` fields to `DeviceInfo`
- Implement `std::vector<DeviceInfo> discover_all_ec20()` that returns ALL matching USB devices instead of stopping at the first
- Read USB serial number from `/sys/bus/usb/devices/<dev>/serial` sysfs attribute
- Keep existing `discover_ec20()` as a wrapper that returns the first result
- Add tests for `discover_all_ec20()` (returns vector, handles 0/1/N devices)

**Acceptance**: `make test` passes. `discover_all_ec20()` returns all connected EC20 modules with serial numbers populated.

---

### Task 2: Refactor BridgeAccount for concurrent calls

**Files**: `src/bridge/bridge_account.h`, `src/bridge/bridge_account.cpp`

- Replace `std::unique_ptr<BridgeCall> active_call_` with a mutex-protected `std::map<int, std::unique_ptr<BridgeCall>>` keyed by PJSIP call ID
- Update `make_outbound_call()` to insert into the map and return a raw pointer
- Add `remove_call(int call_id)` to clean up a completed call
- Update `hangup_call()` to accept an optional call_id parameter (hangup specific call) or hangup all
- Update `onIncomingCall()` to reject inbound SIP calls (unchanged behavior, bridge is outbound-only)
- Existing single-card behavior preserved when only one call is active

**Acceptance**: `make test` passes. Existing bridge-tests still pass unchanged.

---

### Task 3: Implement CardInstance

**Files**: `src/bridge/card_instance.h`, `src/bridge/card_instance.cpp`

- Extract per-card logic from `bridge/main.cpp` into CardInstance class
- CardInstance owns: DeviceInfo, SerialPort, AtCommander, card_id (derived as `ec20-<last6chars of serial_number>`)
- `initialize()`: opens serial port, sends AT setup commands, checks network registration. Returns success/failure.
- `start(BridgeAccount&, BridgeConfig&, SipConfig&, atomic<bool>& running)`: launches a dedicated thread running the card's call-handling loop (poll URC, answer GSM, bridge to SIP)
- `stop()`: signals the thread to exit and joins it. Hangs up active call if any.
- The call-handling loop reuses the existing `handle_bridged_call()` pattern extracted from main.cpp
- All log messages prefixed with `[card_id]`

**Acceptance**: `make test` passes. CardInstance compiles and links into gsm-sip-bridge.

---

### Task 4: Implement CardPool

**Files**: `src/bridge/card_pool.h`, `src/bridge/card_pool.cpp`

- `discover_and_initialize(CliArgs&)`: calls `discover_all_ec20()`, creates CardInstance for each, attempts `initialize()`, separates into active/failed lists
- `print_summary()`: logs the startup summary (card_id, serial port, ALSA device, status for each)
- `start_all(BridgeAccount&, BridgeConfig&, SipConfig&, atomic<bool>& running)`: starts threads for all active cards
- `stop_all()`: stops all card threads and the retry thread
- `start_retry_thread(atomic<bool>& running)`: background thread that retries failed cards every 30 seconds. On success, moves card to active list and starts its thread.
- Returns error if no cards detected or all fail initialization

**Acceptance**: `make test` passes. CardPool compiles and links.

---

### Task 5: Refactor bridge main.cpp to use CardPool

**Files**: `src/bridge/main.cpp`

- Replace single-device flow with CardPool
- Keep CLI arg parsing, config loading, PJSIP init, BridgeAccount creation unchanged
- After PJSIP is ready: `pool.discover_and_initialize(args)` → `pool.print_summary()` → `pool.start_all(account, ...)` → `pool.start_retry_thread(...)` → wait for signal → `pool.stop_all()`
- Remove single-device `resolve_device()`, `handle_bridged_call()`, and the main event loop (now inside CardInstance)
- When `--serial` and `--audio` are both provided, fall back to single-card mode (create one CardInstance manually with the overridden device)
- Update version to 2.0.0

**Acceptance**: `make build` succeeds. `make test` passes. Single-module behavior unchanged.

---

### Task 6: Add integration tests for multi-card scenarios

**Files**: `tests/integration/test_card_pool.cpp`, `CMakeLists.txt`

- Test CardPool discovery with 0 devices (returns error)
- Test CardInstance card_id derivation from serial number
- Test CardPool startup summary formatting
- Test that CardPool handles partial initialization (mix of active/failed)
- Add `test_card_pool.cpp` to bridge-tests in CMakeLists.txt

**Acceptance**: `make test` passes with new tests included.

---

### Task 7: Update README and config documentation

**Files**: `README.md`

- Document multi-card support in the README
- Add section explaining auto-detection of multiple EC20 modules
- Document the stable card identifier scheme
- Document retry behavior for failed modules
- Update the example startup output to show multiple cards

**Acceptance**: README accurately reflects the new multi-card capabilities.
