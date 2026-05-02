# Audio Echo

Echo incoming voice call audio back to the caller. Supports two modes: GSM (Quectel EC20 hardware) and SIP (VoIP via PJSIP).

**Version**: 0.1.0 | **Language**: C++17 | **Platform**: Linux

## Prerequisites

- Linux (Debian/Ubuntu recommended)
- GCC 9+ with C++17 support
- CMake 3.14+
- ALSA development headers (`libasound2-dev`) -- for GSM echo
- PJSIP development libraries (`libpjproject-dev` or built from source) -- for SIP echo
- Quectel EC20 module connected via USB with an active SIM card -- for GSM echo only

Install build dependencies:

```bash
sudo apt install build-essential cmake g++ libasound2-dev libpjproject-dev
```

## Quick Start

```bash
git clone <repo-url> audio-echo && cd audio-echo
make build
make test

# GSM echo (requires EC20 hardware)
make run

# SIP echo (requires SIP server)
cp config.ini.example config.ini   # edit with your SIP credentials
make run-sip
```

## One-Time EC20 Setup

Enable USB Audio Class (UAC) on the EC20 module:

```bash
# Connect to AT command port
minicom -D /dev/ttyUSB2 -b 115200

# Enable UAC (last parameter = 1)
AT+QCFG="USBCFG",0x2C7C,0x0125,1,1,1,1,1,0,1

# Reboot module
AT+CFUN=1,1
```

Verify audio device appears:

```bash
arecord -l    # Should show a card named "Android"
aplay -l      # Same card for playback
```

## Usage

### GSM Echo (audio-echo)

```bash
audio-echo                              # auto-detect EC20 module
audio-echo --serial /dev/ttyUSB3        # override serial port
audio-echo -s /dev/ttyUSB2 -a hw:2,0 -v  # override both, verbose
```

### SIP Echo (sip-echo)

```bash
sip-echo --config config.ini            # use specific config file
sip-echo --config config.ini --verbose  # verbose SIP logging
sip-echo --help                         # show all options
```

## SIP Configuration

Create a `config.ini` file (see `config.ini.example`):

```ini
[sip]
server = pbx.example.com
port = 5060
username = echo-test
password = your-password
transport = udp
```

## Makefile Targets

| Target         | Description                          |
|---------------|--------------------------------------|
| `make build`   | Compile both binaries                |
| `make test`    | Run the full integration test suite  |
| `make run`     | Build and run GSM echo               |
| `make run-sip` | Build and run SIP echo               |
| `make clean`   | Remove all build artifacts           |
| `make lint`    | Run static analysis                  |
| `make help`    | Show all available targets           |

## Architecture

```text
src/
├── logger.h              # Shared timestamped stdout logging
├── main.cpp              # GSM: CLI, signal handling, event loop
├── device_discovery.*    # GSM: USB sysfs auto-detection (VID:PID 2c7c:0125)
├── serial_port.*         # GSM: POSIX termios RAII wrapper
├── at_commander.*        # GSM: AT command send/receive, URC parsing
├── audio_loop.*          # GSM: ALSA capture->playback loopback
└── sip/
    ├── main.cpp          # SIP: CLI, PJSIP endpoint lifecycle
    ├── sip_config.*      # SIP: INI config parser and validation
    ├── echo_account.*    # SIP: pj::Account subclass (registration, incoming calls)
    └── echo_call.*       # SIP: pj::Call subclass (call state, audio loopback)

vendor/
└── mini/ini.h            # mINI header-only INI parser (MIT)

tests/integration/
├── pty_pair.h            # PTY pair helper for serial tests
├── test_device_discovery.cpp
├── test_serial_port.cpp
├── test_at_commander.cpp
├── test_audio_loop.cpp
├── test_end_to_end.cpp
├── test_sip_config.cpp   # SIP config parsing tests
└── test_sip_echo.cpp     # SIP echo lifecycle tests
```

## ModemManager Interference

ModemManager probes `ttyUSB*` ports for modems, which corrupts AT sessions. The program warns at startup if ModemManager is active. To fix permanently, install the included udev rule:

```bash
sudo cp etc/99-ec20-audio-echo.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
```

This tells ModemManager to ignore the EC20 entirely. To stop it immediately:

```bash
sudo systemctl stop ModemManager
sudo systemctl disable ModemManager   # prevent restart on boot
```

## Troubleshooting

**No `/dev/ttyUSB*` devices**: Check `dmesg | grep ttyUSB`. Ensure `option` and `qcserial` kernel modules are loaded.

**No audio device in `arecord -l`**: UAC not enabled. Follow the one-time setup above. Verify firmware version.

**Permission denied**: Add user to `dialout` and `audio` groups:
```bash
sudo usermod -aG dialout,audio $USER
```

**AT commands timing out or garbled responses**: ModemManager is likely probing the port. See the ModemManager section above.

**Audio clicks/dropouts**: Ensure no other process claims the ALSA device (`fuser /dev/snd/*`). Consider real-time scheduling.
