# Quickstart: Verifying VoWiFi Call and SMS Observability

How to confirm the fix works — locally with no hardware first, then on real
hardware.

---

## 1. Local, no hardware (the bulk of the verification)

```bash
make test          # full suite, including the new observability tests
make lint          # rustfmt + clippy -D warnings + unsafe ratio
```

The tests that specifically cover this feature:

| Test | Proves |
|---|---|
| `test_observability_ingest.rs` | A report over a real Unix socket lands in the real registry and shows up in the scraped text (FR-001..008) |
| `test_observability_reporter.rs` | Buffering across a dead collector, overflow accounting, resumed delivery, no counter rewind on restart (FR-019, FR-020) |
| `test_agent_liveness.rs` | A silent agent flips `agent_up` to 0 and zeroes its gauges at scrape time (FR-021, SC-009/010) |
| `test_migration_sql.rs` | A v2 database with rows migrates to v3 with every row backfilled `transport='cs'` (FR-011c/d) |
| `test_metrics_endpoint.rs` | Full metric surface with VoWiFi disabled (FR-022) |

### Poking it by hand

With a daemon running locally (`make dev`), send a report the way an agent would:

```bash
printf '%s\n' '{"cmd":"observe","report":{"agent":"ims","module_id":"ec20-TEST01",
  "state":{"active_calls":0,"registered":true,"tunnel_up":true},
  "events":[{"event":"call_completed","status":"answered","duration_seconds":12.5}],
  "dropped":0}}' | nc -U /tmp/gsm-sip-bridge.sock

curl -s localhost:9091/metrics | grep -E 'transport="vowifi"|vowifi_|agent_'
```

Expected: `calls_total{...transport="vowifi"}` at 1, a duration observation,
`vowifi_registered 1`, `vowifi_tunnel_up 1`, `agent_up{agent="ims"} 1`.

Stop sending and wait past 3× the report interval (30s by default), scrape again:
`agent_up{agent="ims"}` should be 0 and `active_calls{transport="vowifi"}` zeroed.

---

## 2. On hardware, VoWiFi enabled

Bring the stack up with `[vowifi].enabled = true`, then:

**Inbound call (User Story 1, 3)**

1. Call the SIM's number; answer at the PBX extension; hang up.
2. `curl -s localhost:9091/metrics | grep 'transport="vowifi"'` — one more
   `calls_total{status="answered"}`, a `call_duration_seconds` observation,
   `active_calls` back to 0.
3. Grafana → the existing call panels move. **No panel edits.**
4. `sqlite3 <db> 'SELECT module_id,caller_id,duration_seconds,status,transport
   FROM calls ORDER BY id DESC LIMIT 1'` — the call, with
   `transport='vowifi'` and a `module_id` matching the modem's card id.

**Failed bridge (User Story 1 scenario 2, User Story 4 scenario 3)**

Call while the PBX extension is busy or unregistered. Expect a non-answered
`calls_total`, `active_calls` back to 0, and one
`vowifi_bridge_failures_total{reason=...}` with a specific reason.

**Inbound SMS (User Story 2)**

Send an SMS to the SIM's number. Expect `sms_received_total{transport="vowifi"}`
and `sms_forwarded_total{transport="vowifi",outcome="sent"}` to increment, the
message in Discord, and a row in `sms` with `transport='vowifi'`.

**Health (User Story 4)**

`vowifi_registered` and `vowifi_tunnel_up` both 1 on a healthy bridge. Restart
the daemon alone (`pkill -f 'gsm-sip-bridge --config'` — the entrypoint restarts
it in 5s) and confirm both return to 1 within one report interval **without a
call happening first**. That is SC-009, and it is the check that would have
caught the original regression.

---

## 3. Upgrade check (FR-011d)

On a host with existing history, deploy and confirm nothing was lost:

```bash
sqlite3 <db> "SELECT value FROM meta WHERE key='schema_version'"   # 3
sqlite3 <db> "SELECT transport, COUNT(*) FROM calls GROUP BY transport"
sqlite3 <db> "SELECT COUNT(*) FROM calls WHERE transport IS NULL"  # 0
```

Pre-existing rows all read `cs`. No manual migration step, no reset.

---

## 4. What "broken" looks like

For future reference, the regression this feature fixes presents as: infra gauges
(`sip_registered`, `modules_active`, `uptime_seconds`) updating normally on a
VoWiFi deployment while `calls_total` and `sms_received_total` stay perfectly
flat despite real traffic. Everything scrapes fine — which is exactly why it went
unnoticed. `agent_up` is the metric that now makes that state legible.
