# Quickstart: Multi EC20 Card Support

## Prerequisites

- Linux host with USB ports
- Two or more Quectel EC20 modules connected via USB, each with an active SIM card
- Build dependencies installed: `libasound2-dev`, `libpjproject-dev`, `cmake`, `g++`

## Build

```bash
make build
```

## Configure

Edit `config.ini` with SIP server credentials. No per-card configuration needed -- all modules share the same SIP settings.

```ini
[sip]
server = your-sip-server.example.com
port = 5060
username = your-username
password = your-password

[bridge]
sip_destination = 599
sip_dial_timeout_sec = 30
```

## Run

```bash
make run
```

Expected startup output with two modules:

```
2026-05-02T10:00:00.000 INFO detected 2 EC20 module(s)
2026-05-02T10:00:00.100 INFO [ec20-A1B2C3] serial=/dev/ttyUSB2, audio=hw:1,0 — ACTIVE
2026-05-02T10:00:00.200 INFO [ec20-D4E5F6] serial=/dev/ttyUSB6, audio=hw:2,0 — ACTIVE
2026-05-02T10:00:01.000 INFO SIP registered as user@server:5060
2026-05-02T10:00:01.001 INFO ready, 2 module(s) active, 0 failed
```

## Verify

1. Call the SIM number on module 1 -- verify the call bridges to SIP extension 599
2. While that call is active, call the SIM number on module 2 -- verify it also bridges independently
3. Both calls should have clear audio with no cross-talk

## Test

```bash
make test
```
