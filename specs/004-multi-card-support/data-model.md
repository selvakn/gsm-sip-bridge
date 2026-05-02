# Data Model: Multi EC20 Card Support

## Entities

### DeviceInfo (modified)

Represents a discovered EC20 USB device before initialization.

| Field | Type | Description |
|-------|------|-------------|
| serial_port | string | OS path to the AT command serial port (e.g., `/dev/ttyUSB2`) |
| alsa_device | string | ALSA hardware device identifier (e.g., `hw:1,0`) |
| serial_number | string | USB serial number from sysfs, unique per physical module |
| usb_path | string | sysfs USB device path (e.g., `1-2.3`), used during discovery only |

**Identity**: `serial_number` (stable across reboots)

### CardInstance

Represents a single EC20 module throughout its runtime lifecycle.

| Field | Type | Description |
|-------|------|-------------|
| card_id | string | Human-readable stable identifier derived from serial_number (e.g., `ec20-A1B2C3`) |
| device | DeviceInfo | The discovered device information |
| state | CardState | Current lifecycle state |
| serial | SerialPort | Owned serial port connection |
| at | AtCommander | Owned AT command interface, uses serial |
| thread | thread | Dedicated call-handling thread |

**Identity**: `card_id` (derived from `device.serial_number`, stable across reboots)

**State transitions**:

```
DISCOVERED → INITIALIZING → ACTIVE → STOPPING → STOPPED
                ↓                       ↑
              FAILED ──(retry)──→ INITIALIZING
                                    ↓
                                  ACTIVE
```

| State | Description |
|-------|-------------|
| DISCOVERED | USB device found, not yet initialized |
| INITIALIZING | Serial port opening, AT setup, network check in progress |
| ACTIVE | Module ready and listening for calls (thread running) |
| FAILED | Initialization failed, eligible for background retry |
| STOPPING | Shutdown requested, terminating active call and thread |
| STOPPED | Thread joined, resources released |

### CardPool

Manages the collection of all CardInstance entities.

| Field | Type | Description |
|-------|------|-------------|
| active_cards | vector\<CardInstance\> | Successfully initialized modules with running threads |
| failed_cards | vector\<CardInstance\> | Modules that failed initialization, eligible for retry |
| retry_thread | thread | Background thread that periodically retries failed modules |
| retry_interval_sec | unsigned int | Interval between retry attempts (default: 30) |

**Invariant**: `active_cards` is non-empty while the system is running (at least one module must succeed initialization at startup).

### BridgeAccount (modified)

Single SIP account shared across all CardInstances.

| Field | Type | Description |
|-------|------|-------------|
| calls | map\<int, BridgeCall\> | Active outbound calls keyed by PJSIP call ID, mutex-protected |
| registered | atomic\<bool\> | SIP registration state |

**Concurrency**: All call map mutations guarded by a mutex. Multiple CardInstance threads call `make_outbound_call()` concurrently.

## Relationships

```
CardPool 1──* CardInstance
CardInstance 1──1 DeviceInfo
CardInstance 1──1 SerialPort
CardInstance 1──1 AtCommander
CardInstance *──1 BridgeAccount (shared reference)
BridgeAccount 1──* BridgeCall
```
