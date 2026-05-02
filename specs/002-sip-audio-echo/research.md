# Research: SIP Audio Echo Server

**Feature**: 002-sip-audio-echo
**Date**: 2026-05-02

## 1. SIP Library: PJSIP / PJSUA2

**Decision**: Use PJSIP via the PJSUA2 C++ API (high-level wrapper).

**Rationale**: User explicitly requested PJSIP. PJSUA2 provides a C++ object-oriented API on top of the low-level PJSIP C stack. It handles SIP registration, call management, and the conference bridge for audio routing. The conference bridge is the key mechanism for audio echo -- connecting a call's audio media back to itself routes the remote party's audio directly back to them without needing a local sound device.

**Alternatives considered**:
- **liblinphone**: C/C++ SIP library, GPL licensed. Similar capability but less widely used for server-side SIP applications.
- **oSIP/eXosip**: Lightweight C SIP stack. Lower-level than PJSUA2, would require significant boilerplate for call handling and media.
- **Sofia-SIP**: Nokia's C SIP library (LGPL). Mature but less actively maintained.

**License**: GPL v2+ (dual-licensed with proprietary option). This is a restrictive license. Acceptable for this project because: (a) user explicitly specified PJSIP as a binding constraint, (b) the echo server is an internal test/diagnostic tool, not a redistributed product.

**Build integration**: Install system package `libpjproject-dev` and link via `pkg-config`. Falls back to CMake `find_package(Pj)` if available. PJSIP's own CMake support is experimental but functional on Linux x86_64 as of 2.17.

## 2. Audio Echo Mechanism

**Decision**: Use PJSUA2 conference bridge to connect call audio back to itself.

**Rationale**: PJSUA2's conference bridge manages all audio routing. When a call's media becomes active, the `onCallMediaState` callback fires. At that point, get the call's `AudioMedia` object and call `startTransmit()` back to itself. This creates a loopback: the remote party's voice is captured by PJSIP's RTP stack and immediately transmitted back via the same RTP stream. No local sound device is needed (null audio device).

**Key API pattern**:
- `onCallMediaState`: Get `AudioMedia` from `getAudioMedia(i)`
- Echo: `aud_med.startTransmit(aud_med)` connects the port to itself
- Null sound device: `Endpoint::audDevManager().setNullDev()` avoids needing a physical audio device

**Alternatives considered**:
- **Local sound device loopback**: Route call audio to speaker (port 0) and capture from mic (port 0) back to call. Requires a physical sound device or ALSA loopback, adds latency, unnecessary complexity.
- **Custom media port**: Register a custom `AudioMediaPort` that copies frames from rx to tx. More flexible but overkill for simple echo.

## 3. Configuration File Parsing

**Decision**: Use mINI (header-only, MIT license, v0.9.18).

**Rationale**: Lightweight single-header C++ INI parser. MIT licensed (corporate-friendly). Preserves comments and formatting on write. Zero dependencies. Sufficient for reading a `[sip]` section with 5-7 key-value pairs.

**Alternatives considered**:
- **inicpp (dujingning)**: MIT, header-only, modern C++. Similar capability but less mature (fewer stars/downloads).
- **inih (benhoyt)**: BSD licensed, C library with C++ wrapper. Slightly more established but not header-only for the C++ wrapper.
- **Hand-rolled parser**: Simple enough to do manually for a single section, but using a tested library reduces edge-case bugs.

## 4. SIP Transport Protocol

**Decision**: UDP by default, configurable via `transport` field in config.ini.

**Rationale**: UDP is the default SIP transport in RFC 3261 and supported by all SIP servers. TCP and TLS can be added as optional config values. PJSUA2 supports all three transports natively.

**Alternatives considered**:
- **TCP only**: More reliable for large messages but adds overhead for simple echo use case.
- **TLS only**: Most secure but requires certificate configuration, out of scope for a diagnostic tool.

## 5. Testing Strategy

**Decision**: Integration tests using PJSIP's built-in loopback transport and null audio device. GTest framework (consistent with existing project).

**Rationale**: PJSIP supports a loop transport (`pjsip_loop_start()`) that routes SIP messages internally without network I/O. Combined with null audio device, this allows full SIP call lifecycle testing without a real SIP server. Follows Constitution Principle I (Integration-First Testing) by testing real PJSIP components rather than mocking the SIP stack.

**Alternatives considered**:
- **Mock SIP stack**: Would violate Constitution Principle I and miss real protocol bugs.
- **Docker SIP server (Asterisk)**: Full integration but heavyweight for CI. Better suited for manual validation than automated tests.

## 6. Project Structure

**Decision**: New `sip/` subdirectory under `src/` to separate SIP echo server from the existing GSM echo module.

**Rationale**: The project now has two distinct echo modules (GSM/hardware and SIP/VoIP). Keeping them in separate subdirectories maintains clarity. Shared utilities (logger) remain at `src/` root. Each module has its own binary target and test suite.
