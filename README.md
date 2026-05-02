# Audio Echo

Auto-answer incoming GSM voice calls on a Quectel EC20 module and echo the caller's audio back in real time.

**Version**: 0.1.0 | **Language**: C++17 | **Platform**: Linux

## Prerequisites

- Linux with ALSA support (kernel 3.11+, Debian/Ubuntu recommended)
- GCC 9+ with C++17 support
- CMake 3.14+
- ALSA development headers (`libasound2-dev`)
- Quectel EC20 module connected via USB with an active SIM card
- EC20 firmware EC20CEFAGR06A15M4G or later (USB Audio Class support)

Install build dependencies:

```bash
sudo apt install build-essential cmake g++ libasound2-dev
```

## Quick Start

```bash
git clone <repo-url> audio-echo && cd audio-echo
make build
make test
make run
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

```bash
# Auto-detect EC20 module
audio-echo

# Override serial port
audio-echo --serial /dev/ttyUSB3

# Override both with verbose AT logging
audio-echo -s /dev/ttyUSB2 -a hw:2,0 -v

# Show help
audio-echo --help
```

## Makefile Targets

| Target       | Description                          |
|-------------|--------------------------------------|
| `make build` | Compile the audio-echo binary        |
| `make test`  | Run the full integration test suite  |
| `make run`   | Build and run with auto-detection    |
| `make clean` | Remove all build artifacts           |
| `make lint`  | Run static analysis                  |
| `make help`  | Show all available targets           |

## Architecture

```text
src/
├── main.cpp              # CLI, signal handling, event loop
├── device_discovery.*    # USB sysfs auto-detection (VID:PID 2c7c:0125)
├── serial_port.*         # POSIX termios RAII wrapper
├── at_commander.*        # AT command send/receive, URC parsing
├── audio_loop.*          # ALSA capture->playback loopback
└── logger.h              # Timestamped stdout logging

tests/integration/
├── pty_pair.h            # PTY pair helper for serial tests
├── test_device_discovery.cpp
├── test_serial_port.cpp
├── test_at_commander.cpp
├── test_audio_loop.cpp
└── test_end_to_end.cpp
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
