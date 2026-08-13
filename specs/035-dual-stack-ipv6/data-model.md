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

## Entity: IPv6 session identity (new)

| Field | Holder | Meaning |
|-------|--------|---------|
| `V6_PKT_HANDLE` | entrypoint var | v6 packet-data handle — set for the `dual-session` case (empty when the session was adopted from autoconnect, or absent) |
| `V6_WDS_CID` | entrypoint var | retained v6 WDS client id (same discipline as `WDS_CID`); empty for adopted/absent |
| `V6_MODE` | entrypoint var | `adopted` \| `dual-session` \| `none`. The sidecar always dials a **separate `ip-type=6` session** (`dual-session`) alongside v4; `adopted` means a v6 session the modem already had up (autoconnect) that we use read-only. It never issues a combined `ip-type=8` start. |

**Rules**:
- The v6 identity follows the exact retained-CID discipline as v4 (never clear a
  CID after a *failed* stop; a vanished/unreachable modem drops it). It exists only
  in the `dual-session` mode.
- v6 teardown/redial MUST NOT touch `PKT_HANDLE`/`WDS_CID` or the v4 address.

## Entity: IPv6 address state (new)

| Field | Holder | Meaning |
|-------|--------|---------|
| `V6_ADDR` | entrypoint var | current **global** IPv6 address applied to the iface (empty = none) |
| `V6_PREFIX` | entrypoint var | prefix length granted with the address (e.g. `64`) |
| `V6_GW` | entrypoint var | granted v6 gateway (may be empty → on-link default route) |
| `V6_SINCE` | entrypoint var | UTC timestamp of the last v6 up transition |
| *(marker file)* | `${INTERNET_STATUS_FILE}.v6notified` | the last global address a hook run **succeeded** for; de-dupe gates on this so a failed hook is retried and de-dupe survives a restart |
| `V6_NEXT_RETRY` | entrypoint var | monotonic deadline before which no v6 re-establish is attempted (capped-backoff gate) |
| `V6_RETRY_INTERVAL` | entrypoint var | current backoff interval; starts at `INTERNET_PROBE_INTERVAL`, doubles up to `INTERNET_IPV6_RETRY_MAX`, resets to the floor once v6 is up |

**State transitions** (per supervise iteration, after the unchanged v4 logic):

```
        no global v6            global v6 applied
   ┌────────────────────┐   ┌───────────────────────┐
   ▼                    │   ▼                       │
[unavailable] ──apply success (is_global_v6)──▶ [up]
   ▲   │                                          │  │
   │   └── attempt (bounded, may not touch v4) ───┘  │
   │                                                 │
   └────── address dropped / redial yields none ─────┘   (flush stale global v6)
```

- Entering **up** or changing `V6_ADDR` to a different global value ⇒ fire the hook
  (if configured; unless the success marker already records this address); reset
  `V6_RETRY_INTERVAL` to the floor.
- Staying **up** across a supervise tick ⇒ re-attempt the hook only if the success
  marker is stale (i.e. the previous hook run failed) — this is the retry.
- Re-observing the same global `V6_ADDR` ⇒ no hook, no status churn.
- Leaving **up** (address dropped) ⇒ flush the stale global v6, set
  `ipv6_state=unavailable`, log it, and **do not** fire the hook (Clarification
  2026-08-13); schedule the next re-establish per the capped backoff.
- While **unavailable** ⇒ attempt a v6 re-establish only once `V6_NEXT_RETRY` has
  passed, then set `V6_NEXT_RETRY = now + V6_RETRY_INTERVAL` and double
  `V6_RETRY_INTERVAL` up to `INTERNET_IPV6_RETRY_MAX`.
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
