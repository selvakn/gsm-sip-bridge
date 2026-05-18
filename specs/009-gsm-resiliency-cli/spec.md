# Feature Specification: GSM Modem Resiliency & CLI Utilities

**Feature Branch**: `009-gsm-resiliency-cli`  
**Created**: 2026-05-17  
**Status**: Draft  
**Input**: User description: "lets add some resiliency features. When the network disconnects or the card is unplugged, the system should detect it and reload the cards. Also while starting, print the current phonenumber (found using AT commands) and print if its connected via 2g or 3g or 4g. Add additional cli commands for utilities like restarting the card, switching from 2g to 4g (and viceversa)"

## Clarifications

### Session 2026-05-17

- Q: How does the CLI (`card restart`, `card set-mode`, `card get-mode`) communicate with the running bridge daemon? → A: Unix domain socket with a simple request/response protocol. The daemon listens on a well-known socket path; the CLI binary connects, sends a command frame, and waits for the response.
- Q: Does the IMEI→slot mapping persist across process restarts, or is it assigned dynamically at each startup? → A: Persistent, stored in the existing database. A card seen for the first time is assigned the next available slot number and that mapping is saved; on subsequent starts or re-plugs the same IMEI always gets the same slot number.
- Q: Should the operator's network mode preference (`set-mode`) persist across full process restarts? → A: Yes, stored in the database per slot and re-applied on every card initialization (cold start and recovery).
- Q: Where are recovery retry count and network-loss timeout configured? → A: In the existing TOML config file, under a `[resilience]` section.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Automatic Card Recovery on Disconnect (Priority: P1)

A system operator has the GSM-SIP bridge running on a server. The EC20 modem module unexpectedly loses its USB connection (card physically unplugged or USB bus reset) or drops off the mobile network. Without any manual intervention, the system detects the failure, cleans up the affected module's state, and restores full calling capability by reinitializing the card — all within a short, bounded time window.

**Why this priority**: The primary value of this feature is unattended operation. A bridge that silently goes dead without recovering defeats the purpose of a homelab telephony server. Operators cannot be present 24/7 to manually restart failed cards.

**Independent Test**: Simulate a USB unplug event on a running card and verify the system logs the failure, tears down the old state, and successfully re-registers the card within the recovery window — without restarting the whole process.

**Acceptance Scenarios**:

1. **Given** a module is active and bridging calls, **When** the USB device is physically unplugged, **Then** the system detects the removal within 5 seconds, logs a warning with the slot and reason, tears down the module cleanly, and begins a recovery attempt.
2. **Given** a module has lost mobile network registration (e.g., signal dropout), **When** network loss persists beyond the detection threshold, **Then** the system logs the event and initiates a modem re-registration attempt without restarting the whole bridge process.
3. **Given** a recovery attempt is in progress, **When** the card is re-plugged or signal returns, **Then** the system completes re-initialization, re-registers with the SIP proxy, and resumes normal operation — logging success with timestamps.
4. **Given** a card repeatedly fails recovery (e.g., hardware fault), **When** the maximum retry count is reached, **Then** the system logs a critical error for that slot and stops retrying, leaving all other modules unaffected.
5. **Given** one module is in recovery, **When** another module is actively bridging a call, **Then** the active call on the other module is not interrupted.

---

### User Story 2 - Startup Diagnostics Display (Priority: P2)

When the bridge process starts up, an operator sees a clear summary for each detected modem module: its assigned phone number and its current network connection type (2G/3G/4G/No Signal). This gives immediate confidence that cards are provisioned correctly and connected to the expected network tier before any calls are attempted.

**Why this priority**: Operators frequently need to verify that a card came up on the expected network (e.g., 4G for VoLTE) after a reboot. Without this, diagnosing "why is audio quality poor" or "is the card even registered?" requires manual AT command sessions.

**Independent Test**: Start the bridge with one or more cards connected and verify that the startup log/console output includes each card's phone number and network type before the system prints its ready message.

