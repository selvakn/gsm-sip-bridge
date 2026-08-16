# Quickstart: siptest

## 1. Give siptest its own bridge account

**Do not reuse `1001`.** The registrar keeps **one binding per account**
(`HashMap<String, Binding>`, `upsert` replaces), so registering under an
account a physical handset already holds silently evicts that handset and
steals its inbound calls.

Add to the bridge's `config.toml`:

```toml
[[sip_server.account]]
username = "1002"
password = "env:PHONE_1002_PASSWORD"
```

Set `PHONE_1002_PASSWORD` in the bridge's environment and restart it.

- **Outbound testing works immediately.** Any live binding may dial out, so
  `ring_aor` is irrelevant to it.
- **Inbound testing additionally needs `ring_aor = "1002"`** — exactly one
  account rings, and it is the one `ring_aor` names. That is another config
  change plus a restart, and it takes the real handset out of service for the
  duration. Inbound runs should be deliberate, not left running.

Confirm the bridge is up and note the ports actually in use:

```bash
ss -lunp | grep -E '5060|5062|5072|5073'
curl -s localhost:9091/metrics | grep -E 'sip_server_bindings|ring_aor_registered'
```

## 2. Configure siptest

`siptest.toml`:

```toml
[sip]
bridge_host    = "192.168.15.10"
registrar_port = 5060
outbound_port  = 5072     # EXPECTED only; a mismatch logs a warning, never gates.
                          # The real target always comes from the 302's Contact.
local_port     = 5065     # 5060/5062/5072/5073/5074 are taken
# local_ip     = "192.168.15.10"   # auto-detected; set only to override
username = "1002"
password = "env:PHONE_1002_PASSWORD"
realm    = "gsm-sip-bridge"
register_expires_secs = 300

[media]
codec         = "auto"
rtp_port_min  = 40000
rtp_port_max  = 40100
tone_plan     = "grid8"
recording_dir = "/tmp/siptest"
record        = true

[call]
default_duration_secs = 30
ring_timeout_secs     = 40
require               = "packets"   # signalling | packets | tone-loopback

[safety]
# REQUIRED. An empty or absent list denies every outbound call — the guard
# fails closed, so a typo or a looping agent cannot reach a stranger's phone.
allowed_destinations   = ["+919000000000"]   # exact, or trailing-* prefix
min_call_interval_secs = 10
max_calls_per_hour     = 20

[retention]
max_calls_retained = 50   # oldest call's WAVs + report are deleted past this

[inbound]
mode = "answer"
answer_delay_ms = 2000

[api]
bind = "127.0.0.1:8099"
```

```bash
export PHONE_1002_PASSWORD='...'
```

## 3. Build and start

No pjsip is involved, so this runs on the host directly — no `pjsip-linked`
feature, no Docker.

```bash
make build
make siptest                      # or: cargo run -p siptest -- --config siptest.toml
```

Verify registration from both sides:

```bash
curl -s localhost:8099/status | jq '.registration, .local'
curl -s localhost:9091/metrics | grep sip_server_bindings   # should now read 2
```

**If inbound never arrives, check `local.sip_addr` first.** A `Contact` of
`0.0.0.0:5065` is the most likely first-run failure and is otherwise invisible.

## 4. Place a call

```bash
curl -sXPOST localhost:8099/calls -H 'Content-Type: application/json' \
  -d '{"destination":"+919000000000","duration_secs":20}' | jq -r .id
```

The handset rings; answer it and speak. Then:

```bash
curl -s localhost:8099/calls/c-1 | jq '.report.verdicts, .report.media.rx_level, .report.success'
curl -s localhost:8099/calls/c-1 | jq -r .report_text
```

Or in one shot, which is usually what you want:

```bash
siptest call --destination +919000000000 --wait ; echo "exit=$?"
```

Exit `0` only when the call met `require`. **An answered-but-silent call is a
failure**, matching `volte-call`'s rule.

Reading the verdicts — the three axes are independent by design:

| `verdicts.packets` | `verdicts.rx_audio` | Means |
|---|---|---|
| `both_ways` | `tone_detected` | Working end to end |
| `both_ways` | `noise_only` | Media path up, but our audio is not surviving it — transcoding, VAD or a gain stage |
| `send_only` | `silent` | Classic one-way audio: nothing is coming back |
| `neither` | `silent` | Media never established — check `answer_to_first_rtp_ms` |

## 5. Receive a call

Point the bridge at siptest (`ring_aor = "1002"`, restart), then ring the
bridge's number. Discover it by polling — no log scraping:

```bash
curl -s 'localhost:8099/events?since=0' | jq '.[] | select(.kind=="incoming_call")'
```

The canonical agent loop:

```bash
B=localhost:8099; n=0
while :; do
  batch=$(curl -s "$B/events?since=$n&timeout_ms=25000")
  [ "$(jq length <<<"$batch")" -gt 0 ] || continue
  jq -c '.[]' <<<"$batch"
  n=$(jq '.[-1].seq' <<<"$batch")
done
```

For manual control:

```bash
curl -sXPUT  localhost:8099/policy/inbound -H 'Content-Type: application/json' -d '{"mode":"manual"}'
curl -sXPOST localhost:8099/calls/c-2/answer
curl -sXPOST localhost:8099/calls/c-2/reject -H 'Content-Type: application/json' -d '{"status":486}'
```

## 6. Recordings

```bash
curl -s localhost:8099/calls/c-1/recording | jq
ls /tmp/siptest/          # c-1-received.wav, c-1-sent.wav, c-1.json
```

Two files per call, one per direction, at the negotiated audio rate — 16000 for
G.722, 8000 for PCMU. `sent.wav` is written pre-encode (what we intended),
`received.wav` post-decode (what actually arrived, distortion included). The
JSON sidecar holds the full report, so it can be read without HTTP.

## 7. When something is wrong

```bash
curl -s 'localhost:8099/log/tail?lines=200' | jq -r '.lines[]'
```

| Symptom | Cause |
|---|---|
| `Expected request with Content-Type: application/json` | A `-d` body was posted without `-H 'Content-Type: application/json'` — axum's JSON extractor rejects it outright rather than guessing. Every `POST`/`PUT` example above that sends a body includes the header for this reason |
| `403` on an outbound call | Registration lapsed, or the INVITE left from a different local port than the REGISTER. The bridge matches the full `SocketAddr`, so this looks like an auth fault but is not |
| Inbound never arrives | `ring_aor` does not name siptest's account; or `local.sip_addr` is unroutable; or the real handset re-registered and displaced the binding |
| `484` | Destination contains characters outside `[0-9*#+]` |
| `503` | No idle line on the bridge — check `vowifi-status` |
| Redirect port warning | The bridge is running a different subsystem than `outbound_port` expects. Harmless; the real target is taken from the `302` |
| Answered, but `packets: neither` | Media never established — check `answer_to_first_rtp_ms` and whether the SDP answer's `c=` is reachable |
| `403 destination_not_allowed` | siptest's own allow-list, not the bridge. Add the number to `[safety].allowed_destinations`. An empty list denies everything by design |
| `429 rate_limited` | siptest's own rate limit. `retry_after_s` says when; raise `[safety].max_calls_per_hour` if the cap is genuinely too tight |
| `410 call_evicted` | The call aged out past `[retention].max_calls_retained` and its recordings were deleted. Distinct from `404`, which means the id never existed |

## 8. Tests

Everything runs with no modem, no SIM, no carrier and no running bridge — the
integration tests start the bridge's real registrar in-process:

```bash
make test
cargo nextest run -p siptest
```

Before any commit, per `CLAUDE.md`:

```bash
make format && make lint && make test
```
