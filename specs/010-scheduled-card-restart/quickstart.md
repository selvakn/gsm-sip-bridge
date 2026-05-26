# Quickstart: Scheduled Card Auto-Restart

**Feature**: 010-scheduled-card-restart

A 60-second tour: enable the feature, watch the next cycle fire, inspect the result.

## 1. Add the config section (optional — defaults apply if omitted)

Edit `config.toml`:

```toml
[scheduled_restart]
enabled = true
cron = "0 1 * * *"
start_jitter_seconds = 600
inter_card_gap_seconds = 30
inter_card_gap_jitter_seconds = 15
```

Or just leave the section out entirely — the daemon will use the same defaults.

## 2. Restart the daemon

```bash
make run        # or: cargo run --release --bin gsm-sip-bridge -- --config config.toml
```

In the startup logs you will see, near the "configuration loaded" line:

```text
INFO scheduled_restart enabled
INFO   cron = "0 1 * * *" (system local time)
INFO   start_jitter = ±600s (±10 min)
INFO   inter_card_gap = 30s ± 15s
INFO   next scheduled cycle at 2026-05-27 01:00:00 +05:30 (±10 min)
```

## 3. Force an immediate test cycle

To exercise the feature without waiting until 1 AM, change the cron expression to fire in ~2 minutes. For example, if the current time is `14:23`, set:

```toml
cron = "25 14 * * *"
start_jitter_seconds = 0     # disable jitter for a deterministic test
```

Restart the daemon. At 14:25 you will see in the logs:

```text
INFO cycle-start cycle_id=1748413500 cron_tick=2026-05-26T14:25:00+05:30 actual_start=2026-05-26T14:25:00+05:30 pending_slots=[0,1,2] n_slots=3
INFO per-card-start cycle_id=1748413500 slot=0 attempt=initial
INFO per-card-outcome cycle_id=1748413500 slot=0 attempt=initial outcome=success duration_ms=18432
INFO per-card-start cycle_id=1748413500 slot=1 attempt=initial   # after gap
INFO per-card-outcome cycle_id=1748413500 slot=1 attempt=initial outcome=success duration_ms=17104
...
INFO cycle-complete cycle_id=1748413500 total=3 succeeded=3 failed=0 deferred_recovered=0 skipped=0 duration_ms=125400 next_cycle_at=2026-05-27T14:25:00+05:30
```

Each card is restarted in ascending slot order, one at a time, with a 30 s ± 15 s gap between them.

## 4. Inspect metrics

```bash
curl http://localhost:9091/metrics | grep scheduled_restart
```

Sample output:

```text
gsm_sip_bridge_scheduled_restart_total{slot="0",outcome="success"} 1
gsm_sip_bridge_scheduled_restart_total{slot="1",outcome="success"} 1
gsm_sip_bridge_scheduled_restart_total{slot="2",outcome="success"} 1
```

## 5. Disable the feature

```toml
[scheduled_restart]
enabled = false
```

Restart. The startup log will show:

```text
INFO scheduled_restart disabled (enabled = false in config)
```

No cycle will ever fire during this process lifetime.

## 6. Observe defer-then-retry with an active call

To see the active-call deferral path:

1. Set up a short test schedule (as in step 3).
2. Place an incoming GSM call to slot 0 just before the cycle's first card.
3. Watch the cycle defer slot 0, restart slot 1 and slot 2, then come back to slot 0:
   - If the call has ended by the deferred-retry moment: `slot=0 attempt=deferred-retry outcome=success`.
   - If the call is still active: `slot=0 attempt=deferred-retry outcome=skipped reason=active-call`.

## 7. Observe manual-restart concurrency

While a cycle is in progress (say, between slot 1 and slot 2):

```bash
gsm-sip-bridge card restart --slot 2
```

- If slot 2 is still pending: the manual command succeeds; the cycle records `slot=2 outcome=skipped-already-restarted-by-manual` when slot 2's turn comes.
- If slot 2 is the one being restarted by the cycle right now:
  ```text
  error: slot 2 is currently being restarted by the scheduled cycle (cycle id=1748413500)
  ```
  CLI exits with non-zero status.

## 8. Common operations

```bash
# Show all configured slots and their current state
gsm-sip-bridge card list

# Tail the scheduled-restart log entries
journalctl -u gsm-sip-bridge --since "1 hour ago" | grep -E "cycle-(start|complete)|per-card"

# Confirm the next scheduled cycle time
# (it's printed once at startup; also visible in the most recent cycle-complete entry as `next_cycle_at`)
journalctl -u gsm-sip-bridge | grep "next scheduled" | tail -1
```

## Troubleshooting

| Symptom | Likely cause | Fix |
|---------|--------------|-----|
| Startup says `scheduled_restart disabled: cron expression "..." is invalid` | Typo in cron expression | Correct the 5-field syntax. Use a tool like https://crontab.guru to validate. |
| No `cycle-start` log ever appears even after the scheduled time | `enabled = false`, or invalid cron caused auto-disable, or jitter pushed the start later than expected | Check startup log for `next scheduled cycle at …` to confirm the scheduler is armed. |
| A card always appears with `outcome=skipped-non-ready` | That card is stuck in `Recovering` or `GivenUp` | Run `gsm-sip-bridge card list` to see the current state; `card restart --slot N` to recover. |
| A scheduled cycle ran twice in one night | System clock jumped backward across the cycle | Check `journalctl` for time-sync messages; the scheduler suppresses duplicates within the same logical tick but very large backward jumps can re-arm. Use stable NTP. |
