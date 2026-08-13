# Phase 0 Research: Dual-Stack IPv6 for the Cellular-Internet Sidecar

Resolves the unknowns in the plan's Technical Context. Each item: Decision /
Rationale / Alternatives considered. Items marked **VERIFY-ON-HW** need a one-off
check against the actual EC20/EC25 + carrier before the corresponding task is
closed; the plan is written so either outcome is a small, localized change.

## R1 — How to request a dual-stack session over QMI

**Decision (as implemented)**: Keep the existing `ip-type=4` session exactly as
today and start a **separate, independent `ip-type=6` WDS session** on the same
APN, each with its own retained CID/handle pair (`V6_MODE=dual-session`). If a v6
start returns `NoEffect`, a v6 session is already up via modem autoconnect — adopt
it read-only (`V6_MODE=adopted`), holding no client for it. The sidecar does **not**
issue a combined `ip-type=8` start.

**Rationale**: libqmi's `--wds-start-network` `ip-type` takes `4`, `6`, or (on some
builds) a combined value, but Quectel EC20/EC25 modems commonly expose v4 and v6 as
**separate PDN contexts**, so the separate-session approach is the reliable lowest
common denominator. Choosing it unconditionally (rather than trying combined first)
keeps the v4 dial/teardown code path byte-identical and avoids a second code path to
maintain — simpler per Constitution Principle V, at no cost to correctness. Both v6
modes reuse the proven retained-CID discipline from `dial()`/`teardown()`.

**Alternatives considered**:
- *Combined `ip-type=8` first, fall back to separate*: rejected — it would change
  the v4 start/handle parsing and add a second dial path for a "fast" case with no
  functional benefit over two separate sessions. Dropped for simplicity.
- *v6-only session*: rejected — breaks dual-stack (VoWiFi/IPv4-only sites) and the
  health model.
- *Rely solely on modem autoconnect for v6*: rejected as the primary path —
  autoconnect behaviour varies; we start our own session and only adopt an existing
  one on `NoEffect`.

**VERIFY-ON-HW**: confirm the carrier grants a global v6 address on an `ip-type=6`
session:
`qmicli -d /dev/cdc-wdm0 -p --wds-start-network="ip-type=6,apn=$APN" --client-no-release-cid`
then `--wds-get-current-settings` and look for an `IPv6 address:` line.

## R2 — Reading the granted IPv6 settings

**Decision**: Parse `qmicli --wds-get-current-settings` for the lines
`IPv6 address:` (form `2401:4900:...:1/64` — address **with** prefix length),
`IPv6 gateway address:`, and `IPv6 primary/secondary DNS:`. Extract the address and
the prefix length from the single `addr/prefix` token. The query targets the v6
session's own client id (`qmi_wds_v6` uses `V6_WDS_CID`) so it reads the v6 grant,
not the v4 one.

**Rationale**: qmicli prints the QMI-granted IPv6 address already carrying its
prefix length, unlike IPv4 which reports a separate subnet mask. So no
mask→prefix conversion is needed for v6 (contrast `mask2prefix` for v4). This is a
parse-only addition mirroring the existing `apply_settings` IPv4 extraction.

**Alternatives considered**:
- *SLAAC / accept RA on the wwan iface*: rejected as the source of truth. QMI
  `raw_ip` interfaces do not carry Ethernet/NDP the way a LAN NIC does; the
  authoritative address is the QMI-granted one. Applying it explicitly (as we do for
  v4) is deterministic and matches the existing design. (If VERIFY-ON-HW shows the
  modem expects RA-based v6, the fallback is to enable
  `net.ipv6.conf.$IFACE.accept_ra=2` and read the resulting global address — noted,
  not chosen.)

**VERIFY-ON-HW**: exact label spelling/format of the v6 settings lines from this
libqmi version; the parser's `sed` patterns are pinned to that output (same risk the
v4 parser already lives with).

## R3 — Applying the IPv6 address and route

**Decision**: `ip -6 addr add "$V6ADDR/$V6PREFIX" dev "$IFACE"` and, if a gateway is
granted, `ip -6 route replace default via "$V6GW" dev "$IFACE"`, else
`ip -6 route replace default dev "$IFACE"`. Before applying, flush prior sidecar-added
v6 config for the iface (`ip -6 addr flush dev "$IFACE" scope global`) so a changed
prefix does not leave a stale address/route. Set
`net.ipv6.conf.$IFACE.disable_ipv6=0` defensively at bring-up.