**Acceptance Scenarios**:

1. **Given** the bridge starts with two modem modules connected, **When** initialization completes, **Then** the console/log output shows each module's phone number (e.g., `+91XXXXXXXXXX`) and network type (e.g., `4G/LTE`, `3G/UMTS`, `2G/EDGE`, or `No Signal`) before printing the ready message.
2. **Given** a module has no SIM card inserted, **When** initialization runs, **Then** the output shows `No SIM` for that module's phone number field and `No Signal` for network type.
3. **Given** a module is connected but has no network registration (SIM present, no signal), **When** initialization runs, **Then** the output shows the phone number (if readable from SIM) and `No Signal` for network type.

---

### User Story 3 - CLI Card Restart Command (Priority: P3)

An operator can restart a specific modem module on demand via a CLI subcommand without restarting the entire bridge process. This is useful when a card is behaving abnormally (e.g., AT command timeouts, stuck in wrong network mode) and needs a soft reset.

**Why this priority**: Targeted card restart is a common maintenance action in production. It is less disruptive than a full process restart and gives operators a precise recovery tool.

**Independent Test**: With the bridge running and a card active, run the restart subcommand targeting that card's slot/index. Verify the card is torn down, re-initialized, and returns to normal operation without affecting other cards.

**Acceptance Scenarios**:

1. **Given** the bridge is running and a module is in a ready state, **When** the operator runs `gsm-sip-bridge card restart --slot <N>`, **Then** the module is torn down, re-initialized, and returns to ready within 30 seconds, with success reported to the operator.
2. **Given** the operator provides an invalid slot number, **When** running the restart command, **Then** the CLI prints a clear error message listing valid slot numbers and exits with a non-zero status code.
3. **Given** the target module has an active call, **When** the restart command is issued, **Then** the CLI warns the operator that the active call will be dropped and completes the restart.

---

### User Story 4 - CLI Network Mode Switch Command (Priority: P3)

An operator can switch a specific modem module between network preference modes (2G-only, 3G-only, 4G-preferred, auto) via a CLI subcommand. This enables deliberate network-tier control — for example, forcing 4G for VoLTE or forcing 2G to reduce power draw or avoid a degraded LTE cell.

**Why this priority**: Network mode switching is a low-frequency but high-value diagnostic and configuration action. It lets operators experiment with and lock network tiers without manual modem command sessions.

**Independent Test**: With a card connected, run the network-mode switch command to change from auto to 4G-preferred. Query the card's current network mode and verify it reflects the new setting. Repeat for 2G-only mode.

**Acceptance Scenarios**:

1. **Given** a module is in auto network mode, **When** the operator runs `gsm-sip-bridge card set-mode --slot <N> --mode 4g`, **Then** the module is configured to prefer 4G/LTE, reconnects to the network in the new mode, and the result is logged and reported to the operator.
2. **Given** a module is in 4G mode, **When** the operator runs `gsm-sip-bridge card set-mode --slot <N> --mode 2g`, **Then** the module switches to 2G-only mode.
3. **Given** the operator runs `gsm-sip-bridge card set-mode --slot <N> --mode auto`, **Then** the module returns to automatic network selection.
4. **Given** the operator provides an unsupported mode string, **When** running the command, **Then** the CLI prints a clear error listing supported modes (`2g`, `3g`, `4g`, `auto`) and exits with a non-zero status code.
5. **Given** the operator runs `gsm-sip-bridge card get-mode --slot <N>`, **Then** the CLI prints the current network mode preference for that slot.

---

### Edge Cases

