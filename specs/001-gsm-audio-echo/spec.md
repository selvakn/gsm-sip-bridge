# Feature Specification: GSM Audio Echo

**Feature Branch**: `001-gsm-audio-echo`
**Created**: 2026-05-02
**Status**: Draft
**Input**: User description: "A GSM module (Quectel EC20) connected over USB with a SIM module. When an incoming call is received, attend the call and echo the incoming audio back to the caller. Audio devices from the module are available as soundcard at the OS level. Use maximum audio quality parameters."

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Auto-Answer and Echo Audio (Priority: P1)

A Quectel EC20 GSM module is connected to a Linux host via USB with
an active SIM card. When a remote party dials the SIM number, the
system detects the incoming call, automatically answers it, and
immediately begins echoing the caller's audio back to them in
real time. The caller hears their own voice played back with minimal
delay. When the remote party hangs up, the system returns to an idle
state ready for the next call.

**Why this priority**: This is the entire core value proposition.
Without auto-answer and audio echo, no other functionality matters.

**Independent Test**: Place a phone call to the SIM number from any
phone. Verify the call is answered automatically, speak into the
phone, and confirm you hear your own voice echoed back. Hang up and
verify the system returns to idle.

**Acceptance Scenarios**:

1. **Given** the system is running and idle with the EC20 module
   connected, **When** an incoming call arrives, **Then** the system
   answers the call within 3 seconds of the first ring.
2. **Given** an active call is in progress, **When** the caller
   speaks, **Then** the caller hears their own audio echoed back
   with less than 500 milliseconds of round-trip delay.
3. **Given** an active call is in progress, **When** the remote party
   hangs up, **Then** the system detects the hangup and returns to
   idle within 2 seconds.
4. **Given** an active call is in progress, **When** there is silence
   from the caller, **Then** the system does not produce noise or
   artifacts on the return audio.

---

### User Story 2 - Optimal Audio Quality (Priority: P2)

The system uses the highest audio quality parameters supported by
the GSM module and the active network connection. For standard GSM
networks, this means narrowband audio (8 kHz sample rate, 16-bit
depth, mono). For networks supporting HD Voice (AMR-WB / VoLTE),
the system uses wideband audio (16 kHz sample rate, 16-bit depth,
mono). The audio path introduces no additional distortion, clipping,
or sample rate conversion artifacts beyond what the network codec
inherently produces.

**Why this priority**: Audio quality directly affects whether the
echo is usable and perceptible. Poor quality defeats the purpose,
but the echo still works at any quality level, making this P2.

**Independent Test**: Place a call and speak a variety of tones
(low hum, normal speech, high whistle). Verify the echoed audio
is clear, free of clicks/pops/distortion, and faithfully reproduces
the original signal within the limits of the network codec.

**Acceptance Scenarios**:

1. **Given** the system is running on a standard GSM network,
   **When** a call is active, **Then** audio is captured and played
   back at the native sample rate of the module (8 kHz narrowband
   or 16 kHz wideband) without re-sampling.
2. **Given** the audio path is active, **When** audio samples flow
   through the system, **Then** no clipping, clicks, pops, or
   buffer underrun/overrun artifacts are audible.
3. **Given** the system is echoing audio, **When** measured with a
   known reference tone, **Then** the echoed signal has no
   measurable distortion beyond the network codec's own distortion.

---

### User Story 3 - Continuous Operation and Error Recovery (Priority: P3)

The system runs continuously as a long-lived process. After one call
ends, it waits for the next incoming call without manual
intervention. If the GSM module becomes temporarily unavailable
(e.g., USB disconnect/reconnect, SIM registration loss), the system
detects the failure, logs the event, and recovers automatically
when the module becomes available again. The system handles multiple
sequential calls without resource leaks or degradation.

**Why this priority**: Reliability matters for unattended operation
but is not required for a first demonstration of the echo
capability.

**Independent Test**: Place 10 sequential calls and verify each is
answered and echoed correctly. Disconnect and reconnect the USB
cable during idle state and verify the system recovers. Leave the
system running for 1 hour and verify no resource leaks (memory,
file descriptors, audio device handles).

**Acceptance Scenarios**:

1. **Given** a call has just ended, **When** a new incoming call
   arrives, **Then** the system answers and echoes audio identically
   to the first call.
2. **Given** the system has handled 10 sequential calls, **When**
   system resource usage is measured, **Then** memory and file
   descriptor counts remain stable (no growth trend).