**Rationale**: Symmetric with the IPv4 `apply_settings`/`teardown` using the same
`iproute2` verbs; `replace` is idempotent (safe on redial). Flushing only
`scope global` preserves the kernel's link-local address.

**Alternatives considered**:
- *`ip addr add` without `-6`*: unnecessary; explicit `-6` is clearer and avoids
  ambiguity in stubs/tests.
- *NetworkManager/systemd-networkd*: rejected — the Alpine sidecar is deliberately
  dependency-light and imperatively driven; adding a network manager violates
  Principle V.

## R4 — Global vs. link-local/ULA detection (FR-012)

**Decision**: Treat an address as reach-back-eligible ("global") only when it is a
global unicast address: not `fe80::/10` (link-local), not `::1`, not `fc00::/7`
(ULA). Implement a small POSIX helper `is_global_v6()` that rejects those prefixes
by string match on the normalized address. Only a global address is written to the
status `ipv6=` field and only its change fires the hook.

**Rationale**: The whole point is inbound reachability from the public internet; a
link-local or ULA address provides none. Cheap prefix checks avoid pulling in any
address-parsing dependency.

**Alternatives considered**:
- *Trust QMI to only ever hand out globals*: rejected — defensive check is one
  function and prevents a spurious hook fire / misleading status if the modem
  reports a link-local during bring-up.

## R5 — Best-effort supervision without disturbing IPv4 (FR-004/FR-005)

**Decision**: Keep the existing supervise loop and IPv4 health semantics untouched.
Add v6 as a **secondary, non-gating** concern inside the same loop iteration: after
the (unchanged) IPv4 probe/redial logic, check whether a global v6 address is
present; if not, attempt a bounded v6 (re)establish that must never `teardown` or
`ip addr flush` the v4 address, never `exit`, and never affect the healthcheck. The
healthcheck script stays byte-for-byte behavior-identical (gates on
`session_established` = IPv4 + `probe_dns`); a test asserts it ignores v6.

The v6 (re)establish is rate-limited by a **capped backoff** (Clarification
2026-08-13): the next attempt is due no sooner than `INTERNET_PROBE_INTERVAL`,
doubling up to `INTERNET_IPV6_RETRY_MAX` (default `5m`). The backoff resets the
moment a global v6 address is up, so a transient drop recovers within a probe
interval while a v6-incapable carrier is not retried every loop. State is a
`V6_NEXT_RETRY` deadline + current interval carried across iterations.

**Rationale**: Coupling v6 into the same single loop honors Principle V (one
supervisor, not two) while the strict "v6 code path may not touch v4 state or exit"
rule enforces FR-004/FR-005. The container healthcheck is what gates the bridge
(`depends_on: service_healthy`), so leaving it v4-only is the mechanism that
guarantees v6 can never block VoWiFi.

**Identity revalidation (added after review)**: because this loop is the *only*
thing that runs while IPv4 is healthy, it is also the only place a stale v6 identity
can be caught. `v6_teardown_cleanup` runs solely on shutdown or an IPv4 redial, so a
retained CID (kept by a stop that failed while the session still reported connected)
or an `adopted` mode whose autoconnect session ended would otherwise be reused
forever — every attempt re-querying a dead client, never dialling a replacement,
stranding reach-back until a container restart. Rule: **a reused identity that
cannot produce a global v6 address is released and forgotten**, so the next attempt
starts fresh. An identity started in the same call is torn down instead, so a
just-created client is never leaked.

**Alternatives considered**:
- *Separate background v6 supervisor process*: rejected — two supervisors touching
  one interface invites races on redial (v4 teardown vs. v6 bring-up) and doubles the
  lifecycle bugs 032 was careful to avoid. YAGNI.
- *Probe the retained client's status each tick to decide validity*: rejected —
  `--wds-get-packet-service-status` on a fresh client can report the *modem's*
  overall (IPv4) session as connected, so it is an ambiguous liveness signal for v6.
  Keying off "can it still give us a global v6 address" tests exactly what the
  feature needs, with no extra QMI round trip.
- *Make health `require_ipv6` configurable*: the operator explicitly chose
  "stay healthy on v4, keep retrying v6", so no such flag is added (keeps surface
  minimal; can be added later if a deployment ever needs it).

## R6 — Address-change hook mechanism (FR-008/FR-009)

