# Contract: Web UI HTTP interface

Served on `[webui].listen_addr:[webui].port` (default `0.0.0.0:8080`), only when `[webui].enabled = true`. This listener is fully separate from `[metrics].port`, and `/metrics` is never served here.

## Common rules

- **Session**: every route except `GET /login`, `POST /api/login` and the static assets needed by the login page requires a valid `gsb_session` cookie.
  - An unauthenticated `GET` of an HTML route gets `303 See Other` → `/login`.
  - An unauthenticated `/api/*` request gets `401` with `{"error":"unauthenticated"}`. No other data is ever included (SC-005).
- **Response headers on every response**:
  - `Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; frame-ancestors 'none'`
  - `X-Content-Type-Options: nosniff`
  - `Referrer-Policy: no-referrer`
  - `Cache-Control: no-store` on `/api/*`; `no-cache` plus an ETag on assets
- **Methods**: only `GET`, plus `POST` for login and logout. Anything else gets `405`.
- **Format**: JSON is UTF-8. Times are RFC 3339 UTC strings or unix seconds where the field name says `_unix`. Absent or unknown values are `null`, never `0` (FR-023, FR-032).
- **Paging**: `?page=N` (from 1) and `?per_page=M` (default 50, maximum 200; larger values are clamped). Every list response has the shape `{ "items": [...], "page": N, "per_page": M, "total": T }`.
- **Database unavailable**: `503` with `{"error":"store_unavailable"}`. `/api/overview` doesn't depend on the store.

## Routes

### `GET /` , `GET /login`

Both serve `index.html` (single-page app with hash routes: `#/overview`, `#/calls`, `#/sms`, `#/events`, `#/config`). The `/login` route renders the login form.

### `GET /assets/app.js`, `GET /assets/style.css`

Embedded static files (`include_str!`).

### `POST /api/login`

Request body: `{"password": "..."}`.

| Result | Response |
|---|---|
| Correct password | `204`, plus `Set-Cookie: gsb_session=<token>; HttpOnly; SameSite=Strict; Path=/; Max-Age=604800` |
| Wrong password | `401` with `{"error":"invalid_credentials"}`; the client IP's failure count is incremented |
| Client IP locked out | `429` with `{"error":"too_many_attempts","retry_after_seconds":N}` and a `Retry-After` header |

An IP is locked out for 300s after 5 consecutive failures.

### `POST /api/logout`

Removes the session and returns `204` with `Set-Cookie: gsb_session=; Max-Age=0; Path=/`.

### `GET /api/overview`

```json
{
  "bridge": {
    "version": "1.42.0",
    "git_sha": "343c2f2",
    "uptime_seconds": 86400,
    "pbx_registered": true,
    "grafana_url": "https://grafana.example/d/abc"
  },
  "active_alerts": [
    {"category": "registration_loss", "module_id": "card-1a2b", "since": "2026-10-24T03:01:12Z"}
  ],
  "lines": [ /* LineView, see data-model.md */ ],
  "discovery_note": null
}
```

- `pbx_registered` is `null` when no PBX is configured.
- `grafana_url` is `null` when unset.
- `active_alerts` is built from open raises (open `alert_raised:*` events with no resolution in the in-memory map) plus `CRITICAL_EVENT_ACTIVE == 1`.
- `discovery_note` is `"no_lines"` when there are no lines at all (spec edge case), otherwise `null`.

One `LineView` item:

```json
{
  "id": "card-1a2b", "index": 1, "transport": "vowifi",
  "carrier": "405-857", "msisdn": "+919000000000",
  "registration_state": "registered",
  "registered_at": "2026-10-24T02:00:00Z", "expires_at": "2026-10-24T03:00:00Z",
  "tunnel_up": true, "child_sa_up": true, "gm_connection_up": true, "uplink": "up",
  "pdn_up": null,
  "active_calls": [{"remote": "+919000000001", "direction": "inbound",
                    "started_at_unix": 1793000000, "answered": true,
                    "loss_percent": 0.4, "jitter_ms": 12.0, "rtt_ms": null}],
  "signal": {"at_unix": 1793000000, "rssi_dbm": -71, "rat": "LTE",
             "rsrp_dbm": -98, "rsrq_db": -11, "sinr_db": 9},
  "signal_note": null,
  "last_report_age_seconds": 4, "stale": false,
  "health": "ok", "health_reasons": []
}
```

Fields that don't apply to a transport are `null`. For example, a CS line has `tunnel_up: null`, and a VoWiFi line has `pdn_up: null`.

### `GET /api/calls/summary`

```json
{
  "today":  {"answered": 3, "missed": 1, "failed": 0, "avg_answered_seconds": 142.5},
  "last_7d": {"answered": 20, "missed": 4, "failed": 2, "avg_answered_seconds": 131.0},
  "by_line": [{"module_id": "card-1a2b", "today": {}, "last_7d": {}}],
  "by_direction": {"inbound": {"today": {}, "last_7d": {}}, "outbound": {"today": {}, "last_7d": {}}}
}
```

The empty `{}` objects in the example have the same shape as `today`. "Today" is the bridge host's local midnight to now. The client also shows the timezone.

### `GET /api/calls`

Query parameters: `line`, `direction` (`inbound`|`outbound`), `transport`, `status`, `from` and `to` (RFC 3339), plus paging. Results are newest first.

Each item: `{id, module_id, direction, remote, started_at, duration_seconds, status, transport, loss_percent, jitter_ms, rtt_ms}`.

### `GET /api/sms`

Query parameters: `line`, `q` (substring of sender or body, bound as a parameter), plus paging. Results are newest first.

Each item: `{id, module_id, sender, body, received_at, forwarding_status, forwarded_at, transport}`.

### `GET /api/events`

Query parameters: `line`, `kind` (exact, or a prefix like `alert_raised:`), plus paging. Results are newest first.

Each item: `{id, at, module_id, kind, severity, detail, raise_id, duration_seconds}`.

### `GET /api/health`

```json
{"modules_active": 2, "modules_failed": 0, "store_queue_depth": 0,
 "observability_events_dropped": 0, "cs_enabled": true}
```

### `GET /api/config`

```json
{"loaded_at": "2026-10-24T00:00:00Z", "source": "/etc/gsm-sip-bridge/config.toml",
 "note": "As loaded at startup; later edits to the file are not shown.",
 "document": { /* the TOML document as JSON, env: refs shown as written, secrets "[REDACTED]" */ }}
```

## Not provided (FR-006)

There are no routes that write, send, dial, restart or change state, apart from login and logout.
