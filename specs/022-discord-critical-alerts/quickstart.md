# Quickstart: Discord Alerts for Critical Events

Manual verification steps, one per user story. Requires a running bridge
(container or `sugam-direct` hardware) and a Discord webhook URL (a
throwaway channel is fine).

## Setup

```toml
[alerts]
discord_webhook_url = "https://discord.com/api/webhooks/<id>/<token>"

[alerts.module_lifecycle]
enabled = true
at_worker_unresponsive_sec = 60

[alerts.registration_loss]
enabled = true
unhealthy_sec = 300

[alerts.tunnel_failure]
enabled = true
unhealthy_sec = 300

[alerts.missed_call]
enabled = true
```

Restart the bridge (`gsm-sip-bridge` / `docker compose restart`) to pick up
the config change (SC-003: config-only, no rebuild).

## US1 — Module/Modem Lifecycle Failure

1. Physically remove the SIM from a running EC20 module (or pull its serial
   port briefly to force a discovery failure).
2. Wait up to `at_worker_unresponsive_sec` (default 60s) past the module's
   own SIM-recovery attempts.
3. Expect one Discord message naming the module and the SIM condition.
4. Reinsert the SIM before recovery is exhausted (within the retry budget)
   on a second attempt — expect **no** alert for that incident.

## US2 — IMS/SIP Registration Loss

1. On a VoWiFi or VoLTE line, block the PBX (firewall the SIP port) or force
   the line's registration to expire.
2. Wait past `unhealthy_sec` (default 300s) — the agent's own 5s
   crash-restart loop will keep retrying underneath this.
3. Expect one Discord "registration lost" message naming the line, then a
   short "recovered" message once the block is lifted and it re-registers.
4. Restart the bridge cleanly (deliberate shutdown) — expect **no** alert
   for the resulting unregistration.

## US3 — VoWiFi Tunnel Failure

1. Block the ePDG endpoint (firewall the IKE/ESP ports) for a VoWiFi line.
2. Wait past `unhealthy_sec` (default 300s).
3. Expect one Discord "tunnel failure" message naming the line, distinct
   from any registration-loss message, then a "recovered" message once the
   block is lifted.

## US4 — Missed Call

1. Call a configured line and let it ring out without answering.
2. Expect one Discord message with caller number, receiving line, timestamp.
3. Call again and answer normally — expect no message.
4. Call again, answer, then immediately pull audio (broken/one-way audio) —
   expect no missed-call message (it's `CallStatus::Failed`, out of scope
   per Clarifications Q4).

## US5 — Per-Category Configuration

1. Set `[alerts.missed_call].enabled = false`, restart, miss a call —
   expect no Discord message, but confirm the event appears in
   `gsm_sip_bridge_critical_alerts_total{category="missed_call",
   outcome="skipped"}` and in the logs.
2. Set a category-specific `discord_webhook_url` override under
   `[alerts.module_lifecycle]` pointing at a second webhook — trigger a SIM
   failure — expect the alert at the *overridden* channel, not the shared
   default.

## Verifying no regression to existing paths

- `curl :9091/metrics | grep gsm_sip_bridge_critical_alerts_total` — new
  series present.
- Existing SMS forwarding (`[sms]` only, no `[alerts.sms]`) still delivers
  exactly as before an upgrade with no config changes.
- Place and answer a normal call, send/receive a normal SMS — no alert
  fires, and neither call setup time nor SMS delivery latency changes
  (SC-002).