**Decision**: New env var `INTERNET_IPV6_HOOK` (path to an executable). De-dupe
gates on a **success marker file** (`${INTERNET_STATUS_FILE}.v6notified`) that holds
the address the last hook run *succeeded* for — not an in-memory var. When the
current global v6 address differs from the marker **and** a hook is configured,
invoke it as `"$INTERNET_IPV6_HOOK" "$addr"` in a **detached, time-bounded**
subshell (`( timeout <N> "$hook" "$addr" && printf %s "$addr" > marker ) &`), so a
slow/hanging hook cannot stall the loop and the marker advances **only on exit 0**.
notify is called both on the up/change transition and on every supervise tick while
up, so a failed hook is **retried** each tick until it succeeds. Missing/
non-executable hook path → one warning log, no effect on connectivity.

**Rationale**: A fire-and-forget, time-bounded subshell satisfies "notify my tooling
with the new address" without the sidecar taking on DNS credentials or a network
dependency (both declined). Backgrounding + `timeout` give FR-009 isolation.
Gating on a *success* marker (rather than "attempted") means a transient DDNS outage
right after the link comes up does not strand the AAAA record for the prefix's whole
lifetime — the record is what makes the host reachable (SC-001), so "notified once,
even if it failed" would defeat the feature. The marker also persists de-dupe across
a sidecar restart. The hook is **not** fired when v6 is lost (Clarification
2026-08-13): loss is reflected in status/logs only, and downstream consumers expire
stale records via TTL. A single "current global address" argument, no
sentinel/withdrawal convention.

**Alternatives considered**:
- *Run hook synchronously in the loop*: rejected — a hanging hook would stall
  probing/redial (violates FR-009).
- *Fire on every observation*: rejected — FR-008 requires once-per-change.
- *Fire on loss with an empty/sentinel argument*: considered and declined by the
  operator — TTL-based expiry is simpler and avoids a second argument convention.
- *Built-in DDNS client*: declined by operator (see Complexity Tracking).

## R7 — IPv6 reachability probe / status (FR-007)

**Decision**: Do **not** add a v6 reachability probe to the healthcheck (health is
v4-only by FR-004). For observability only, record in the status file: `ipv6=<global
addr or empty>` and an `ipv6_state=up|unavailable` indicator, updated within one
probe interval. Optionally attempt a best-effort v6 DNS resolve for logging, but its
result never affects exit codes.

**Rationale**: Keeps the health contract unchanged while making v6 state observable
(`vowifi-status`-style introspection and the operator's own tooling can read it).

**Alternatives considered**:
- *Gate health on a v6 probe*: directly violates FR-004; rejected.

## R8 — Container / host prerequisites for inbound v6 (FR-006)

**Decision**: The sidecar installs **no** firewall rules (it never has). It sets
`disable_ipv6=0` on the wwan iface and installs the global address + default route,
then leaves inbound open. Document in quickstart that (a) the container already runs
host-network + privileged, so the global v6 address lands on the host and makes the
host itself reachable; (b) the operator is responsible for any host firewall
(e.g. allowing inbound SSH on v6) and must confirm the carrier does not filter
inbound. `net.ipv6.conf.all.forwarding` is **not** enabled (reach-back target is the
host itself, not a routed downstream — FR-006 / Assumptions).

**Rationale**: Matches the operator's decision ("reach-back = the host itself") and
the sidecar's minimal, non-firewalling posture. Carrier-side inbound filtering is
explicitly out of scope (spec Edge Cases / Assumptions).

**Alternatives considered**:
- *Enable IPv6 forwarding / NPTv6*: rejected — no downstream network in scope; would
  add attack surface and complexity for no requirement.

## Summary of decisions feeding Phase 1

| # | Decision | Feeds |
|---|----------|-------|
| R1 | Always a separate `ip-type=6` session alongside v4 (`dual-session`); adopt on `NoEffect` | data-model (v6 session identity), tasks |
| R2 | Parse `IPv6 address:` (addr/prefix) from current-settings | apply-v6 function |
| R3 | `ip -6 addr add` / `ip -6 route replace default`; flush global on redial | apply/teardown v6 |
| R4 | `is_global_v6()` rejects fe80::/10, ::1, fc00::/7 | status + hook gating |
| R5 | v6 folded into existing loop; may never touch v4 state or exit | supervise loop |
| R6 | `INTERNET_IPV6_HOOK` fired backgrounded+`timeout`; de-dupe on a success marker, retried until it succeeds | hook contract |
| R7 | v6 is observability-only in status; health stays v4-only | status-file contract, healthcheck (unchanged) |
| R8 | No firewall; no forwarding; document host-firewall responsibility | quickstart |
