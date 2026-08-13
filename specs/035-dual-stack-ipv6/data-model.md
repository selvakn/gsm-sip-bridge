# Phase 1 Data Model: Dual-Stack IPv6 for the Cellular-Internet Sidecar

The sidecar is a shell process, so "entities" here are in-memory shell variables
and status-file fields, not a database schema. This documents the state each piece
of logic owns and how it transitions.

## Entity: IPv4 session identity (existing — unchanged)

| Field | Holder | Meaning |
|-------|--------|---------|
| `PKT_HANDLE` | entrypoint var | v4 packet-data handle from `--wds-start-network` |
| `WDS_CID` | entrypoint var | retained v4 WDS client id (targets every later v4 action) |
| `ADOPTED` | entrypoint var | 1 when the v4 session is the modem's autoconnect (not ours to stop) |

Unchanged by this feature. IPv4 remains the health-gating uplink.

## Entity: the bearer (single IPv4v6 session)

There is **no separate v6 session identity**. Dual-stack is one IPv4v6 bearer, so v6
rides the same `WDS_CID`/`PKT_HANDLE` as v4 (see research.md R1: a second session to
the same APN is refused by Jio). The only v6-specific "identity" work is provisioning
the profile:

| Field | Holder | Meaning |
|-------|--------|---------|
| `INTERNET_IPV6_PROFILE` | env (config, default `1`) | 3GPP profile index provisioned as `pdp-type=IPv4v6` (with the APN) and dialed by `profile-index` when v6 is enabled |

**Rules**:
- When v6 is enabled, `dial()` provisions the profile IPv4v6 and starts ONE bearer
  by `profile-index`; when disabled it dials `ip-type=4` (byte-identical to pre-035).
- Tearing down the single bearer (`teardown`) drops both families; there is no
  separate v6 stop. `teardown` also flushes the host-side v6 address/route.
- v6 apply/read uses `qmi_wds` (the shared client), never a separate client.

## Entity: IPv6 address state (new)

| Field | Holder | Meaning |
|-------|--------|---------|
| `V6_ADDR` | entrypoint var | current **global** IPv6 address applied to the iface (empty = none) |
| `V6_PREFIX` | entrypoint var | prefix length granted with the address (e.g. `64`) |
| `V6_SINCE` | entrypoint var | UTC timestamp of the last v6 up transition |
| *(marker file)* | `${INTERNET_STATUS_FILE}.v6notified` | the last global address a hook run **succeeded** for; de-dupe gates on this so a failed hook is retried and de-dupe survives a restart |

There is **no backoff state**. v6 rides the v4 bearer, so `refresh_v6` on each
supervise tick is just a settings read (cheap) — there is no separate v6 dial to
rate-limit, hence no `V6_NEXT_RETRY`/`V6_RETRY_INTERVAL`.

**State transitions** (per supervise iteration, after the unchanged v4 logic):

Each transition is driven by `refresh_v6` re-reading the current bearer's settings
(`--wds-get-current-settings`) on every supervise tick — there is no separate v6
dial:

- Bearer carries a global v6 that is **new/changed** ⇒ apply it, set
  `ipv6_state=up`, and fire the hook (unless the success marker already records this
  address).
- Bearer carries the **same** global v6 ⇒ no status churn; re-attempt the hook only
  if the success marker is stale (a prior hook run failed) — this is the retry.
- Bearer **stops carrying v6** (was up) ⇒ flush the stale global v6 address/route,
  set `ipv6_state=unavailable`, log it, and **do not** fire the hook (Clarification
  2026-08-13).
- Bearer carries **no v6** (was already down) ⇒ no-op (no status churn).
- A non-global address (fe80::/10, ::1, fc00::/7) ⇒ treated as **unavailable** for
  reach-back purposes (FR-012).

## Entity: Sidecar status record (existing file, extended)

File: `/run/internet-status` (`INTERNET_STATUS_FILE`). Existing fields:
`state`, `iface`, `ipv4`, `probe`, `since`, `last_change`. **Added**:

| Field | Values | Meaning |
|-------|--------|---------|
| `ipv6` | global addr or empty | current global IPv6 address (reach-back address) |
| `ipv6_prefix` | integer or empty | prefix length of `ipv6` |
| `ipv6_state` | `up` \| `unavailable` | whether a global v6 address is currently applied |
| `ipv6_since` | RFC3339 or empty | when the current v6 address came up |

**Rules**:
- `write_status` merges over prior values (as today) so a partial writer preserves
  the rest; the v6 fields are preserved/overridden the same way `ipv4`/`iface` are.
- Health/`state` is **derived from IPv4 only**; `ipv6_state` is informational and
  never changes `state` or the healthcheck exit code (FR-004/FR-007).

## Entity: Address-change hook (new, config + runtime)

| Field | Holder | Meaning |
|-------|--------|---------|
| `INTERNET_IPV6_HOOK` | env (config) | path to an executable; empty/unset = feature off |
| `INTERNET_IPV6_HOOK_TIMEOUT` | env (config, default `10s`) | max wall time for a hook invocation |
| *(success marker)* | `${INTERNET_STATUS_FILE}.v6notified` | address a hook run **succeeded** for; de-dupe key so the hook fires once per distinct address AND a failed run is retried |

**Invocation contract** (see contracts/sidecar-config.md):
`"$INTERNET_IPV6_HOOK" "$V6_ADDR"` — single argument, the new global address —
run **backgrounded** and wrapped in `timeout`, so failure/hang is isolated from the
supervise loop (FR-009). The background subshell writes the address to the success
marker **only on exit 0**, so a failed hook leaves the marker stale and the next
supervise tick retries it (a transient DDNS outage does not strand the record).

## Validation rules (derived from FRs)

- **FR-003/FR-004**: no field or transition here may alter v4 identity, v4 address,
  or `state`. Enforced by keeping v6 code in a separate function that receives only
  the iface name and touches only `V6_*` vars + `ip -6`.
- **FR-011**: with no v6 grant and no `INTERNET_IPV6_HOOK`, every `V6_*` stays empty
  and the status file's v6 fields are empty — byte-compatible with today's behavior
  for consumers that ignore unknown lines.
- **FR-012**: `V6_ADDR` only ever holds a global address; `is_global_v6()` gates
  assignment.
