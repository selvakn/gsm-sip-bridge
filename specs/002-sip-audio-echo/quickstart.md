# Quick Start: SIP Audio Echo Server

## Prerequisites

- Linux (Debian/Ubuntu recommended)
- GCC 9+ with C++17 support
- CMake 3.14+
- PJSIP development headers

Install build dependencies:

```bash
sudo apt install build-essential cmake g++ libpjproject-dev
```

## Build

```bash
make build
```

## Configure

Create a `config.ini` file in the project root:

```ini
[sip]
server = your-sip-server.com
port = 5060
username = echo-test
password = your-password
transport = udp
```

## Run

```bash
make run-sip
```

Or directly:

```bash
./build/sip-echo --config config.ini --verbose
```

## Verify

1. Start the echo server and confirm `SIP registration successful` appears in the logs.
2. From a SIP client (e.g., Ooma, Zoiper, Linphone), dial the registered extension.
3. Speak into the phone -- you should hear your own voice echoed back.
4. Hang up. The server logs `call ended` and returns to idle.
5. Press Ctrl+C to stop. The server de-registers and exits cleanly.

## Troubleshooting

**Registration fails**: Verify server address, port, and credentials. Check that the SIP server is reachable (`ping` or `nc -zvu server 5060`).

**No audio echo**: Confirm the SIP server is not acting as a media proxy that strips direct RTP. Check firewall rules for UDP ports 10000-20000 (RTP range).

**Build fails with missing pjsua2.hpp**: Install `libpjproject-dev` package. On non-Debian systems, build PJSIP from source.
