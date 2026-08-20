# Quickstart: verifying bounded modem I/O and stall detection

**Feature**: 039-at-stall-watchdog | **Date**: 2026-08-17

## Build and gate

```bash
make format
make lint          # workspace-wide, all targets, -D warnings, + deny/shellcheck/unsafe
make test
bash tools/count-unsafe.sh    # must stay at zero — the whole reason for the worker-thread design
```

## Automated verification (no hardware)

The tests that matter most, and what each proves:

```bash
cargo test at_commander      # deadline honoured, drain/resync, desync regression
cargo test watchdog          # budget derivation, two-sample confirmation, call deferral
cargo test renewal           # granted Expires + headroom scaling
cargo test health            # expiry outranks other blocked reasons
cargo test reaper            # orphan claimed, owned pid left alone
```

**The desync regression test** is the one to run if you change anything in the AT path.
It fails against the pre-fix code:

1. issue a command, let it time out;
2. have the fake modem emit the late reply;
3. issue a different command;
4. assert it returns *its own* reply, not the stale one.

**The never-responding modem test** reproduces the production fault exactly: a real
pseudo-terminal that accepts writes and never answers. Pre-fix this hangs the test
thread forever; post-fix every operation returns an error within its deadline.

## Hardware verification (Raspberry Pi, live line)

Build for arm64 with the `arm-build-200` skill, deploy, then confirm the normal path
before injecting faults — the AT layer is shared by every subsystem:

```bash
# on the Pi, exercise every AT consumer
docker exec <c> gsm-sip-bridge -c /etc/gsm-sip-bridge/config.toml discover
docker exec <c> gsm-sip-bridge -c /etc/gsm-sip-bridge/config.toml vowifi-imsi --modem /dev/ttyUSB2
docker exec <c> gsm-sip-bridge -c /etc/gsm-sip-bridge/config.toml vowifi-plmn  --modem /dev/ttyUSB2
docker exec <c> gsm-sip-bridge -c /etc/gsm-sip-bridge/config.toml vowifi-status
# then: place an inbound call, and let one SMS sweep run
```

### Fault injection

**Stall the AT port** (the actual failure):

```bash
# hold the port from another process so the agent's next AT command cannot complete
docker exec <c> sh -c 'exec 9<>/dev/ttyUSB2; sleep 600'
```

Expected, in order:

1. `vowifi-status` shows the activity over budget (`dispatch_stall_seconds` rising).
2. Within the phase budget (≤60s for a sweep), the marker appears in
   `/tmp/ims-agent-0.out`:
   `watchdog: the dispatch loop has made no progress`
   with `activity`, `phase`, `stalled_secs`, `budget_secs`, `last_at_command`.
3. The agent exits 70; the supervisor restarts the line within ~5s.
4. The line re-registers (~150s observed) and `can_answer` returns to true.

**Expired registration visibility** — confirm all three surfaces agree:

```bash
docker exec <c> gsm-sip-bridge -c /etc/gsm-sip-bridge/config.toml vowifi-status
#   expires_in: -NNNs (LAPSED) / can_answer: false / blocked_reason: the registration has expired
docker exec <c> wget -qO- http://127.0.0.1:9091/metrics | grep -E 'expires_in_seconds|agent_up|vowifi_registered'
docker ps --format '{{.Names}}: {{.Status}}'     # must show (unhealthy)
```

Pre-fix all three reported healthy for 2h45m; that disagreement is the regression to
watch for.

**Preserve a stall for diagnosis** (FR-034/FR-035): set
`[vowifi].watchdog_recovery_enabled = false`, re-inject. The stall must still appear in
the status, the metrics and the container health — only the exit is suppressed.

**Deferral during a call** (FR-029): stall the port while a call is up. Recovery must be
deferred (`watchdog: recovery deferred while a call is in progress`) and the call must
survive, until the ceiling forces recovery.

## Soak

- **7 days** on the live line with zero automatic restarts (SC-006).
- **24 hours** with a flat process count (SC-007) — baseline was 462 zombies in 3.8h:
  ```bash
  docker exec <c> sh -c 'ps -eo stat | grep -c Z'   # must not grow
  ```
- Several hourly renewals logging `registration renewed` with `expires_at` advancing.

## Forensics reference

The original incident's evidence is preserved at
`tmp/ims-stall-forensics-2026-08-16/` in the main checkout — kernel stack, blocked
syscall and fd, agent log, charon log, metrics and tty settings. Useful as the
"before" picture when validating that the "after" behaves differently.
