# Live verification: Read-only management web UI

**Status**: done on the Jio Pi except step 1 (a CS line, needs the local host) and the live refusal rows in step 5. Step 2, the merge-blocking one, passed.

Never paste real numbers, IMSIs or secrets into this file. Use `+919000000000`-style placeholders.

## Checked without hardware

Date: 2026-10-24, on a development host, with the debug build of `target/debug/gsm-sip-bridge`.

### The real daemon, UI only (quickstart "Local development loop")

Config: `[cs] enabled = false`, no VoWiFi or VoLTE, `[webui]` on `127.0.0.1` with the password given as `env:` and a synthetic value. Driven with `curl`:

| Check | Result |
|---|---|
| `GET /api/overview` without a cookie | `401 {"error":"unauthenticated"}`, with the CSP, `nosniff`, `no-referrer` and `no-store` headers |
| Wrong password | `401`, no cookie |
| Correct password | `204` with `HttpOnly; SameSite=Strict; Path=/; Max-Age=604800` |
| `overview`, `health`, `calls/summary`, `calls`, `sms`, `events`, `config` with the cookie | all `200`; empty store gives `no_lines` and zero counts |
| `/config` password | `[REDACTED]` for a literal; the `env:` reference is shown as written |
| `/` and `/assets/app.js` with and without a session | page needs one, assets do not |
| UI port serving `/metrics`, metrics port serving `/api/*` | neither (404) |
| `gsm_sip_bridge_webui_logins_total` on `/metrics` | present, `invalid=1`, `success=1` |
| UI port already in use | covered by `tests/test_webui_disabled.rs`: an error, not a panic |

One thing seen in this run: with a long scratchpad path the control socket failed to bind (`path must be shorter than SUN_LEN`). That is an existing limit of `[control].socket_path` and is unrelated to the UI; the UI came up regardless.

### The frontend

- `app.js` run in Node against a fake DOM and mock API responses (all five pages and the login page): no runtime errors; every page renders its content (cards, tiles, tables, timeline, config tree), and the hostile SMS body appears as text.
- `tests/test_webui_assets.rs` fails on any HTML sink, inline script or style, external reference, or more than 150 KB.
- Assets: `app.js` 36.8 KB, `style.css` 9.3 KB, `index.html` 0.3 KB, 46.4 KB together.
- **Not done**: looking at the pages in a browser. Chrome and Firefox could not run in the development sandbox. The layout, colours and the 640 px breakpoint are unseen.

## On the Jio Pi (2026-10-24)

Image `gsm-sip-bridge:webui122-arm64`, built natively on 10.13.13.200 and loaded onto the Pi; `[webui]` enabled with the password in `.env`. Driven with dev-browser, `curl` and a locally run siptest.

| # | Step | Result |
|---|---|---|
| 1 | Signal on a CS line | **Not run.** The Pi has no CS line; needs the local host |
| 2 | Signal on the VoWiFi line; no EAP-SIM `6F00` / card mute | **Pass.** Signal on the card from first registration; `signal_*` gauges exported. A `docker-compose restart` re-registered the line with polling on and logged 0 matches for `6F00`, `card mute` or `CSIM fail`. The log search covered 10 to 20 minutes after each start |
| 3 | Health turn-around | **Pass.** A blackhole route to the ePDG injected inside the container: card went `degraded` / `uplink_held` (`uplink: no_route`) 40 s after injection, at the supervisor's next tick, and back to `ok` within 10 s of the route being removed. A P-CSCF black-hole was not used |
| 4 | Timeline | **Pass.** `uplink_hold_started` and `uplink_hold_ended` (61 s) were recorded, and both were still listed after `docker-compose restart` |
| 5 | Outbound rows | **Pass for answered and missed.** An answered 10 s call and an abandoned one each left exactly one outbound row (`vowifi`, line `ec20-11`). Refusal rows (invalid destination, no idle line) were **not** exercised live: siptest refuses an invalid destination itself, so the SIP-side process never saw one. That path is covered by unit tests |
| 6 | Footprint | **Pass.** `docker image inspect` on the Pi: 41,114,791 B against 40,392,887 B for `latch-e2e5e7c-arm64`, the image it replaced, a difference of 721,904 B (about 705 KiB). That comparison is against the previously deployed image, not a fresh `main` build |
| 7 | Load | **Pass.** Container CPU averaged 1.0% with no tabs and 2.0% with three tabs open for 10 minutes (one 11.8% sample), a difference of about 1 point. The container figure covers the whole bridge, not only the UI |
| 8 | Phone width | **Pass** at 390 px: no horizontal scroll, nav wraps. The copy button was not tapped on a real phone |

Seen while doing this:

- A restart clears every session (by design), so open tabs return to the login page.
- The `baresip-amr` container on the Pi also registers as 1002. A second 1002 client replaces the first, so a test call can get `403` until it re-registers; restart siptest and dial straight away.
- The Number field on the card is `-` because the Pi's `[[vowifi.line]]` has no `msisdn`.

Things worth looking at while doing them:

- **SINR scale.** `modules/signal.rs` converts the `AT+QENG` SINR with Quectel's `-20 + 0.2·v` mapping. That is the EC25-family definition; the repo's EC200U fixture (`41` → -12 dB) has not been checked against a modem display. Compare with a known-good reading in step 1.
- **Call details on CS lines.** The card pool knows a call is up on a slot but not who is on it, so a CS card shows "Calls now: 1" and the Calls page says "details unavailable". Say so if that is not enough.
- **Outbound rows.** A VoWiFi/VoLTE outbound call that is answered is recorded by the line's IMS agent (with quality); the SIP-side process records only the requests that never became a call. Check in step 5 that an answered call has exactly one row.
