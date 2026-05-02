# Research: Multi EC20 Card Support

## R1: USB Serial Number Availability via sysfs

**Decision**: Read the USB serial number from `/sys/bus/usb/devices/<dev>/serial` sysfs attribute to derive stable card identifiers.

**Rationale**: The EC20 module's USB serial number is exposed by the kernel in sysfs and is unique per physical module. This avoids sending AT commands (AT+GSN for IMEI) before the serial port is fully initialized, keeping discovery fast and independent of modem state. The card identifier will use a truncated form of the serial number for human readability (e.g., `ec20-<last6chars>`).

**Alternatives considered**:
- AT+GSN (IMEI query): Requires opening and configuring the serial port during discovery, making the process slower and dependent on modem readiness. Rejected.
- USB device path (e.g., `1-2.3`): Changes when the device is plugged into a different port. Not stable across port reassignment. Rejected.
- Sequential index: Simple but not stable across reboots if USB enumeration order changes. Rejected per clarification.

## R2: PJSIP Concurrent Outbound Calls on One Account

**Decision**: A single `pj::Account` supports multiple concurrent outbound `pj::Call` instances. Each CardInstance creates its own `BridgeCall` via the shared account.

**Rationale**: PJSIP's architecture allows unlimited concurrent calls per account (limited only by the endpoint's max call setting, which defaults to 32). The current `BridgeAccount` stores a single `active_call_` pointer; this must be changed to a thread-safe collection (mutex-guarded map keyed by call ID) to support concurrent calls from multiple card threads.

**Alternatives considered**:
- One BridgeAccount per card: Causes multiple SIP registrations to the same server with the same credentials, which most SIP servers reject or handle poorly. Rejected.
- External call manager class: Adds an unnecessary abstraction layer. The account itself is the natural owner of its calls. Rejected per Principle V.

## R3: Per-Card Threading Model

**Decision**: Each CardInstance runs its own dedicated thread that polls AT commands and handles the full bridged call lifecycle (answer, dial SIP, bridge audio, teardown). The existing `handle_bridged_call()` blocking pattern is reused per-card.

**Rationale**: The current bridge main loop is inherently blocking (poll URC → answer → handle call → return to poll). Running each card in its own thread preserves this simple sequential flow per card while enabling parallelism across cards. This avoids refactoring the entire call handling into an async state machine.

**Alternatives considered**:
- Single-threaded async/event-driven: Would require rewriting all blocking I/O (serial reads, ALSA reads) into non-blocking form with an event loop. Major refactor with high risk. Rejected per Principle V.
- Thread pool with work queue: Over-engineered for 2-8 cards. A dedicated thread per card is simpler and maps 1:1 to the hardware. Rejected.

## R4: Failed Module Retry Strategy

**Decision**: A single background retry thread in CardPool attempts to reinitialize failed modules every 30 seconds. On success, the module is added to the active pool and its call-handling thread is started.

**Rationale**: 30 seconds balances responsiveness (a SIM registering on the network typically takes 10-60 seconds) against unnecessary CPU/serial port churn. The retry thread is separate from card threads to avoid blocking active call handling.

**Alternatives considered**:
- No retry (restart required): Poor operational experience for unattended deployments. Rejected per clarification.
- Per-card retry thread: Creates N idle threads for N failed cards. A single retry thread iterating over the failed list is simpler. Rejected per Principle V.
- Exponential backoff: Adds complexity with minimal benefit for a small number of retries against hardware state. Fixed interval is sufficient. Rejected.
