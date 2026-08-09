# Phase 1 Data Model: Bad-port isolation

These are in-process runtime types (no persisted schema beyond the `[discovery]`
TOML section). Names are indicative; final identifiers chosen in code.

## Entity: `PortMatcher`

A single parsed entry from `[discovery] excluded_ports`.

| Field | Type | Notes |
|-------|------|-------|
| kind | enum `DevicePath` \| `TopologyPrefix` | Determined at parse time from the string shape (starts with `/dev/` ⇒ device path; otherwise topology fragment) |
| value | `String` | The literal to compare |

**Behavior** — `matches(device_path: &Path, iface_path: &Path) -> bool`:
- `DevicePath`: `device_path == value` (exact string equality).
- `TopologyPrefix`: `iface_path_str == value` OR `iface_path_str` starts with
  `value` at a path-segment boundary (`5-1.2.1.2` matches `5-1.2.1.2:1.1`).
  Never an unanchored `contains`.

**Validation**: an empty string is ignored (no-op entry). A matcher that matches
no attached port is not an error (spec US2 scenario 4).

## Entity: `CandidatePort`

Extends today's bare `PathBuf` returned by `candidate_tty_ports`.

| Field | Type | Notes |
|-------|------|-------|
| device_path | `PathBuf` | e.g. `/dev/ttyUSB1` (today's value) |
| iface_path | `PathBuf` | The sysfs USB interface dir, whose name is the topology fragment (e.g. `.../5-1.2.1.2:1.1`) — new; needed for matching + logging |

## Entity: `DiscoveryConfig` (runtime, from `RawDiscovery`)

| Field | Type | Default | Source key |
|-------|------|---------|------------|
| excluded | `Vec<PortMatcher>` | `[]` | `[discovery] excluded_ports` |
| probe_timeout | `Duration` | `3000 ms` | `[discovery] probe_timeout_ms` |

Empty `excluded` + default timeout ⇒ behavior identical to pre-feature
(FR-008). `RawDiscovery` (in `config/raw.rs`) mirrors the TOML keys exactly via
the `section!` macro; `From<RawDiscovery>` parses each string into a
`PortMatcher`.

## Entity: quarantine bookkeeping (in-memory, part of `DiscoveryPolicy`)

Tracks consecutive timeouts across rescans; owned by the long-lived `CardPool`
(via its `Mutex<DiscoveryPolicy>`) so it persists across rescans but is cleared
on process restart (never persisted). **All keys are the stable USB-topology
interface path**, not the `/dev/ttyUSB*` device path (which is reused across
replug — a device-name key could skip a healthy replacement modem, P1-B).

| Field | Type | Notes |
|-------|------|-------|
| consecutive_at_timeouts | `HashMap<PathBuf, u8>` | AT-open-probe timeouts by iface path; reset on any non-timeout AT result |
| consecutive_sim_timeouts | `HashMap<PathBuf, u8>` | SIM-read timeouts by iface path; **separate** so the per-rescan AT success doesn't reset it (P1-A); reset on any completed SIM read |
| quarantined | `HashSet<PathBuf>` | an iface reaching 3 consecutive timeouts of *either* phase is inserted here and skipped by later scans |

**Transitions** (each counter independently):
- AT probe times out → `consecutive_at_timeouts[iface] += 1`; at 3, insert into
  `quarantined` (one-time transition `WARN`).
- AT probe returns any result → `consecutive_at_timeouts[iface] = 0`.
- SIM read times out → `consecutive_sim_timeouts[iface] += 1`; at 3, insert into
  `quarantined` (one-time transition `WARN`). Bounds the abandoned SIM-probe
  workers a persistently SIM-hanging port would otherwise leak.
- SIM read completes → `consecutive_sim_timeouts[iface] = 0`.
- next scan: a `quarantined` iface is skipped like a blocklisted one (never
  opened), logged at `DEBUG` (the transition `WARN` is the durable record).

## Entity: `ProbeOutcome` (result of probing one candidate)

As implemented, `ProbeOutcome` is the return of the per-candidate probe
(`probe_one_candidate` in production, a scripted fake in tests) consumed by
`select_at_capable_port`:

| Variant | Meaning |
|---------|---------|
| `AtCapable` | Answered `AT` with `OK`; this device path is selected |
| `NotAtCapable` | Opened/answered but not AT-capable, or a clean open failure — a real (non-timeout) result; resets the timeout streak |
| `TimedOut` | The bounded probe was abandoned; takes a quarantine strike, worker leaked, scan continues to the next candidate |

The **skip** states from earlier drafts (blocklisted, quarantined) are *not*
`ProbeOutcome` variants: they are control flow in `select_at_capable_port` that
runs before the prober is ever called (so the port is never opened — SC-003),
each emitting its own distinct log line (FR-012). FR-011: a `TimedOut`/skipped
candidate never yields a usable `at_port`; a modem whose candidates are all
`TimedOut`/skipped resolves to no usable AT port. FR-012: the `TimedOut` log
includes the `iface_path` (topology) so it is copy-pasteable into
`excluded_ports`.
