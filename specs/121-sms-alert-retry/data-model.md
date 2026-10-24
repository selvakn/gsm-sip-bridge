# Data Model: SMS Alert Delivery Status and Retry

## Table `sms` (schema v5)

Existing columns are unchanged: `id`, `module_id`, `sender`, `body`, `received_at`, `forwarding_status`, `forwarded_at`, `discord_status_code`, `transport`.

New columns:

| Column | Type | Default | Meaning |
|---|---|---|---|
| `attempt_count` | INTEGER NOT NULL | 0 | Discord post attempts made (first attempt included). |
| `last_attempt_at` | INTEGER | NULL | Unix seconds of the most recent attempt. |
| `last_error` | TEXT | NULL | Stable failure category of the most recent failed attempt; cleared on success. |
| `first_seen_at` | INTEGER NOT NULL | 0 | Unix seconds when the row was inserted; basis for age limit and ordering. Back-filled to `strftime('%s','now')` for pre-existing rows (irrelevant: they are not retry-eligible). |
| `claimed_until` | INTEGER | NULL | Lease expiry (unix seconds). A row is being attempted while `claimed_until > now`. |
| `retry_eligible` | INTEGER NOT NULL | 0 | 1 = this row participates in retry. 0 for every pre-v5 row and for `skipped` rows. |

`forwarding_status` CHECK is widened to `('pending','sent','failed','skipped','abandoned')`. Because SQLite cannot alter a CHECK, the migration uses the project's established rebuild pattern (`sms_v5` → drop → rename), then recreates indexes and the `recent_sms` view (adding the new columns to the view).

New index: `idx_sms_retry ON sms(retry_eligible, forwarding_status, claimed_until)`.

## State machine

```text
            insert (webhook set, leased)                insert (no webhook)
                       │                                       │
                       ▼                                       ▼
                   pending ──attempt ok──► sent             skipped (terminal)
                       │
                  attempt fails (lease released)
                       ▼
                    failed ──retry ok──► sent (terminal)
                       │  ▲
                       │  └── retry fails (attempt_count++, last_error)
                       │
        age > retry_max_age_sec (checked at start of each sweep)
                       ▼
                  abandoned (terminal, last_error preserved)
```

`pending` with an expired lease (process died mid-send) is treated like `failed` by the sweep.

## Eligibility predicate (the sweep's SELECT)

```sql
retry_eligible = 1
AND forwarding_status IN ('pending','failed')
AND (claimed_until IS NULL OR claimed_until <= :now)
ORDER BY id ASC
LIMIT :batch
```

Claim is a separate conditional `UPDATE` per row (see research R2); the SELECT is only a candidate list.

## Rules

- A row in `sent`, `skipped` or `abandoned` is never selected (FR-006, FR-015).
- Pre-v5 rows have `retry_eligible = 0`: never retried, never abandoned by the sweep, only reported under their historical `failed` status.
- `attempt_count` increments when an attempt *starts* (at claim), so a crash mid-send still shows the attempt.
- Secrets: `last_error` never stores URLs or response bodies.

## Summary view (for gauges and CLI)

`SELECT forwarding_status, COUNT(*) … WHERE retry_eligible = 1 GROUP BY 1` plus `MIN(first_seen_at)` over `pending`/`failed` rows → oldest undelivered age.
