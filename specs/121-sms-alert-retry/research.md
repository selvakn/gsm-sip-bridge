# Research: SMS Alert Delivery Status and Retry

## R1. Where does delivery state live?

- **Decision**: Extend the existing `sms` table (schema v5). No separate outbox table.
- **Rationale**: The row already carries `forwarding_status`, `forwarded_at`, `discord_status_code`, is written by every transport (cs/vowifi/volte), and `recent_sms` / `operations.md` already point operators at it. A second table would need joins and a second source of truth. Requested explicitly: "track the status using the sqlite database".
- **Alternatives**: separate `sms_outbox` table (rejected: duplicate state, more migration); in-memory queue (rejected: lost on restart, violates FR-007).

## R2. Duplicate-send safety across processes

- **Facts**: The daemon, VoWiFi agent B (own process, own `sms_runtime`), and host-side VoLTE all write one DB file (`store/mod.rs` BUSY_TIMEOUT comment). Each can hold a `DiscordClient` for the SMS webhook. A timer in each process would otherwise race.
- **Decision**: A **lease column** `claimed_until` (unix seconds). A sender claims a row with a single conditional `UPDATE … SET claimed_until = now+lease WHERE id = ? AND forwarding_status IN ('pending','failed') AND (claimed_until IS NULL OR claimed_until <= now)` and proceeds only if one row changed. SQLite serialises writers, so exactly one process wins. Lease length 120 s (> the 30 s total post timeout, with margin).
- **First attempt**: `record_and_forward` inserts the row **already leased**, so a concurrent sweep never double-posts a text whose first attempt is still in flight. If the process dies mid-send the lease expires and the sweep picks the row up (pending-with-expired-lease) → at-least-once, matching the spec's accepted limitation.
- **Alternatives**: in-process `Mutex` (rejected: doesn't cover other processes); single designated "retry owner" process (rejected: needs election/failover, and the daemon may be down while agent B is up); file lock (rejected: `serialport-already-locks` lesson — extra locks self-conflict).

## R3. Retry triggers

- **Decision**: (a) after a fresh text's first attempt completes, run a sweep (spec: "next alert"); (b) per-process timer, default 3600 s, first tick one interval after start. There is deliberately no extra sweep at startup: the spec says "next alert or long interval", and a restart storm (container recreation, hourly churn) would otherwise hammer Discord. Rows therefore wait at most one interval after a restart (SC-001 holds).
- **Why sweep after, not before, the first attempt**: FR-009 — the new text must never wait behind a backlog.
- **Timer anchoring** (deferred from clarify): `tokio::time::interval` with `MissedTickBehavior::Delay`, so a clock jump or a stalled runtime produces one late tick, not a burst (spec edge case).
- **Alternatives**: exponential per-row backoff (rejected by the user: "next alert or long interval of an hour").

## R4. Age limit instead of attempt cap (clarified)

- **Decision**: `first_seen_at` (unix seconds, set on insert) is the age basis. Sweep first marks undelivered rows with `first_seen_at < now - retry_max_age_sec` as `abandoned`, then claims the rest.
- **Rationale**: `received_at` is a free-form string whose format differs by transport (CS path vs SIP `MESSAGE` `Date`), so comparing it numerically is fragile; ordering and age use `first_seen_at`/`id` instead. `received_at` is still what the operator sees in the late-delivery embed.

## R5. Pre-existing failed rows are not retried (clarified)

- **Decision**: v5 adds `retry_eligible INTEGER NOT NULL DEFAULT 0`. The migration leaves every existing row at 0; new rows are inserted with 1 when a webhook is configured. The sweep only considers `retry_eligible = 1`.
- **Rationale**: expresses "only failures after the feature is enabled" in data, with no time-of-upgrade comparison and no special-casing in the sweep. A skipped row (no webhook) is inserted with `retry_eligible = 0` and state `skipped`, so a later webhook configuration doesn't resurrect it (FR-015).

## R6. Failure classification and rate limits

- **Facts**: `post_with_retry` returns `Result<u16, String>`; failure causes are only strings ("client error 404", "network error after retries: …", "total timeout exceeded"). On 429 it sleeps `retry-after` and retries up to 3 times.
- **Decision**: introduce a typed `PostError { Unreachable, Timeout, Rejected(u16), RateLimited{retry_after}, Exhausted(u16) }` inside `alerts/discord.rs`; `send_alert` keeps its `Result<u16,String>` signature through `Display`, so critical alerts are untouched. `forward_sms` returns the typed error. The persisted `last_error` is the stable category string (`unreachable|timeout|rejected:<code>|rate_limited|server_error:<code>`), never a raw `reqwest` message (may contain the webhook URL — secret).
- **Rate limit during a sweep**: when a retry attempt ends `RateLimited`, the sweep releases that row's lease, stops, and the next trigger continues (FR-011). The inline 429 wait in `post_with_retry` still applies to the single in-flight post.
- **Alternatives**: parse strings (rejected: brittle); change `post_with_retry` for everyone (rejected: critical alerts must not change).

## R7. Observability across processes

- **Facts**: Prometheus counters incremented in agent B land in a registry nobody scrapes (`sms::record_and_forward` doc comment; spec 014). Only the daemon serves `/metrics`.
- **Decision**: expose **gauges derived from the database at scrape time** in `metrics_handler` (`gsm_sip_bridge_sms_undelivered{state}`, `…_sms_oldest_undelivered_age_seconds`), via a cheap read-only query. This is correct regardless of which process sent or retried. A new `sms-status` CLI reads the same summary for operators without Prometheus.
- **Existing counter**: `SMS_FORWARDED_TOTAL{outcome}` keeps its meaning (outcome of each attempt, including retries) via the existing reporter path, so no new cross-process plumbing is needed.
- **Alternatives**: report undelivered counts over the control socket (rejected: more protocol surface, stale when an agent is down).

## R8. Late-delivery presentation (clarified)

- **Decision**: same embed as today, plus title prefix `[Late] ` and a field `Originally received: <received_at>` for retried rows only. Fresh-text payload stays byte-identical (FR-016, SC-006).

## R9. Testing the Discord side

- **Decision**: a small in-test `tokio::net::TcpListener` HTTP responder returning scripted statuses (204, 500, 429+Retry-After, dropped connection). `DiscordClient` already takes any webhook URL, so no production seam is added.
- **Justification (constitution I)**: real Discord can't be called from CI; a real local HTTP server exercises the real `reqwest` client, unlike mocking the client.
