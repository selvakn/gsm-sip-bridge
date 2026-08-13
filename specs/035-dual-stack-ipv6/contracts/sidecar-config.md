# Contract: Sidecar configuration (feature 035 additions)

Extends the feature-032 sidecar config. All existing variables keep their meaning
and defaults. New variables are **optional** and default to today's IPv4-only
behavior (FR-011).

## Existing variables (unchanged)

| Variable | Default | Meaning |
|----------|---------|---------|
| `INTERNET_APN` | *(required)* | Carrier INTERNET APN (not IMS). |
| `INTERNET_QMI_DEV` | `/dev/cdc-wdm0` | QMI control node. |
| `INTERNET_WWAN_IFACE` | *(auto-detect)* | Pin the wwan netdev. |
| `INTERNET_PROBE_INTERVAL` | `10s` | Supervise/probe interval. |
| `INTERNET_PROBE_HOST` | `one.one.one.one` | DNS probe target (IPv4 health). |
| `INTERNET_PROBE_RESOLVER` | `1.1.1.1` | Resolver for the probe (empty = system). |

## New variables (feature 035)

| Variable | Default | Meaning |
|----------|---------|---------|
| `INTERNET_ENABLE_IPV6` | `1` (on) | Master switch for dual-stack. `0` = force today's IPv4-only behavior. Present so a deployment can hard-disable v6 without removing config. |
| `INTERNET_IPV6_PROFILE` | `1` | 3GPP profile index the sidecar provisions as `pdp-type=IPv4v6` (with the APN) and dials by `profile-index` for dual-stack. Dual-stack is a SINGLE IPv4v6 bearer (a second session to the same APN is refused), so v6 comes from the profile, not a separate session. |
| `INTERNET_IPV6_HOOK` | *(unset)* | Path to an executable invoked when the global IPv6 address first appears or changes. Unset/empty = no hook (address still recorded in status/logs). |
| `INTERNET_IPV6_HOOK_TIMEOUT` | `10s` | Max wall-clock time a single hook invocation may run before it is killed. Bounds a hanging hook so it cannot accumulate. |

> Note: there is no v6 re-establish backoff knob. v6 rides the single v4 bearer, so
> "retrying v6" is just re-reading the bearer's settings each probe interval (a cheap
> query) — there is no separate v6 dial to rate-limit.

### `INTERNET_ENABLE_IPV6` behavior

- `1` (default): request a dual-stack session; bring up a global IPv6 address +
  default route when the carrier grants one. IPv6 is best-effort (never gates
  health, never blocks the bridge).
- `0`: dial `ip-type=4` exactly as feature 032; never touch `ip -6`; v6 status
  fields stay empty; hook never fires.

## Hook calling convention (FR-008 / FR-009)

When configured and the current global IPv6 address differs from the last one the
hook was notified about, the sidecar invokes:

```sh
"$INTERNET_IPV6_HOOK" "<new-global-ipv6-address>"
```

- **Argument 1**: the new global IPv6 address, **without** prefix length
  (e.g. `2401:4900:1c30:abcd::1`). The prefix is available in the status file if the
  hook wants it.
- **Environment**: the hook inherits the sidecar's environment (so `INTERNET_*` and
  the status-file path are visible); no additional variables are guaranteed.
- **Execution**: run **backgrounded** and wrapped in `timeout
  $INTERNET_IPV6_HOOK_TIMEOUT`. The sidecar does **not** wait for it, does not read
  its stdout, and does not act on its exit code beyond logging.
- **Fires**: once when a global address first appears, and once each time it changes
  to a different global address. Never when the address is unchanged. Never for a
  non-global (link-local/ULA) address. **Never on loss** — when IPv6 drops, the
  status flips to `ipv6_state=unavailable` and it is logged, but the hook is not
  invoked; consumers expire stale records via TTL.
- **Failure isolation**: a missing/non-executable/crashing/slow hook MUST NOT affect
  the IPv4 or IPv6 sessions, the supervise loop timing, or the healthcheck. A
  missing/non-executable path is logged once as a warning.
- **Retry on failure**: de-dupe gates on a success marker
  (`${INTERNET_STATUS_FILE}.v6notified`) that is written only when the hook exits 0.
  A hook that exits nonzero (e.g. the DDNS endpoint is briefly down right after the
  link came up) leaves the marker stale, so the sidecar retries it on the next probe
  tick until it succeeds — a transient failure never strands the reachable address.

### Example hook (operator-supplied DDNS updater)

```sh
#!/usr/bin/env sh
# $1 = new global IPv6 address. Push it to your DNS provider.
new_addr="$1"
curl -fsS -X PATCH "https://api.example-dns.tld/records/host-aaaa" \
     -H "Authorization: Bearer $DDNS_TOKEN" \
     -d "{\"type\":\"AAAA\",\"content\":\"$new_addr\"}" >/dev/null
```

The sidecar ships no DDNS client and holds no DNS credentials; the hook is the
operator's integration point.

## Invariants

- `INTERNET_APN` remains the only required variable.
- With none of the new variables set beyond defaults, behavior on an IPv6-capable
  carrier gains a global v6 address (best-effort) and, on an IPv6-incapable
  carrier, is identical to feature 032.
- The AT port is never opened; all modem interaction is QMI (`qmicli`).
