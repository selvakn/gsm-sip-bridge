# Quickstart: Card Phone Number and Instance Identity in Alerts

## Configure

`config.toml`:

```toml
[alerts]
# Optional. Identifies this deployment in every alert footer.
# When unset, the host's system hostname is used.
instance_name = "bridge-01"

# VoWiFi line phone number (new) — shown in that line's alerts.
[[vowifi.line]]
modem_port = "/dev/ttyUSB2"
msisdn = "+919000000001"

# VoLTE line phone number (existing field, now also shown in alerts).
[[volte.line]]
modem_serial = "abc123"
msisdn = "+919000000001"
```

Circuit-switched cards have no config table; their number comes from the SIM
(`AT+CNUM`) automatically, and shows `unknown` when the SIM has none.

## Verify (automated)

```bash
make format
make lint
make test          # alert suite asserts the Phone field + footer instance
```

## Verify (end-to-end, hardware)

1. Set `[alerts].instance_name` and a line `msisdn` in `config.toml`; start the
   bridge.
2. Send an SMS to a card → the Discord message shows a `Phone` field and a footer
   `gsm-sip-bridge · bridge-01`.
3. Trip a critical event (e.g. pull a SIM to exhaust ModuleLifecycle recovery) →
   the alert shows the same `Phone` + footer.
4. Unset `instance_name`, restart → the footer falls back to the host hostname.
5. Trigger an alert on a card with no resolvable number → the `Phone` field shows
   `unknown` and the alert still delivers.

## Success signals (from spec)

- SC-001: every alert shows an instance name.
- SC-002: every alert for a configured line shows that number.
- SC-005: with multiple lines/hosts on one channel, each alert is attributable to
  exactly one line and one host.
