# Quickstart: Read-only management web UI

## Enable it

1. Add to `config.toml`:

   ```toml
   [webui]
   enabled = true
   password = "env:WEBUI_PASSWORD"
   # port = 8080            # default
   # listen_addr = "0.0.0.0"
   # grafana_url = "https://grafana.example/d/abc"
   ```

2. Put the password in the container's `.env`, using an obviously fake value in anything you commit:

   ```sh
   WEBUI_PASSWORD=change-me
   ```

3. Recreate the container. A restart isn't enough for config/env changes; see the "switching transports" note in the README.

   ```sh
   docker-compose up -d --force-recreate
   ```

4. Open `http://<host>:8080/` and log in.

There's no built-in TLS. For access beyond a trusted LAN, put the UI behind a reverse proxy with TLS (Caddy, nginx) or a VPN, and keep `listen_addr` on the LAN or loopback interface.

## Local development loop

```sh
make build
make test                                   # includes tests/test_webui_*.rs
cargo run -p gsm-sip-bridge -- --config test/config.toml
```

With `[cs].enabled = false`, `[vowifi].enabled = false` and `[volte].enabled = false`, the daemon still serves the UI. The overview says "no lines", and the Calls, SMS and Events pages read whatever is in `[sms].db_path`. This is the fastest way to iterate on the frontend.

`app.js` and `style.css` are embedded at compile time, so `cargo run` again after editing them.

## Verify (automated: `make test`)

| Check | Test file |
|---|---|
| Every `/api/*` route returns 401 without a session, and a wrong password returns 401 (SC-005) | `test_webui_auth.rs` |
| 5 bad logins → 429, then a good login is refused until the lockout expires | `test_webui_auth.rs` |
| Logout and a "restart" (new router instance) both invalidate the session | `test_webui_auth.rs` |
| `enabled = true` with an empty password → config error | `test_config.rs` |
| Agent reports drive `/api/overview`: registered/unregistered, stale after 3× interval, health reasons | `test_webui_overview.rs` |
| Calls page filters, today/7-day summary, outbound rows | `test_webui_calls.rs` |
| SMS filter and search, read-only (no `StoreCommand` issued), markup returned verbatim as JSON text | `test_webui_sms.rs` |
| Events: raise → resolve with duration, dedup of retried Failure, survives a store reopen, retention prune | `test_webui_events.rs` |
| Config: no secret sentinel or resolved `env:` value appears (SC-006) | `test_webui_config.rs` |
| `app.js` contains no `innerHTML`/`outerHTML`/`insertAdjacentHTML`/`document.write` | `test_webui_assets.rs` |
| `AT+CSQ`/`AT+QENG` parsing, `99` → `None` | unit tests in `modules/signal.rs` |
| Schema v4 → v5 migration keeps rows, backfills `direction='inbound'` | `test_migration_sql.rs` |
| Disabled UI: `/metrics` output unchanged apart from the new series (SC-009) | `test_metrics_endpoint.rs` |

## Verify on hardware (manual, record results in `live-verification.md`)

1. **Signal, CS line** (local Vodafone host): watch `gsm_sip_bridge_signal_rssi_dbm{transport="cs"}` on `/metrics` for 3 minutes. It must update about every 60s, and must not update during a call.
2. **Signal, VoWiFi line** (Jio Pi):
   - Confirm `signal` appears on the line card within about 80s of registration.
   - Run a forced re-registration and check the charon log for EAP-SIM `6F00` / "card mute" responses. There must be **none**, which proves the poll doesn't starve `vowifi-usim-bridge`.
3. **Health turn-around (SC-003)**: on the Pi, black-hole one P-CSCF (see the memory note on doing this without iptables) or stop charon. The line card must turn `down`/`degraded` within 15s of `vowifi-status` showing it.
4. **Timeline (SC-004)**: after step 3 recovers, `#/events` shows the raise, the resolve and the duration. It must still be there after `docker-compose restart`.
5. **Outbound**: place one answered and one rejected outbound call from a SIP client. Both appear under `direction=outbound` with the right status.
6. **Footprint (SC-007)**: compare `docker images` size for the feature build against `main`. The difference must be under 1 MB.
7. **Load (SC-008)**: with three tabs open on `#/overview` for 10 minutes, `top` on the Pi shows the daemon at under 2% CPU above the baseline. Call setup and quality figures stay within run-to-run noise.
8. **Phone width**: open the UI on a phone. There must be no horizontal scroll, and the OTP copy button must work.
