# Quickstart: Bad-port isolation

## What changed, in one line

Discovery no longer wedges when a serial port hangs the kernel driver: it
abandons that port after a timeout and keeps going, and you can pre-exclude a
known-bad port from `config.toml`.

## For operators

### A port makes discovery slow / a modem went missing

1. Look for a WARN in the logs like:
   `port=/dev/ttyUSB1 iface=…/5-1.2.1.2:1.1 timeout_ms=5000 AT probe exceeded timeout; abandoning port, left unresolved…`
   If a port keeps timing out, you'll also see a one-time
   `…quarantined for the process lifetime after consecutive probe timeouts…`
   WARN — the durable record that it has stopped being probed until restart.
2. That `iface=` value is the stable USB-topology path. Copy it into config:

   ```toml
   [discovery]
   excluded_ports = ["5-1.2.1.2:1.1"]
   ```

   To exclude the *whole* misbehaving unit (all its interfaces), drop the
   interface suffix: `excluded_ports = ["5-1.2.1.2"]`.
3. Restart the service. The port is now never opened or probed.

### Notes

- Topology fragments survive replug/reboot; `/dev/ttyUSBn` paths do not — prefer
  the topology form for anything you want to stay excluded.
- You do **not** need this config for the daemon to survive a hung port — that
  protection is automatic (the timeout + 3-strike quarantine). The blocklist is
  the permanent, no-per-rescan-cost escape hatch for a known-bad port.
- The abandon budget defaults to 5000ms (covers the SIM read: open + `AT+CPIN?`
  + `AT+CIMI`). Raise it only if a genuinely slow-but-healthy modem is still
  being falsely abandoned: `probe_timeout_ms = 8000`. Values below 1000 are
  clamped up (a lower value would abandon every port and quarantine all modems).

## For developers — verifying the mechanism

The kernel hang itself needs the specific physical unit; CI validates the
*mechanism* against a fake never-responding port.

```bash
make format && make lint && make test
```

Key tests (integration-first at the two testable seams, no real hardware):
- `run_bounded_abandons_work_that_never_finishes` — a never-returning closure
  (the fake for a wedged open) is abandoned at ~the timeout, on a real thread.
- `run_bounded_returns_a_slow_but_healthy_result` — a slow-but-working probe is
  NOT falsely abandoned.
- `select_at_capable_port` tests with a scripted `probe_one`:
  abandon-then-continue-to-the-next-candidate; all-timeout ⇒ no usable port
  (FR-011); a blocklisted port and a quarantined port are never handed to the
  prober (SC-003).
- Quarantine counter: 3 consecutive timeouts quarantine a port; a success resets
  the streak; distinct ports are tracked independently.
- `PortMatcher`: exact device path, exact topology, whole-device prefix, and a
  non-anchored substring that MUST NOT match.
- Empty `[discovery]` ⇒ results identical to a no-config baseline (FR-008), and
  `tests/test_config_docs.rs` stays green (new keys documented).

The one layer left to real hardware is the serial `open` itself
(`probe_one_candidate` / `candidate_tty_ports` over sysfs) — the same boundary
the module already leaves untested for `probe_sim_status_at`.
```
