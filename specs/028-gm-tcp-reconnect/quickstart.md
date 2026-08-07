# Quickstart: Exercising Gm Connection Liveness

**Feature**: `028-gm-tcp-reconnect`

## Build & verify

```bash
make format && make lint && make test        # the mandatory pre-commit trio
```

## Run the feature's own tests

```bash
cargo test --test test_gm_connection_liveness      # detection, reconnect, escalation
cargo test --test test_vowifi_health_metrics       # the new gauge
cargo test --test test_ingest_critical_alerts      # failure/recovery pairing
cargo test --test test_config                      # alerts.gm_connection_lost
cargo test -p gsm-sip-bridge ims::agent::tests     # PingVerdict unit tests
```

## Synthetic reproduction (no carrier, no hardware)

The detection path is exercisable against a real `TcpListener` standing in for
the P-CSCF — no mock transport, per Constitution Principle I.

1. Bind a listener, accept one connection, answer the first `OPTIONS` with
   `200 OK` (echoing the request's `CSeq` and `Via` branch).
2. Assert the connection is scored alive.
3. Drop the accepted stream **without** a graceful close — the blackholed case.
4. Advance past `PING_INTERVAL + PING_RESPONSE_TIMEOUT` (the verdict function
   takes `now`, so tests pass a synthetic `Instant`; no sleeping).
5. Assert the verdict is `Dead` and that a reconnect is attempted.

For the listener half: `spawn_gm_server` on a real port, force the accept
loop's fatal path, assert `is_alive()` flips to `false`.

## Live check on a running deployment

```bash
# per-line connection health, alongside registration and tunnel state
vowifi-status
volte-status

# the gauge — 1 healthy, 0 reconnecting or failed, absent means never reported
curl -s localhost:<metrics-port>/metrics | grep gm_connection_up
```

Healthy line:

```
    gm_connection: up
    can_answer: true
```

Mid-reconnect:

```
    gm_connection: reconnecting since 2026-08-07T10:14:03Z (attempt 2)
    can_answer: false
    blocked_reason: the carrier signaling connection is down
```

## Live fault injection (privileged container)

Per the `sandbox-blocks-root-network-testing` note, anything needing
`CAP_NET_ADMIN` runs inside the privileged container, not the session shell.

Kill the Gm client connection out from under a registered line without a
graceful close, then watch:

```bash
# inside the privileged container, in the line's netns
conntrack -D -p tcp --dport <pcscf-port>     # or drop the flow with nft/iptables
```

Expected, within ~130s:

1. `tracing` warns that the liveness probe went unanswered.
2. `gm_connection` flips to `reconnecting`, the gauge to `0`.
3. `reconnect_transport` runs; a confirming ping round-trips.
4. `gm_connection` returns to `up`, the gauge to `1`.
5. **No Discord alert** — the episode resolved well inside the 300s threshold.

To exercise escalation and alerting instead, leave the flow blocked: three
failed reconnects escalate to a forced re-registration, and at 300s the
`gm_connection_lost` alert fires. Unblocking produces the paired recovery
notice.

## What this cannot confirm

SC-010. The original incident was a live carrier silently resetting an idle Gm
connection some minutes after registration; it was never reproduced
synthetically. The tests above bound the logic, but only a hardware re-run of
the specs/025 T072 pass-1 scenario — a line up for some minutes post-
registration on real Airtel/Vodafone VoWiFi — confirms the fix works against
the thing that actually broke.

## Rollback

The feature is additive and self-contained. To disable alerting without
reverting code:

```toml
[alerts.gm_connection_lost]
enabled = false
```

The probe, reconnect, and status/metrics reporting have no config switch by
design (the probe interval is a constant — see plan.md's Complexity Tracking).
Backing out the mechanism means reverting the commits.
