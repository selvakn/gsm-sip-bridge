# Contract: Sidecar status file (feature 035 additions)

File: `/run/internet-status` (overridable via `INTERNET_STATUS_FILE`). Written
atomically (temp + `mv`) by `write_status` in `internet-lib.sh`. Sidecar-local
observability only (FR-008 of feature 032); never a control surface.

## Schema

Existing fields (feature 032, unchanged):

```
state=up|dialing|redialing|down|probe-fail
iface=<wwan iface name>
ipv4=<assigned IPv4 address or empty>
probe=<free-text probe result>
since=<RFC3339 when the current IPv4 session came up>
last_change=<RFC3339 of this write>
```

New fields (feature 035), appended to every write:

```
ipv6=<current global IPv6 address, or empty>
ipv6_prefix=<prefix length of ipv6, or empty>
ipv6_state=up|unavailable
ipv6_since=<RFC3339 when the current global IPv6 address came up, or empty>
```

## Rules

- **Health independence (FR-004/FR-007)**: `state` is derived from the IPv4 session
  and probe **only**. `ipv6_state` is informational; it MUST NOT influence `state`,
  the healthcheck exit code, or the bridge's `depends_on: service_healthy` gate.
- **Merge semantics**: `write_status` continues to merge over prior values, so a
  writer that only knows the new v6 fields preserves `iface`/`ipv4`/`since`, and a
  writer that only knows v4 preserves the v6 fields. New optional exported overrides
  mirror the existing `STATUS_IFACE`/`STATUS_IPV4`/`STATUS_SINCE` pattern
  (e.g. `STATUS_IPV6`, `STATUS_IPV6_PREFIX`, `STATUS_IPV6_STATE`, `STATUS_IPV6_SINCE`).
- **Global-only**: `ipv6` only ever holds a global unicast address. A link-local or
  ULA address is reported as `ipv6_state=unavailable` with an empty `ipv6`.
- **Backward compatibility (FR-011)**: consumers of the 032 status file that read
  fields by name are unaffected by the appended lines; a deployment with IPv6
  disabled or ungranted shows empty `ipv6`/`ipv6_prefix`/`ipv6_since` and
  `ipv6_state=unavailable`.

## Example (dual-stack up)

```
state=up
iface=wwan0
ipv4=100.72.13.4
probe=ok one.one.one.one@1.1.1.1
since=2026-08-13T09:15:02Z
last_change=2026-08-13T09:15:12Z
ipv6=2401:4900:1c30:abcd::1
ipv6_prefix=64
ipv6_state=up
ipv6_since=2026-08-13T09:15:05Z
```

## Example (IPv4 up, IPv6 ungranted)

```
state=up
iface=wwan0
ipv4=100.72.13.4
probe=ok one.one.one.one@1.1.1.1
since=2026-08-13T09:15:02Z
last_change=2026-08-13T09:15:12Z
ipv6=
ipv6_prefix=
ipv6_state=unavailable
ipv6_since=
```