- What happens when the USB bus resets and the device re-enumerates at a different device path? The system must identify the card by its hardware identity (IMEI), not by device path, to correctly restore its slot assignment.
- What happens when a card is unplugged during an active call? The active call must be cleanly torn down on the SIP side before recovery is attempted.
- What happens when all cards disconnect simultaneously? Each card's recovery runs independently; the system attempts to recover all of them without one blocking another.
- What happens when the phone number cannot be read (no SIM, PIN-locked SIM)? The startup display shows `Unknown` for that field and logs a warning; the bridge continues starting up.
- What happens when a network mode switch is requested on a card that is in recovery? The command must fail with a clear error indicating the card is not in a ready state.
- What happens when the bridge runs as a daemon with no attached terminal? Startup diagnostics appear in the structured log output, not only on stdout.
- What happens if a card gives up after max retries but is then manually restarted via `card restart`? The manual restart should reset the give-up state and allow the card to re-enter operation.

## Requirements *(mandatory)*

### Functional Requirements

**Resiliency / Auto-Recovery**

- **FR-001**: The system MUST detect when a modem module's USB connection is lost within 5 seconds of the disconnect event.
- **FR-002**: The system MUST detect when a modem module loses mobile network registration and fails to re-register within a timeout configurable in the TOML config file under `[resilience]` (default: 60 seconds).
- **FR-003**: Upon detecting a module failure, the system MUST cleanly tear down that module's state (active calls, modem command session, audio paths) before attempting recovery.
- **FR-004**: The system MUST automatically attempt to re-initialize a failed module when the USB device reappears or network registration is restored.
- **FR-005**: Recovery attempts MUST use an exponential back-off delay with initial delay and cap configurable in the TOML config file under `[resilience]` (defaults: start 5 seconds, cap 120 seconds).
- **FR-006**: The system MUST cease retrying a module after a maximum retry count configurable in the TOML config file under `[resilience]` (default: 10 attempts) and produce a critical log entry for that slot.
- **FR-006a**: The system MUST persist the IMEI→slot mapping in the existing database. A newly detected card is assigned the next available slot number and that assignment is stored permanently. On all subsequent starts and re-plug events, the same IMEI always receives the same slot number.
- **FR-007**: A failure or recovery of one module MUST NOT affect the operation of any other module.
- **FR-008**: All recovery lifecycle events (detected failure, teardown, retry attempt, recovery success, give-up) MUST be logged with module slot, timestamp, and cause.

**Startup Diagnostics**

- **FR-009**: On startup, the system MUST query each detected module for its phone number and display it in the startup output before declaring readiness.
- **FR-010**: On startup, the system MUST query each detected module for its current network registration type and display one of: `4G/LTE`, `3G/UMTS`, `2G/EDGE`, `No Signal`, or `No SIM`.
- **FR-011**: Startup diagnostics MUST complete and be visible in output before the system declares itself ready to accept calls.
- **FR-012**: If a phone number or network type cannot be determined, the system MUST display `Unknown` for that field and continue startup without aborting.

**CLI-to-Daemon Communication**

- **FR-012a**: The running bridge daemon MUST expose a Unix domain socket at a well-known path (e.g., `/run/gsm-sip-bridge/control.sock`) to accept management commands from the CLI.
- **FR-012b**: The CLI binary MUST connect to the daemon's Unix socket, send a command frame, and wait for a response before printing output and exiting.
- **FR-012c**: If the daemon is not running or the socket is unreachable, the CLI MUST exit with a non-zero status code and a human-readable error message.

**CLI Card Restart**

- **FR-013**: The CLI MUST provide a `card restart` subcommand that accepts a slot identifier (`--slot <N>`) and triggers a controlled restart of that module.
- **FR-014**: The `card restart` command MUST wait for teardown and re-initialization to complete (or timeout) and report success or failure to the operator before exiting.
- **FR-015**: If the target slot has an active call, the restart command MUST warn the operator that the call will be dropped and complete the restart.
- **FR-016**: The `card restart` command MUST reject invalid or out-of-range slot identifiers with a descriptive error and exit with a non-zero status code.
- **FR-017**: A successful `card restart` on a slot that had previously given up MUST reset the give-up state, allowing the card to re-enter normal operation.

**CLI Network Mode Switch**

