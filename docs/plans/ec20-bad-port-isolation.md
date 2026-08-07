# Plan: Isolate a hanging serial port from wedging `discover`'s startup scan

**Triaged**: 2026-08-06 · **Effort**: small–medium · **Origin**: `docs/todo.md`
item 5

## The problem, precisely

A specific EC20 unit (`2c7c:0125`, `EC20-CE-HDLG`) has one interface —
`/dev/ttyUSB1` in the session that found this, suspected to be the GNSS/NMEA
interface based on interface number (`:1.1`) — where *any* operation
(including a bare `stty`) hangs the kernel `option` USB-serial driver
forever, uninterruptible by `timeout`/SIGTERM (confirmed via
`/proc/<pid>/task/*/stack` showing `tty_wait_until_sent`). This is a
kernel-level block, not something `AtCommander`'s userspace read-timeout can
do anything about.

Confirmed still present in current code:

- `scan_all_inner` (`gsm-sip-bridge/src/modules/discovery.rs:194-297`)
  enumerates every USB device and, via `candidate_tty_ports`
  (`discovery.rs:559-576`), collects **every** `ttyUSB*` interface with no
  filtering by interface number or type.
- `probe_at_port` / `probe_sim_status_at` (`discovery.rs:601-619`,
  `636-660`) call `AtCommander::open_with_timeout` synchronously, in-process,
  on the scan's own thread — no wrapper that could be abandoned if the
  underlying syscall blocks.
- `AtCommander::open_with_timeout` (`gsm-sip-bridge/src/modules/at_commander.rs:126-136`)
  only sets a userspace `serialport` read-timeout, which the todo's own
  stack-trace evidence shows has no effect on this class of hang.
- No blocklist/allowlist config exists. `excluded_ports_from_lines_file`
  (`discovery.rs:470-479`) is unrelated — it filters already-*resolved*
  VoWiFi ports out of the CS pool's results, it doesn't skip probing them.

Impact: this wedges the *whole daemon's startup* (not just outbound
calling), and already happened once to the deployed VoWiFi service on an
unrelated restart while this unit was attached. The only known mitigation
today is a host-level `udev`/`sysfs` unbind that doesn't survive
unplug/replug or reboot.

## Two independent fixes — recommend both

### 1. Timeout-proof the probe itself (closes the wedge generally, any port)

The kernel hang means the probe must run somewhere abandonable — a thread
whose join is itself timed, not the fd's read timeout:

- Wrap each `probe_at_port`/`probe_sim_status_at` call in
  `std::thread::spawn` and join with a bounded `recv_timeout` on a channel
  the spawned thread signals completion on (same shape as other bounded
  waits already in this codebase, e.g. `dispatch_loop`'s
  `inbound.rx.recv_timeout`). If the join times out, log a warning
  (`"port <X> did not respond within <T>; abandoning probe, port left
  unresolved"`) naming the specific port, and move on to the next candidate
  — **the scan as a whole must not block**, even though the leaked thread
  itself will sit blocked in the kernel forever (already true today, just
  currently taking the whole scan down with it; an abandoned probe thread
  holding a wedged fd is a smaller, contained cost).
- This is the general fix — it protects against *any* misbehaving port, not
  just this one already-seen unit, and needs no configuration to be
  effective.

### 2. Configurable port blocklist (cheap, operator-controlled, skips the probe entirely)

- Add a `discovery.excluded_ports` (or similar) config list of glob/exact
  `/dev/ttyUSB*` paths (or, more robustly, USB path fragments like
  `5-1.2.1.2:1.1`, since `ttyUSB` numbering isn't stable across
  replug/reboot but the USB topology position is) that `candidate_tty_ports`
  skips outright — never opened, never probed.
- This is the operator escape hatch for a known-bad port on a known unit,
  replacing the current host-level `udev unbind` workaround (which doesn't
  survive replug/reboot) with something that lives in the container's own
  config and does survive.
- Optionally, default-skip interface number `:1.1` — commonly GNSS/NMEA on
  Quectel modems per the todo's own note — but this is a guess, not
  confirmed for this unit; recommend making it configurable and *not*
  defaulting to skip it silently, so a working `:1.1` AT-capable interface
  on some other modem model isn't quietly dropped for everyone.

Fix 1 is the one that actually closes the "whole daemon wedges" gap. Fix 2 is
a nice-to-have on top (avoids paying even the per-port thread-spawn+timeout
cost for a port already known bad) but isn't sufficient alone, since it only
protects units that have already been diagnosed and configured.

## Testing

- Unit test: a fake port abstraction (or a `mio`/pipe-backed fake device)
  that never returns from open/read, assert `scan_all_inner` still completes
  within a bounded time and reports the other ports normally.
- Can't practically reproduce the actual kernel-level hang in CI (it's
  hardware/driver-specific) — the unit test above validates the
  timeout-and-move-on *mechanism*, not the specific trigger. Real
  confirmation needs the same physical unit that found this, per the
  original todo note.

## Open questions for you

1. **Probe timeout value for fix 1** — `PROBE_TIMEOUT` is currently 800ms
   for the normal (non-hung) case; the abandon-and-move-on timeout should be
   longer than that (to not falsely abandon a slow-but-working port) but
   still short enough that a full scan with one bad port doesn't take
   unreasonably long. A few seconds seems reasonable — your call.
2. **Blocklist key** — exact `/dev/ttyUSBn` path (simple, but renumbers
   across replug) vs. USB topology path (`5-1.2.1.2:1.1`, stable but less
   obvious to write by hand from `lsusb`/`dmesg` output) vs. both accepted.
3. Worth filing the host-level half separately (a udev rule blocklisting
   this port by USB topology, so it never even enumerates as a tty) as a
   parallel, non-code mitigation? That would be genuinely permanent (survives
   replug) but is infrastructure, not something this repo can carry.
