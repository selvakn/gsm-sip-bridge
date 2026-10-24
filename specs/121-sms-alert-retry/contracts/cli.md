# Contract: `gsm-sip-bridge sms-status`

Reads the store at `[sms].db_path` (read-only) — works with the daemon down.

```text
SMS Discord delivery
  delivered: 412   pending: 0   failed: 2   abandoned: 1   skipped: 0
  oldest undelivered: 1h 12m

Undelivered / abandoned (newest first, max 20):
  id   module   transport  received_at           attempts  last_attempt        last_error
  431  line-1   vowifi     2026-10-24T08:01:12Z  2         2026-10-24T09:05Z   unreachable
```

- Flag `--json` prints the same data as one JSON object (`counts`, `oldest_undelivered_age_sec`, `rows[]`).
- Exit code 0 even when failures exist (it is a report, not a health check); non-zero only if the store can't be opened.
- Message bodies are never printed (PII); sender is shown as stored.
