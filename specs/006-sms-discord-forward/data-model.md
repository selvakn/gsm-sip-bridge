# Data Model: SMS to Discord Forwarding

## Entity: SmsRecord

Represents a single received SMS persisted in the local SQLite database.

| Field | Type | Constraints | Description |
|-------|------|-------------|-------------|
| id | INTEGER | PRIMARY KEY AUTOINCREMENT | Unique record ID |
| sender | TEXT | NOT NULL | Originating phone number (E.164 or local format as received) |
| body | TEXT | NOT NULL | SMS message body (may be empty string for empty SMS) |
| received_at | TEXT | NOT NULL | ISO 8601 timestamp from module (UTC) |
| module_id | TEXT | NOT NULL | Serial number of the EC20 module that received the SMS |
| discord_status | TEXT | NOT NULL DEFAULT 'pending' | Forwarding status: pending, sent, failed, skipped |
| forwarded_at | TEXT | NULLABLE | ISO 8601 timestamp when Discord POST succeeded (NULL if not sent) |

### State Transitions

```
pending → sent       (Discord POST returned 2xx)
pending → failed     (Discord POST returned non-2xx or network error)
pending → skipped    (No webhook URL configured)
```

### SQL Schema

```sql
CREATE TABLE IF NOT EXISTS sms (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    sender        TEXT    NOT NULL,
    body          TEXT    NOT NULL,
    received_at   TEXT    NOT NULL,
    module_id     TEXT    NOT NULL,
    discord_status TEXT   NOT NULL DEFAULT 'pending',
    forwarded_at  TEXT
);

CREATE INDEX IF NOT EXISTS idx_sms_received_at ON sms(received_at);
CREATE INDEX IF NOT EXISTS idx_sms_module_id ON sms(module_id);
```

## Entity: SmsConfig

Configuration section parsed from `config.ini`. Not persisted.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| discord_webhook_url | string | "" (disabled) | Full Discord webhook URL |
| db_path | string | "/var/lib/gsm-sip-bridge/sms.db" | Path to SQLite database file |
| enabled | bool | true | Master enable/disable for SMS monitoring |

### INI Format

```ini
[sms]
enabled = true
discord_webhook_url = https://discord.com/api/webhooks/123456/abcdef
db_path = /var/lib/gsm-sip-bridge/sms.db
```

## Entity: DiscordEmbed

Outgoing webhook payload (not persisted).

```json
{
  "embeds": [{
    "title": "SMS Received",
    "color": 3447003,
    "fields": [
      {"name": "From", "value": "+1234567890", "inline": true},
      {"name": "Module", "value": "ec20-A1B2C3", "inline": true},
      {"name": "Time", "value": "2026-05-03 14:30:00", "inline": true}
    ],
    "description": "Message body here"
  }]
}
```

## Prometheus Metrics (Additions)

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| gsm_bridge_sms_received_total | Counter | module_id | Total SMS messages received |
| gsm_bridge_sms_forwarded_total | Counter | module_id, status | Discord forwarding outcomes (sent/failed/skipped) |
| gsm_bridge_sms_db_writes_total | Counter | status | SQLite persistence outcomes (success/failure) |
