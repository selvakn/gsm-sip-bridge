# Phase 1 Data Model: Slim default image with optional SWu engine

This feature has no persistent/business data model. The "entities" are
build-and-runtime configuration artifacts and one small in-process value type.

## Entity: Image variant

| Field | Values | Notes |
|---|---|---|
| `name` | slim \| full | Conceptual; not stored. |
| build arg `INCLUDE_SWU` | `false` (default) \| `true` | Selects the SWu payload stage and gates python3/net-tools. |
| image name | `<repo>` (shared) | Both variants share one image name. |
| tag(s) | slim: `{{version}}`, `{{major}}.{{minor}}`, `latest`, sha · full: `{{version}}-swu` | Floating/canonical tags → slim. |
| payload marker | `/etc/gsm-sip-bridge/swu-available` present only in full | Runtime variant signal (R2). |
| SWu dialer | `/opt/SWu-IKEv2/swu_emulator.py` present only in full | Secondary presence check. |
| python3 apk | installed only when `INCLUDE_SWU=true` | |
| net-tools apk | installed only when `INCLUDE_SWU=true` | SWu dialer parses the real `/sbin/ifconfig` it provides; its `route` never wins over busybox in PATH. |
| bind-tools / wget apk | not installed in either | Removed (R3/R4). `dig` has no busybox applet → genuinely gone; `wget` busybox applet remains. |

**Validation / invariants**:
- The slim variant MUST contain no `python3`, no `/opt/pydeps`, no
  `/opt/SWu-IKEv2/swu_emulator.py`, and no `swu-available` marker (an empty
  `/opt/SWu-IKEv2` dir from `WORKDIR` is expected).
- The full variant MUST be a strict superset of the slim variant (same binary,
  same strongSwan assets) plus the SWu payload + python3 + net-tools.
- Exactly one variant is produced per build; the tag disambiguates.

## Entity: Tunnel-engine ↔ variant compatibility

| `[vowifi].tunnel_engine` | slim image | full image |
|---|---|---|
| `strongswan` (default) | OK | OK |
| `swu` | **fail fast at startup** (FR-004) | OK |

**State/behavior**: checked once at supervise startup in `start_vowifi_subsystem`
(beside `check_pcsc_engine_compatibility`). No runtime state transition; it is a
precondition gate that either proceeds or returns a fatal `Err(String)`.

## Value type: resolved ePDG address list

- Produced by `CommandRunner::resolve_host(host) -> io::Result<Vec<Ipv4Addr>>`.
- Consumed by `resolve_epdg_ip`, which selects the first `Ipv4Addr` (mirroring
  the previous "first A record" behavior).
- Empty vector ⇒ treated as unresolved ⇒ line is skipped (unchanged behavior).
