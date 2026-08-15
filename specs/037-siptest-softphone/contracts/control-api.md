# Contract: siptest control API

HTTP/JSON over `127.0.0.1:8099` by default. **Never bind a public address** —
this interface places real phone calls.

The consumer is a coding agent using `curl` and `jq`. Every response is JSON,
every call has a stable id, and asynchronous occurrences are discoverable by
polling alone (FR-029). There is no authentication: the loopback bind is the
security boundary.

## Endpoints

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/health` | Liveness |
| `GET` | `/status` | Full snapshot |
| `POST` | `/calls` | Place an outbound call |
| `GET` | `/calls?limit=N` | Recent call summaries, newest first |
| `GET` | `/calls/{id}` | Full call report |
| `POST` | `/calls/{id}/answer` | Answer a ringing inbound call (manual policy) |
| `POST` | `/calls/{id}/reject` | Reject a ringing inbound call |
| `POST` | `/calls/{id}/hangup` | Terminate an active call |
| `GET` | `/calls/{id}/recording` | Recording paths and rate |
| `GET` | `/calls/{id}/recording/{received,sent}.wav` | Audio bytes |
| `GET` | `/policy/inbound` | Current inbound policy |
| `PUT` | `/policy/inbound` | Change inbound policy at runtime |
| `POST` | `/registration/{register,refresh,deregister}` | Force a registration action |
| `GET` | `/events?since=N&timeout_ms=M` | Ordered event log (long-poll) |
| `GET` | `/log/tail?lines=N` | Recent log lines |

## `GET /status`

```json
{
  "registration": {
    "state": "registered",
    "aor": "sip:1002@gsm-sip-bridge",
    "contact": "sip:1002@192.168.15.10:5065",
    "expires_at": "2026-08-15T10:05:00Z",
    "renews_in_secs": 137,
    "last_response": { "status": 200, "reason": "OK" },
    "consecutive_failures": 0
  },
  "local":  { "sip_addr": "192.168.15.10:5065", "rtp_range": [40000, 40100] },
  "bridge": { "registrar": "192.168.15.10:5060", "outbound_observed": "192.168.15.10:5072" },
  "active_call": {
    "id": "c-7", "direction": "inbound", "state": "ringing",
    "peer": "+919000000000", "since": "2026-08-15T10:02:41Z"
  },
  "inbound_policy": { "mode": "manual", "answer_delay_ms": 2000 },
  "counters": { "calls_placed": 3, "calls_received": 1, "registrations": 12, "errors": 0 },
  "event_seq": 412
}
```

`registration.state` ∈ `unregistered | challenged | registered | refreshing | failed`.
`active_call` is `null` when idle.

`local.sip_addr` exists specifically so an unroutable advertised contact — the
most likely first-run failure, and otherwise silent — is visible.
`bridge.outbound_observed` reports the redirect target actually seen, never a
configured guess.

## `POST /calls`

Request:

```json
{
  "destination": "+919000000000",
  "duration_secs": 30,
  "codec": "auto",
  "tone_plan": "grid8",
  "require": "packets",
  "record": true,
  "ring_timeout_secs": 40
}
```

Only `destination` is required; everything else falls back to config. The
destination is validated client-side as `[0-9*#+]+` so an invalid one is
reported clearly rather than surfacing as a bridge `484`.

Response `202`:

```json
{ "id": "c-8", "state": "inviting", "events_since": 412 }
```

With `?wait=true`, blocks until the call reaches a terminal state and returns
the full report instead — a complete one-shot test in a single request.

Errors:

| Status | Body `error` | Cause |
|---|---|---|
| `400` | `invalid_destination` | Fails the `[0-9*#+]+` check |
| `403` | `destination_not_allowed` | Not matched by the configured allow-list |
| `429` | `rate_limited` | Below `min_call_interval_secs`, or at `max_calls_per_hour`; carries `retry_after_s` |
| `409` | `call_in_progress` | `max_concurrent` reached |
| `503` | `not_registered` | No live registration |

```json
{ "error": "rate_limited", "retry_after_s": 48,
  "detail": "20 calls in the last hour (max_calls_per_hour = 20)" }
```

`403` and `429` are enforced **before any signalling leaves the host**, so they
are always locally attributable and never confusable with the bridge's own
`403` (untrusted source) or `503` (no idle line). An empty allow-list denies
everything — the guard fails closed.

## `GET /calls/{id}`

```json
{
  "id": "c-8",
  "direction": "outbound",
  "destination": "+919000000000",
  "peer_uri": "sip:+919000000000@192.168.15.10:5072",
  "caller_id": { "from": null, "p_asserted_identity": null, "x_gsm_caller_id": null },
  "state": "ended",
  "end_reason": "duration_elapsed",
  "signalling": {
    "invite_at": "2026-08-15T10:02:00Z",
    "redirect_302": { "contact": "sip:+919000000000@192.168.15.10:5072", "port": 5072 },
    "invite_to_180_ms": 210,
    "invite_to_200_ms": 4210,
    "answer_to_first_rtp_ms": 180,
    "final_status": 200
  },
  "media": {
    "codec": "PCMU", "payload_type": 0, "rtp_clock_hz": 8000, "audio_hz": 8000,
    "local_rtp": "192.168.15.10:40002", "remote_rtp": "192.168.15.10:41984",
    "sent_packets": 1500, "sent_samples": 240000,
    "received_packets": 1494, "received_samples": 239040,
    "lost_packets": 6, "loss_percent": 0.4, "reordered_packets": 0, "jitter_ms": 2.3,
    "rx_level": {
      "peak_dbfs": -14.2, "mean_dbfs": -28.0,
      "noise_floor_dbfs": -52.1, "silent_frame_pct": 3.1
    },
    "tone": {
      "plan": "grid8", "tx_symbols_sent": 150, "rx_symbols_detected": 146,
      "rx_symbol_error_pct": 2.7, "detected": true,
      "first_detected_ms_after_answer": 842,
      "round_trip_delay_ms": { "min": 312, "median": 338, "max": 410, "samples": 140 }
    }
  },
  "verdicts": { "packets": "both_ways", "rx_audio": "tone_detected", "loopback": "confirmed" },
  "success": true,
  "recordings": {
    "received": "/tmp/siptest/c-8-received.wav",
    "sent": "/tmp/siptest/c-8-sent.wav"
  },
  "report_text": "\ncall report\n  direction      : both ways\n  ..."
}
```

The three `verdicts` axes are independent and are never collapsed into one
"audio ok" boolean — that distinction is the point of the tool.
`round_trip_delay_ms` is `null` and `loopback` is `not_confirmed` when the
signal never returned; that is **not** a failure unless `require` was
`tone-loopback`.

`report_text` carries the human-readable rendering, matching the existing
`render_call_report` format, so an agent can paste it without re-formatting.

**Retention.** Only the most recent `max_calls_retained` calls are kept; older
ones have their recordings and report deleted. A request for an evicted call
returns `410 Gone` with `{"error":"call_evicted"}`, deliberately distinct from
the `404` an unknown id returns — an agent must be able to tell "you asked too
late" from "that id never existed". The same distinction applies to
`/calls/{id}/recording*`.

## `GET /events?since=N&timeout_ms=M`

Returns every event with `seq > N`, immediately if any are buffered, otherwise
blocking up to `timeout_ms` (default 25000) and returning `[]`.

```json
[
  { "seq": 413, "at": "2026-08-15T10:02:41Z", "kind": "incoming_call",
    "call_id": "c-7",
    "caller_id": { "from": "sip:+919000000000@192.168.15.10:5060",
                   "p_asserted_identity": "sip:+919000000000@ims.mnc000.mcc000.3gppnetwork.org",
                   "x_gsm_caller_id": "+919000000000" } }
]
```

Kinds: `registration_state` · `incoming_call` · `call_state` ·
`media_first_packet` · `tone_detected` · `call_ended` · `warning` · `error`.

Sequence numbers are monotonic and gaps are detectable, so an agent knows when
it has fallen behind the ring buffer. The canonical agent loop:

```bash
n=0
while :; do
  batch=$(curl -s "$B/events?since=$n")
  [ "$(jq length <<<"$batch")" -gt 0 ] && { jq -c '.[]' <<<"$batch"; n=$(jq '.[-1].seq' <<<"$batch"); }
done
```

**No SSE.** A stream that never terminates is the wrong shape for an agent
driving `curl`, and the cursor form is replayable and resumable where a stream
is not.

## `PUT /policy/inbound`

```json
{ "mode": "manual", "answer_delay_ms": 2000, "reject_status": 486, "duration_secs": 30 }
```

Takes effect for the next inbound call; never disturbs one in progress.

## `GET /log/tail?lines=200`

```json
{ "lines": ["2026-08-15T10:02:00Z  INFO siptest::sip: registered contact=sip:1002@192.168.15.10:5065"] }
```

Present so an agent can diagnose without locating the daemon's stderr.

## Exit-code contract for the one-shot CLI

`siptest call` is an HTTP client against a running daemon — **not** a second
implementation of the call flow. It exits `0` only when `success` is `true`
under the configured `require`, so an answered-but-silent call fails (FR-032),
matching `volte-call`'s established rule. The text report goes to stdout;
all diagnostics go to stderr (FR-033).
