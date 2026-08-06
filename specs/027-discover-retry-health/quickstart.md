# Quickstart: Discovery Retry & Missing-Line Health Reporting

## Reproducing the original incident (before this feature)

1. Configure a modem-pinned VoWiFi line in `config.toml`:
   ```toml
   [[vowifi.line]]
   modem_port = "/dev/ttyUSB3"
   ```
2. Start the container (`docker compose up` / `docker run` against the existing image) at a moment
   the modem hasn't finished USB enumeration yet (e.g. right after a host reboot, or right after
   replugging the modem).
3. Observe: `docker exec <container> gsm-sip-bridge -c /etc/gsm-sip-bridge/config.toml vowifi-status`
   shows no mention of this line at all — not even as failed. `docker ps` shows the container
   `healthy`. Nothing in the container logs mentions this modem's `card_id` at any point.

## Verifying the fix once implemented

1. Same config and same slow-enumerating modem.
2. Within the bounded retry window, once the modem finishes enumerating, its line resolves and
   starts registering — no restart needed. Confirm via `vowifi-status`: the line now appears under
   its normal `Line <index> (card <card_id>):` section, registering normally.
3. To exercise the failure path instead: configure a `modem_port` pointing at a device that will
   never exist (e.g. `/dev/ttyUSB99`), start the container, and after the retry window elapses:
   - `vowifi-status` shows a `Configured line /dev/ttyUSB99 (from config.toml): NOT RUNNING` /
     `reason: not_found` entry.
   - `docker ps` / `gsm-sip-bridge healthcheck` reports unhealthy.
   - `curl localhost:9091/metrics | grep vowifi_line_discovery_failed` shows
     `gsm_sip_bridge_vowifi_line_discovery_failed{module="/dev/ttyUSB99"} 1`.
   - If `[alerts.line_discovery_failed].enabled = true`, a Discord failure notification for this
     line arrived once the window elapsed (not before, not repeated).
4. Fix the config back to a real device and restart: the line resolves normally, and — if a failure
   alert had already fired for a *previous* run's terminal failure with the same identifier — a
   recovery notification is not expected here (each container run is its own retry-window
   lifetime, per the startup-only scope clarification); the self-heal-after-alert / recovery-notice
   path is instead exercised within a single run by using a modem that appears *after* the retry
   window would otherwise have elapsed but the process is still up polling (e.g. via a
   test/integration harness rather than manual reproduction, since manually timing a real USB
   replug against a multi-minute window is impractical).

## Automated test entry points (for `/speckit-tasks` to expand on)

- `gsm-sip-bridge/tests/test_discovery.rs` — add cases for the new `Rejection::NotFound` reason and
  for retry-then-resolve not disturbing an already-resolved sibling line.
- `gsm-sip-bridge/tests/test_cli.rs` — add a case for `vowifi-status`'s new "Configured line ...
  NOT RUNNING" output section.
- `gsm-sip-bridge/tests/test_metrics_endpoint.rs` — add a case for
  `gsm_sip_bridge_vowifi_line_discovery_failed`'s presence/value.
- `gsm-sip-bridge/tests/test_ingest_critical_alerts.rs` (or a new sibling file) — add cases for the
  `line_discovery_failed` category's failure/recovery notification pairing, mirroring the existing
  `registration_loss`/`tunnel_failure` test cases in that file.
- A new or extended `healthcheck.rs` unit test module — add cases for `evaluate()` treating a
  terminal configured-line failure as unhealthy, and a still-retrying one as not-yet-unhealthy.
