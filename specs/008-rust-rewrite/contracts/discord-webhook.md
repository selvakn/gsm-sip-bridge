# Contract — Discord Webhook (SMS Forwarding)

**Direction**: Outbound HTTP POST from gsm-sip-bridge to a Discord webhook URL.
**Client**: `reqwest` with `rustls-tls`, `json` features.
**Triggered by**: An SMS arrival on any module (FR-030..032).
**Spec links**: User Story 2, FR-030, FR-031, FR-032, FR-078, R-09

## Request shape

`POST {discord_webhook_url}`

Headers:
- `Content-Type: application/json`
- `User-Agent: gsm-sip-bridge/5.0.0`

Body — Discord embed payload (single embed per SMS):

```json
{
  "embeds": [
    {
      "title": "SMS from <sender>",
      "description": "<body>",
      "timestamp": "2026-05-05T10:15:23.000Z",
      "color": 3447003,
      "fields": [
        { "name": "Module",  "value": "<module_id>", "inline": true },
        { "name": "Sender",  "value": "<sender>",    "inline": true }
      ],
      "footer": { "text": "gsm-sip-bridge" }
    }
  ]
}
```

- `<sender>` is the SMS sender's MSISDN as decoded from the PDU.
- `<body>` is the plain-text SMS body (UTF-8). For concatenated SMS, the bridge reassembles before posting.
- `<module_id>` is the persisted module identifier (e.g. `ec20-A1B2C3`).
- `timestamp` is the SMS arrival time on the bridge (matches `received_at` in the SMS row).
- `color` is `0x3498DB` (blue) — informational embed.

## Length safety

- Discord embed `description` limit is 4096 characters. The bridge truncates `body` at 4090 characters and appends `…` if the original is longer. The full body is always preserved in the persisted store.
- Embed `fields[].value` limit is 1024 characters. `module_id` and `sender` are short enough to never hit this; defensive truncation is still applied.

## Response handling

| HTTP status | Action |
|---|---|
| `200` `204` | Mark `forwarding_status = sent`, set `forwarded_at = NOW`, record `discord_status_code`. |
| `429` (rate limited) | Read `Retry-After` header (seconds). Sleep that long. Retry. Counts as one of 3 retries. If `Retry-After` is absent, fall back to exponential backoff. |
| `5xx` | Retry with exponential backoff. |
| `4xx` other than `429` | Terminal failure. Mark `forwarding_status = failed`, record `discord_status_code`. Log a `WARN` with the response body (truncated to 256 chars) so the operator can diagnose. |
| Network error / timeout | Treat like `5xx`: retry with backoff. |

## Retry policy

- Maximum retries: 3 (so up to 4 total attempts including the initial POST).
- Backoff schedule (when no `Retry-After` is provided): 1s, 2s, 4s.
- Total time budget: 30 seconds. After this, abandon and mark `failed`.
- Each retry attempt counts toward `gsm_sip_bridge_sms_forwarded_total{outcome="failed"}` only on the final abandonment, not on individual retry attempts.

## Concurrency and ordering

- Forwards are dispatched as independent tokio tasks; multiple SMS may be in-flight to Discord simultaneously.
- Persistence to the local store happens in the SMS reader task BEFORE the forward task is spawned (FR-031).
- Ordering: Discord receipt order is best-effort; the local store is the source of truth for arrival order.

## Disabled / empty webhook URL

- If `discord_webhook_url` resolves to an empty string at startup (because the config value was `""` or `env:UNSET_VAR` was permitted-empty — note: per FR-077, an unset env-ref is a startup error, so this case requires the literal empty string in the config), SMS forwarding is disabled:
  - SMS rows are still written with `forwarding_status = skipped`.
  - No HTTP POST is made.
  - No log noise per SMS — a single `INFO` is emitted at startup explaining the SMS-disabled state.

## Secret handling

- The webhook URL is a `Secret<String>` end-to-end (R-10).
- The URL is never logged. If a `WARN` log includes a Discord response body, the bridge does not include the URL itself in that log line.
- Metrics never include the URL or any portion of it as a label.

## Test contract

- `tests/test_sms_discord.rs` uses `wiremock` to:
  - Assert the JSON body matches the structure above for representative SMS (ASCII, UTF-8 emoji, multi-segment concatenated, body length 4097).
  - Assert retry/backoff behaviour against scripted `429` and `503` responses.
  - Assert `forwarding_status` transitions match the response status.
  - Assert the URL is not present in any captured log line.
- Justification (per Constitution): Discord cannot be hit from CI; `wiremock` is the lightest-weight stand-in that exercises the real `reqwest` HTTP path.