3. **Given** the USB connection to the EC20 is interrupted during
   idle, **When** the USB is reconnected, **Then** the system
   detects the module within 10 seconds and resumes normal
   operation.
4. **Given** the system encounters an unrecoverable error on a
   single call, **When** that call ends or times out, **Then** the
   system logs the error and returns to idle without crashing.

---

### Edge Cases

- What happens when two calls arrive simultaneously (call waiting)?
  The system MUST reject the second call while one is active.
- What happens if the SIM has no network registration? The system
  MUST log the condition and retry until registration succeeds.
- What happens if the audio device is claimed by another process?
  The system MUST report the conflict and exit with a clear error.
- What happens if the caller's audio is pure silence for an extended
  period? The system MUST continue the call without timeout (the
  remote party controls call duration).
- What happens if the EC20 module is not detected at startup? The
  system MUST report the missing device and exit with a clear error
  message indicating which device was expected.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: System MUST detect incoming voice calls on the GSM
  module by monitoring the module's call signaling interface.
- **FR-002**: System MUST automatically answer an incoming call
  within 3 seconds of the first ring indication.
- **FR-003**: System MUST capture audio from the GSM module's
  OS-level audio input device (microphone/capture) in real time.
- **FR-004**: System MUST play the captured audio back to the GSM
  module's OS-level audio output device (speaker/playback) in real
  time, creating an echo effect for the remote caller.
- **FR-005**: System MUST use the native sample rate and bit depth
  of the GSM module's audio device without re-sampling (typically
  8 kHz/16-bit mono for narrowband or 16 kHz/16-bit mono for
  wideband).
- **FR-006**: System MUST detect when the remote party hangs up
  and release the audio devices, returning to an idle monitoring
  state.
- **FR-007**: System MUST reject additional incoming calls while
  a call is already active.
- **FR-008**: System MUST run as a foreground process that can be
  started from the command line and stopped with a standard
  interrupt signal (e.g., Ctrl+C).
- **FR-009**: System MUST log call events (ring, answer, hangup,
  errors) to standard output with timestamps.
- **FR-010**: System MUST exit with a nonzero status code and a
  descriptive error message if the GSM module or its audio devices
  are not detected at startup.

### Key Entities

- **GSM Module**: The Quectel EC20 hardware connected via USB.
  Exposes a call signaling interface (for ring/answer/hangup) and
  audio devices (capture and playback) as OS-level soundcards.
- **Call Session**: Represents an active voice call. Has a lifecycle:
  ringing -> answered -> active (echoing) -> ended. Only one
  session can be active at a time.
- **Audio Stream**: The real-time bidirectional audio data between
  the module and the system. Characterized by sample rate, bit
  depth, channel count, and buffer size.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Incoming calls are answered within 3 seconds of the
  first ring, 100% of the time under normal conditions.
- **SC-002**: The caller perceives their own voice echoed back with
  less than 500 milliseconds of round-trip delay.
- **SC-003**: Echoed audio is free of system-introduced artifacts
  (clicks, pops, dropouts) during a 5-minute continuous call.
- **SC-004**: The system handles 10 sequential calls without manual
  intervention, resource leaks, or degraded performance.
- **SC-005**: The system starts and is ready to receive calls within
  10 seconds of launch.
- **SC-006**: All call lifecycle events (ring, answer, hangup, error)
  are logged with timestamps and are human-readable.

## Assumptions

- The host operating system is Linux with ALSA support. The EC20
  module's USB audio interface is enumerated as a standard ALSA
  soundcard automatically by the kernel.
- The Quectel EC20 module is connected via USB and powered. Its
  serial/AT command interface and audio interface are both available
  at the OS level (typically as /dev/ttyUSBx for AT commands and as
  an ALSA hw:X,Y device for audio).
- A SIM card with an active voice plan is inserted into the module.
  The SIM does not require a PIN at startup (PIN is disabled or
  pre-entered).
- The system handles one call at a time. Concurrent call handling
  (conference, call waiting) is out of scope.
- The echo is a simple audio loopback (capture -> playback) with no
  signal processing, filtering, or transformation applied.
- C++ is a suitable technology choice for this feature given the
  real-time audio processing requirements and direct hardware
  interaction via ALSA and serial interfaces. Alternative
  consideration: C with POSIX APIs would also work but C++ provides
  better resource management (RAII) for audio device handles and
  serial ports.
- Network audio codec selection (AMR-NB vs AMR-WB) is handled
  entirely by the EC20 module and the network. The system operates
  on whatever PCM format the ALSA device provides.
- The system does not need a GUI. It operates as a command-line
  process.
