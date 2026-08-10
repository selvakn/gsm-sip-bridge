# Contract: Sidecar configuration surface

The `internet` sidecar is configured **only** through environment variables
(FR-011), set on its service in `.env` / the override file. It never reads the
bridge's `config.toml`, and the bridge never reads these.

## Environment variables

See [data-model.md](../data-model.md#entity-sidecar-configuration-environment-variables)
for the full table. Contractual guarantees:

- `INTERNET_APN` (**required**) — empty/unset ⇒ entrypoint exits nonzero with a
  message naming the variable. No default internet APN is ever dialed.
- `INTERNET_QMI_DEV` (default `/dev/cdc-wdm0`) — must resolve to a QMI node. A
  missing or non-QMI device ⇒ fail fast with an actionable error (FR-010); the
  sidecar MUST NOT open the AT port as a fallback.
- `INTERNET_PROBE_HOST` (default `one.one.one.one`) and
  `INTERNET_PROBE_RESOLVER` (default `1.1.1.1`) — define the readiness probe.
- `INTERNET_ATTACH_GRACE` (default `90s`) and `INTERNET_PROBE_INTERVAL`
  (default `10s`) — map directly onto the healthcheck's `start_period` /
  `interval`.

## Behavioral contract

1. On start: validate config → set `qmi_wwan` raw-ip → dial `INTERNET_APN` over
   `INTERNET_QMI_DEV` → `udhcpc` on the wwan iface → install default route.
2. Continuously: run the readiness probe every `INTERNET_PROBE_INTERVAL`; update
   `/run/internet-status`; on link/session/probe loss, re-dial (self-heal).
3. The sidecar MUST use only the QMI device for data; the AT port is off-limits.
4. Observability is sidecar-local only: logs + `/run/internet-status`. No
   Prometheus, no Discord.

## Stability

These variable names + defaults are the public contract for operators. Changing a
name or a default is a documented, breaking change to the override/`.env` and
MUST be reflected in `docs/` and `.env.example`.
