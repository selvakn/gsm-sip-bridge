# Feature Specification: Multi EC20 Card Support

**Feature Branch**: `004-multi-card-support`
**Created**: 2026-05-02
**Status**: Draft
**Input**: User description: "this module handles one gsm module (EC20). Now, add support for handling multiple ec20 (over usb) cards attached to the same host. During the boot, number of available cards should be detected (one or more) and incoming from all the cards should be forwarded to the SIP server."

## Clarifications

### Session 2026-05-02

- Q: Should the system periodically retry modules that failed initialization due to transient conditions (e.g., SIM not registered), or permanently exclude them until restart? → A: Periodically retry failed modules in the background.
- Q: Should card identifiers be stable across reboots, or is a simple sequential index acceptable? → A: Stable ID derived from hardware serial number, consistent across reboots.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Multi-Card Detection at Boot (Priority: P1)

An operator connects multiple Quectel EC20 USB GSM modules to a single Linux host, each with its own SIM card. When the system starts, it scans the USB bus and detects all connected EC20 modules (identified by vendor/product ID 2c7c:0125). For each module found, the system identifies the corresponding serial port and ALSA audio device. The system reports the total number of detected modules and their assigned identifiers. If no modules are found, the system exits with an error. If some modules fail initialization (e.g., missing audio device, SIM not registered), the system logs a warning for each failed module and continues operating with the remaining functional modules, provided at least one is available.

**Why this priority**: Without detecting all cards, the system cannot handle calls from multiple GSM lines. This is the foundational capability that all other stories depend on.

**Independent Test**: Connect two or more EC20 modules via USB. Start the system and verify it logs the detection of each module with its serial port and ALSA device. Disconnect one module, restart, and verify the system detects only the remaining module(s).

**Acceptance Scenarios**:

1. **Given** two EC20 modules are connected via USB, **When** the system starts, **Then** it detects both modules and logs their serial ports and ALSA devices with unique card identifiers.
2. **Given** three EC20 modules are connected but one has no SIM card inserted, **When** the system starts, **Then** it logs a warning for the SIM-less module and operates with the remaining two.
3. **Given** no EC20 modules are connected, **When** the system starts, **Then** it exits with a nonzero status code and a descriptive error message.
4. **Given** two EC20 modules are connected but one has a faulty USB audio interface, **When** the system starts, **Then** it logs a warning for the faulty module and operates with the remaining functional module.

---

### User Story 2 - Concurrent Call Handling Across Cards (Priority: P2)

Each detected EC20 module independently listens for incoming GSM calls. When a call arrives on any module, the system answers it and bridges it to the SIP server, exactly as it does today for a single module. Multiple modules can handle calls simultaneously -- if two GSM calls arrive on two different modules at the same time, both are bridged to the SIP server concurrently. Each call uses its own module's serial port and audio device.

**Why this priority**: Concurrent call handling is the core value of supporting multiple cards. However, detection (P1) must work first before calls can be handled.

**Independent Test**: With two EC20 modules connected, place a call to each SIM number simultaneously. Verify both calls are answered, both are bridged to SIP, and both have independent bidirectional audio without interference.

**Acceptance Scenarios**:

1. **Given** two EC20 modules are active, **When** a call arrives on module 1, **Then** the system answers and bridges it to SIP while module 2 remains idle and ready.
2. **Given** two EC20 modules are active, **When** calls arrive on both modules within seconds of each other, **Then** both calls are answered and bridged to SIP concurrently with independent audio paths.
3. **Given** module 1 is handling an active bridged call, **When** module 1's call ends, **Then** module 1 returns to idle without affecting module 2's ongoing call.
4. **Given** two concurrent bridged calls are active, **When** the SIP party on one call speaks, **Then** audio is heard only by the corresponding GSM caller (no cross-talk between modules).

---

### User Story 3 - Per-Card Status and Logging (Priority: P3)

Each module is assigned a stable, human-readable identifier derived from its hardware serial number. All log messages related to a specific module include this identifier so an operator can distinguish events from different cards. At startup, the system outputs a summary showing all detected modules and their status (active, failed, reason for failure).

**Why this priority**: Operational visibility is important for managing a multi-card deployment, but the system functions correctly without enhanced logging.

**Independent Test**: Start the system with three modules. Verify each log line during call handling identifies which module it pertains to. Verify the startup summary lists all three modules with status.

**Acceptance Scenarios**:

1. **Given** three EC20 modules are detected, **When** the system finishes initialization, **Then** it outputs a summary listing each module's identifier, serial port, ALSA device, and status (active/failed).
2. **Given** a GSM call arrives on module 2, **When** the system logs call events (ring, answer, bridge, hangup), **Then** each log entry includes module 2's identifier.
3. **Given** module 3 failed initialization, **When** the startup summary is displayed, **Then** module 3 is listed with status "failed" and the reason (e.g., "no network registration").

---

### Edge Cases

