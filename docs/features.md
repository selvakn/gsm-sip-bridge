# Features

The full list. The [README](../README.md) has the short version.

## Calls

- Inbound calls are answered automatically on all three paths. The caller hears
  ringback while the SIP side rings.
- Outbound calling is off by default (`[outbound].enabled`). A PBX INVITE, or an
  INVITE from a phone registered to the bridge, is placed on the first idle
  line. There is no dial plan and no allow-list; the destination is dialed as
  given. See [architecture.md](architecture.md#outbound-calling).
- An inbound call carries the receiving line's own number to the SIP side: as
  `P-Called-Party-ID` in SIP server mode, or as the Request-URI when
  `[bridge].sip_destination` is empty. The caller's number is sent separately in
  `P-Asserted-Identity` and `X-GSM-Caller-ID`.

## Call paths

- **Circuit-switched**: GSM voice over a Quectel EC20's USB serial and USB audio
  interfaces. `[cs].enabled = false` disables the path and its modem probing.
- **VoWiFi**: IKEv2/IPsec ePDG tunnel (strongSwan), IMS-AKA registration, Gm
  IPsec. AMR-WB to G.722, wideband end to end. Discovers every VoWiFi-capable
  SIM and runs one tunnel and registration per line. Off by default.
- **VoLTE**: the bridge runs its own IMS registration over the modem's LTE data
  PDN instead of using the modem's internal voice stack. One line per modem,
  each in its own network namespace. Off by default.

A card reader holds the SIM with no modem present; the SIM reaches the carrier
over the host's own network. See [supported-hardware.md](supported-hardware.md)
and [omnikey-pcsc-vowifi.md](omnikey-pcsc-vowifi.md).

## SIP-side topologies

Either works with any call path:

- **PBX trunk** (default). The bridge registers to an external PBX, whose
  inbound routes pick the destination.
- **SIP server** (`[sip_server].enabled`). Phones REGISTER to the bridge and it
  rings the configured account. No PBX is needed.

A trunk holds a single binding, so only one component owns the SIP
registration: the circuit-switched daemon normally, or the VoWiFi/VoLTE
telephony agent when either of those is enabled. Outbound calls from a
registered phone are redirected (302) to the dial-out port; both kinds are
placed on the first idle line in that process's own pool.

With VoWiFi or VoLTE enabled the bridge runs as several processes: one
carrier-facing agent per line in its own network namespace, one telephony-side
process holding the PBX registration, and a supervisor that builds the
namespaces and veth pairs and tears them down on exit.

## IMS/SIP stack

The bridge implements SIP, SDP, RTP and the 3GPP SMS layers itself instead of
re-bridging audio the modem has already decoded.

- Decodes and re-encodes carrier media: AMR-NB and AMR-WB, RFC 4867 framing,
  both octet-aligned and bandwidth-efficient. Most carriers offer no G.711.
- SDP: direction attributes and per-media sections are answered as offered.
  Offerless INVITEs, locally-confirmable QoS preconditions (RFC 3312) and
  RFC 4028 session timers are supported.
- RTCP per RFC 3550 on the carrier leg. Carrier-reported loss, jitter and
  round-trip are logged per call and exported as metrics.
- DTMF (RFC 4733) is forwarded across transcoding, and re-stamped when the two
  legs negotiate different payload types. Each event's duration is re-counted
  when the legs' `telephone-event` clocks differ (8 kHz and 16 kHz), so a
  keypress keeps its length. On calls the bridge places, its offer to the
  carrier includes `telephone-event` so the carrier's answer has one.
- SMS over IMS per TS 23.040 and TS 24.011, including the TS 24.341 delivery
  report (RP-ACK) that some carriers require before they deliver messages.
- In-dialog requests are matched against the call they name.

## SMS, alerts and storage

- Incoming SMS is read on all three paths, both over the IMS registration and by
  sweeping the modem's own storage. Both routes share one PDU decoder,
  duplicates are suppressed, and multi-part messages are reassembled. The bridge
  does not send SMS.
- SQLite store: calls (caller ID, line, duration, outcome), SMS, card slots and
  per-slot network mode. sqlite-web is included for read-only browsing.
- Discord webhooks: SMS as embeds, plus alerts for module/SIM lifecycle,
  registration loss, tunnel failure, missed calls, line discovery failure and Gm
  connection loss. Each has its own enable flag, threshold and webhook override,
  and sends a recovery notice when the condition clears.
- Prometheus metrics endpoint and a Grafana dashboard. See
  [observability.md](observability.md).
- A read-only web UI for line health, calls, SMS, events and the loaded config.
  Off by default. See [configuration.md](configuration.md#webui).

## Operation

- Recovery: USB disconnects and registration loss are handled per card with
  exponential backoff. Modem commands have deadlines, and a watchdog restarts a
  line that stops making progress. A nightly restart cycle runs by default. A
  serial port that hangs the kernel driver is quarantined after three timeouts.
- Control CLI over a Unix socket: `card list`, `card restart`, `card set-mode`,
  `card get-mode`. The per-slot mode (`2g`/`3g`/`4g`/`auto`) persists across
  restarts. `vowifi-status` and `volte-status` report line health.
- Audio tuning: `lan`/`wan` latency profiles, ALSA and jitter buffer sizes,
  modem gain, echo canceller, SCHED_FIFO thread priority.
- Configuration is a single TOML file with strict parsing: an unknown key aborts
  startup and names the offending key. Secrets support `env:VAR_NAME`.
- No `unsafe` in the application binary. FFI is confined to the
  `pjsua-sys`/`pjsua-safe` and `amr-sys`/`amr-safe` crates.
