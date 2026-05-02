# Data Model: SIP Audio Echo Server

**Feature**: 002-sip-audio-echo
**Date**: 2026-05-02

## Entities

### SipConfig

Represents the SIP connection parameters read from the configuration file.

| Field | Type | Required | Validation | Default |
|-------|------|----------|------------|---------|
| server | string | yes | Non-empty, valid hostname or IP | - |
| port | integer | no | 1-65535 | 5060 |
| username | string | yes | Non-empty | - |
| password | string | yes | Non-empty | - |
| display_name | string | no | Max 128 characters | username |
| transport | string | no | One of: udp, tcp, tls | udp |

**Source**: `[sip]` section of the INI configuration file.

### CallSession

Represents an active voice call managed by the echo server.

| Field | Type | Description |
|-------|------|-------------|
| call_id | integer | PJSUA internal call identifier |
| remote_uri | string | SIP URI of the calling party |
| state | CallState | Current call lifecycle state |
| start_time | timestamp | When the call was answered |

**Constraint**: At most one active CallSession exists at any time. Concurrent calls are rejected with SIP 486 Busy Here.

### CallState (Enumeration)

| Value | Description | Transitions To |
|-------|-------------|----------------|
| INCOMING | INVITE received, not yet answered | ACTIVE, ENDED |
| ACTIVE | Call answered, audio echo running | ENDED |
| ENDED | Call terminated (BYE or error) | - |

## State Machine: Server Lifecycle

```text
STARTING ──(config loaded)──> REGISTERING
REGISTERING ──(200 OK)──> REGISTERED
REGISTERING ──(timeout/error)──> RETRY_WAIT ──(timer)──> REGISTERING
REGISTERED ──(INVITE)──> IN_CALL
REGISTERED ──(reg expired)──> REGISTERING
IN_CALL ──(BYE/error)──> REGISTERED
REGISTERED ──(SIGINT/SIGTERM)──> SHUTTING_DOWN
IN_CALL ──(SIGINT/SIGTERM)──> SHUTTING_DOWN
SHUTTING_DOWN ──(unregistered)──> STOPPED
```

## Configuration File Format

```ini
[sip]
server = pbx.example.com
port = 5060
username = echo-test
password = secret123
display_name = Echo Server
transport = udp
```