- What happens when all but one module fail initialization? The system operates with the single functional module and logs warnings for the failed ones.
- What happens when a module is physically disconnected (USB unplug) during operation? The system detects the loss on the next I/O operation on that module, logs an error, marks the module as unavailable, and continues operating with the remaining modules. Active calls on the disconnected module are terminated.
- What happens when two modules have the same ALSA card number? This cannot happen -- the OS assigns unique card numbers to each USB audio device. The system relies on OS-level uniqueness.
- What happens when the maximum number of SIP registrations is exceeded on the SIP server? All modules share a single SIP registration. The SIP server sees one registered endpoint making multiple concurrent outbound calls. If the SIP server limits concurrent calls, excess bridge attempts will fail with a SIP error and the GSM caller hears an error tone.
- What happens when the system receives SIGINT/SIGTERM while multiple calls are active? All active calls on all modules are terminated, all modules are cleaned up, and the system shuts down gracefully.
- What happens when a module that failed initialization later becomes available (e.g., SIM registers on the network)? The system periodically retries failed modules in the background and promotes them to the active pool once initialization succeeds.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: System MUST scan the USB bus at startup and detect all connected EC20 modules (USB vendor/product ID 2c7c:0125), identifying each module's serial port and ALSA audio device.
- **FR-002**: System MUST assign a stable, unique identifier to each detected module derived from its hardware serial number. The same physical module MUST receive the same identifier across reboots regardless of USB enumeration order.
- **FR-003**: System MUST initialize each detected module independently (serial port open, AT command setup, network registration check). Failure of one module MUST NOT prevent other modules from operating.
- **FR-004**: System MUST require at least one functional module to start. If no modules are detected or all fail initialization, the system MUST exit with a nonzero status code and a descriptive error message.
- **FR-005**: System MUST listen for incoming GSM calls on all active modules simultaneously.
- **FR-006**: System MUST handle incoming calls on each module independently, allowing concurrent bridged calls across different modules.
- **FR-007**: System MUST maintain isolated audio paths for each module -- audio from one module's call MUST NOT leak into another module's call.
- **FR-008**: System MUST share the same SIP server configuration and SIP registration across all modules.
- **FR-009**: System MUST use the same SIP destination routing logic (configured destination or caller-based routing) for calls from all modules.
- **FR-010**: System MUST include the module identifier in all log messages related to a specific module's operations.
- **FR-011**: System MUST output a startup summary listing all detected modules, their serial ports, ALSA devices, and initialization status.
- **FR-012**: System MUST shut down gracefully on SIGINT/SIGTERM, terminating active calls on all modules and releasing all resources.
- **FR-013**: System MUST continue operating if a module becomes unavailable during runtime (e.g., USB disconnect), logging the event and continuing with remaining modules.
- **FR-014**: System MUST periodically retry initialization of modules that failed during startup due to transient conditions (e.g., no network registration). When a previously failed module becomes functional, it MUST be added to the active pool and begin accepting calls.

### Key Entities

- **CardInstance**: An individual EC20 GSM module with its own serial port, ALSA audio device, and call state. Each instance operates independently and can handle one voice call at a time. Identified by a stable unique identifier derived from the module's hardware serial number, consistent across reboots.
- **CardPool**: The collection of all detected and initialized CardInstance entities. Manages the lifecycle of all modules and provides the startup summary.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: The system detects all connected EC20 modules within 10 seconds of launch.
- **SC-002**: The system successfully initializes and operates with at least 2 concurrent EC20 modules.
- **SC-003**: Two simultaneous GSM calls on different modules are both bridged to SIP with independent audio and no cross-talk.
- **SC-004**: A module failure during initialization does not prevent the remaining modules from accepting calls, provided at least one module is functional.
- **SC-005**: All log messages during call handling identify which module the event belongs to.
- **SC-006**: The system handles 20 sequential calls spread across multiple modules without resource leaks or degraded performance.
- **SC-007**: Graceful shutdown terminates all active calls across all modules within 5 seconds.

## Assumptions

- All EC20 modules share the same USB vendor/product ID (2c7c:0125). The system distinguishes them by their hardware serial numbers, which are unique per physical module and stable across reboots.
- Each EC20 module exposes its own independent serial ports and ALSA audio device. The OS kernel handles USB device enumeration and assigns unique device paths and ALSA card numbers.
- All modules share a single SIP server account and registration. The SIP server supports multiple concurrent outbound calls from the same registered endpoint.
- Hot-plugging of EC20 modules after boot is out of scope for this feature. Modules must be connected before the system starts. Runtime disconnection is handled gracefully, but newly plugged modules are not detected without a restart.
- The existing config.ini structure is sufficient. No per-module configuration is needed -- all modules use the same SIP destination and bridge settings.
- The host has sufficient USB bandwidth and CPU to handle concurrent audio streams from multiple EC20 modules. Each module uses approximately 128 kbps of USB bandwidth for audio (8 kHz, 16-bit, mono, bidirectional).
- The existing single-module behavior (feature 003) is preserved when only one EC20 module is connected.
- CLI overrides for serial port and audio device (--serial, --audio) apply only to single-module operation. When multiple modules are detected, auto-discovery is used for all of them.
