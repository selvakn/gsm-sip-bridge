# Quickstart: verifying SMS delivery retry

## Automated

```bash
make format && make lint && make test
```
Key tests: `test_sms_retry_store`, `test_sms_retry_sweep`, `test_sms_status_metrics`.

## Manual (local docker test dir, see `test/`)

1. Point `[sms].discord_webhook_url` at a dead address (e.g. `http://127.0.0.1:9/hook`), set `retry_interval_sec = 60`.
2. Inject or receive a text. `gsm-sip-bridge sms-status` shows `failed: 1`, `last_error: unreachable`.
3. Fix the webhook (or start a listener on it) and wait up to 60 s. Status becomes delivered; Discord shows `[Late] SMS from …` with the original time.
4. Restart the container with a failed row present: it is retried after one interval, and not re-sent if already delivered.
5. `curl :9091/metrics | grep sms_undelivered` reflects the same counts.