- **FR-018**: The CLI MUST provide a `card set-mode` subcommand accepting `--slot <N>` and `--mode <mode>`, where `<mode>` is one of: `2g`, `3g`, `4g`, `auto`.
- **FR-019**: The `card set-mode` command MUST apply the requested network mode to the modem, persist the preference in the database, and confirm via follow-up query that the mode was accepted, reporting the result to the operator.
- **FR-019a**: On every card initialization (cold start and auto-recovery), the system MUST read the stored network mode preference for that slot from the database and apply it to the modem before declaring the card ready.
- **FR-020**: The CLI MUST provide a `card get-mode` subcommand that queries and displays the current network mode preference for a given slot.
- **FR-021**: All `card` subcommands MUST display a list of valid slots when an invalid slot is provided.
- **FR-022**: Network mode commands MUST fail with a descriptive error if the target module is not in a ready state.

### Key Entities

- **Module / Card Slot**: Represents one modem module. Identified by slot index (0-based) and hardware identity (IMEI). The IMEI→slot mapping is persisted in the database so the same physical card always receives the same slot number across process restarts and re-plugs. Has a lifecycle state: `Initializing`, `Ready`, `Recovering`, `GivenUp`.
- **Network Registration**: The module's current mobile network attachment. Attributes: type (`4G/LTE`, `3G/UMTS`, `2G/EDGE`, `No Signal`, `No SIM`), detected at startup and updated on recovery.
- **Recovery Event**: A logged record of a failure/recovery cycle for a slot. Attributes: slot index, timestamp, trigger (USB loss / network loss), attempt count, outcome (success / give-up).
- **Network Mode Preference**: The operator-configured preferred network tier for a slot: `2g`, `3g`, `4g`, `auto`. Stored in the database per slot and re-applied to the modem on every card initialization — including cold starts and auto-recovery — so the preference survives daemon restarts.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A modem that is unplugged and re-plugged resumes full operation (SIP-registered, able to bridge calls) within 30 seconds of the USB device reappearing, with no manual intervention.
- **SC-002**: A modem that drops network registration recovers and re-registers within 90 seconds of signal returning, with no manual intervention.
- **SC-003**: Startup diagnostics (phone number and network type for all connected cards) are visible in output within 10 seconds of process start.
- **SC-004**: 100% of recovery lifecycle events produce a structured log entry with slot, timestamp, and cause — zero silent failures.
- **SC-005**: A `card restart` command completes (success or reported failure) within 30 seconds for a card in normal operating state.
- **SC-006**: A `card set-mode` command produces a confirmed network mode change within 15 seconds under normal signal conditions.
- **SC-007**: Zero degradation in concurrent bridged calls on other cards when one card is in recovery.
- **SC-008**: All `card` subcommands return a non-zero exit code and a human-readable error message for every invalid input combination.

## Assumptions

- The bridge is already running in a Rust-based architecture (per the 008-rust-rewrite spec) with per-module slot management and an existing TOML config file; this feature adds a `[resilience]` section to that config and resiliency/CLI tooling on top of the existing foundation.
- EC20 modules communicate over serial modem commands; network registration status and phone number are queryable via standard 3GPP commands available on the EC20.
- USB hotplug events are detectable at the OS level and can be monitored by the bridge process without elevated privileges (standard Linux kernel interfaces; udev rules may be pre-configured).
- Hardware identity (IMEI) is stable across re-enumeration and can be used to match a re-plugged device to its previous slot assignment.
- Operators interact with the running bridge exclusively via the CLI binary for the commands added in this feature; no web UI or separate management API is needed.
- Network mode preferences are stored in the bridge's own database (not relying on the modem's non-volatile storage) and re-applied by the bridge on every card initialization.
- At most 8 modem modules are supported per host (inherited constraint from the multi-card and rust-rewrite specs).
- SIM PIN unlocking is out of scope; cards requiring a PIN unlock will show `Unknown` or `No SIM` in startup diagnostics.
- Startup diagnostic queries are expected to add at most a few seconds to startup time, which is acceptable for the homelab/SOHO deployment target.
