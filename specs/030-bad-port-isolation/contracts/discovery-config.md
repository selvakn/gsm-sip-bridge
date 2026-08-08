# Contract: `[discovery]` config + discovery behavior

This feature's external surface is (a) a new `config.toml` section and (b) the
observable behavior/log contract of the scan. No network/RPC API changes.

## Config contract — `[discovery]`

```toml
[discovery]
# Ports to never open or probe during discovery (startup or rescan).
# Each entry is either:
#   - an exact device path:        "/dev/ttyUSB1"
#   - a USB-topology fragment:     "5-1.2.1.2:1.1"  (a full interface)
#                                  "5-1.2.1.2"      (whole device: all its interfaces)
# Topology fragments are matched by exact-equality OR leading path-prefix
# (segment-aligned); they survive replug/reboot where /dev/ttyUSBn renumbers.
# Prefer the topology form for a known-bad unit.
excluded_ports = []

# Per-port probe abandon timeout, milliseconds. A port whose open/probe does not
# complete within this budget is abandoned and the scan continues. Must exceed a
# slow-but-healthy probe. It bounds the SIM read too (open + AT+CPIN? + AT+CIMI),
# so the default is 5000, not just an open's worth. Values below 1000 are
# clamped up (too low would abandon every port and quarantine all modems).
probe_timeout_ms = 5000
```

- Both keys are OPTIONAL; the whole section is OPTIONAL.
- **FR-008**: an absent `[discovery]` section (or `excluded_ports = []`) MUST
  yield behavior identical to the pre-feature build.
- Unknown keys under `[discovery]` are an error (`deny_unknown_fields`, per the
  `section!` macro), consistent with every other section.
- `tests/test_config_docs.rs` MUST see these keys documented in
  `docs/configuration.md`, and the `[discovery]` section present in
  `config.toml.example` — the reference/keys parity tests.

## Matching contract

| Entry | Matches device `/dev/ttyUSB1` at iface `.../5-1.2.1.2:1.1` | Matches iface `.../5-1.2.1.2:1.0` |
|-------|-----------------------------------------------------------|-----------------------------------|
| `/dev/ttyUSB1` | ✅ exact device path | ❌ |
| `5-1.2.1.2:1.1` | ✅ exact topology | ❌ |
| `5-1.2.1.2` | ✅ device prefix (all interfaces) | ✅ device prefix |
| `1.2` (unanchored) | ❌ MUST NOT match (no substring) | ❌ |

## Behavior / log contract

- **Timeout (FR-002, FR-004, FR-012)**: a port whose AT probe exceeds
  `probe_timeout_ms` emits a WARN naming the device path AND the USB interface
  (topology) path, and the scan proceeds. Example intent:
  `WARN port=/dev/ttyUSB1 iface=5-1.2.1.2:1.1 timeout_ms=5000 "AT probe exceeded timeout; abandoning port…"`
- **Quarantine (FR-013)**: the scan that first crosses 3 consecutive timeouts on
  a port emits a one-time `WARN` (with the iface path) stating it is quarantined
  for the process lifetime; subsequent scans skip it at `DEBUG` (the WARN is the
  durable record). A SIM-read-phase timeout (as opposed to the AT open) does
  **not** count toward this — a port that already answered `AT` is not
  quarantined for slow SIM reads.
- **Blocklist skip (FR-007, FR-012)**: a port matched by `excluded_ports` is
  never opened; an `INFO` line (visible at default log level, US3 scenario 2)
  names the port and states it was skipped by the exclusion list.
- **Isolation (FR-003, FR-011)**: an abandoned/skipped/quarantined port never
  appears as a usable modem in scan results, and never delays other ports.

## Non-goals (out of contract)

- Host-level udev/unbind rules (infrastructure; spec Assumptions).
- Auto-persisting a quarantined port into `excluded_ports` (quarantine is
  in-memory only).
- Live config reload — changing `excluded_ports` takes effect on next process
  start (documented in quickstart).
