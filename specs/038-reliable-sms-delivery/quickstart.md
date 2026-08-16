# Quickstart: Verifying Reliable SMS Delivery

## Automated

```bash
make format
make lint
make test
```

Relevant test targets after implementation:

- `cargo test -p gsm-sip-bridge --test test_volte_sms` — existing VoLTE dedupe/route logic, extended to cover the newly-shared `Dedupe` wiring and `MessageRoute::OverRegistration` now being reachable from production code paths.
- `cargo test -p gsm-sip-bridge --test test_vowifi_sms_reader` (new) — mirrors `test_volte_sms.rs` for the VoWiFi wiring: modem-sweep spawn decision (`!config.pcsc_reader`), shared-dedupe cross-bearer suppression, and `parse_cmgl_indexes`/backlog-recovery reuse.

## Manual, on real hardware (the way this bug was originally found and confirmed)

1. Deploy with a VoWiFi (or VoLTE) line configured, `[cs].enabled = false`, against a real modem (`pcsc_reader = false`).
2. Confirm the line registers normally (`vowifi-status` / `volte-status`).
3. From outside, send a long (multi-part) SMS to the line's number.
4. Watch the operator's Discord channel and the `sms` table (`sqlite3 <db_path> "select * from recent_sms"`) — every part should arrive, whichever bearer the carrier used.
5. Cross-check against the modem's own storage directly (read-only, on a spare AT port not held by another process): `AT+CMGF=1` then `AT+CMGL="ALL"` should show the messages as no longer `"REC UNREAD"` once relayed (cleared after successful relay+forward), and nothing should accumulate indefinitely.
6. To exercise the duplicate-suppression path deliberately, note that a carrier occasionally delivers the same text over both bearers — if observed, confirm the operator sees it exactly once.

## What "done" looks like

- A VoWiFi-only deployment no longer has any text silently stuck, unread, in modem storage — the exact failure mode that triggered this feature (7-part SMS, 6 of 7 parts stuck).
- No deployment shows a duplicate notification for a message the carrier delivered over both bearers.
- CS-only deployments show no behavior change at all.
