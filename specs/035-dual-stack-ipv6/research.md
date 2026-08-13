# Phase 0 Research: Dual-Stack IPv6 for the Cellular-Internet Sidecar

Resolves the unknowns in the plan's Technical Context. Each item: Decision /
Rationale / Alternatives considered. Items marked **VERIFY-ON-HW** need a one-off
check against the actual EC20/EC25 + carrier before the corresponding task is
closed; the plan is written so either outcome is a small, localized change.

## R1 — How to request a dual-stack session over QMI (HARDWARE-VERIFIED, revised)

**Decision (as implemented)**: Dual-stack is ONE IPv4v6 bearer, not two sessions.
When v6 is enabled the sidecar provisions the data profile
(`INTERNET_IPV6_PROFILE`, default 1) as `pdp-type=IPv4v6` with the APN
(`--wds-modify-profile`), dials a single session by `profile-index`, and reads BOTH
IPv4 and IPv6 from that one session's `--wds-get-current-settings`. v6 rides the same
WDS client (`WDS_CID`) as v4; there is no separate v6 identity. When v6 is disabled,
the sidecar dials `ip-type=4` exactly as before.

**Why the original two-session design was wrong (Jio, 2026-08-13)**:
- `ip-type=6` **alone** returns a real global v6 address
  (`2409:4072:99:1a6a:.../64`) — so the carrier grants IPv6 and the parser's
  `IPv6 address: <addr>/<prefix>` format is exactly right.
- But `ip-type=6` **alongside** an `ip-type=4` session to the same APN is refused
  with QMI error 14 `CallFailed` / verbose `multiple-connection-to-same-pdn-not-allowed`.
  Jio (like most modern carriers) permits only ONE connection per PDN, carrying both
  families. Two separate sessions cannot work.
- There is **no `ip-type=8`** in QMI — the WDS IP-family preference is only 4 or 6.
  `ip-type=8` hung the modem. Dual-stack therefore MUST come from the profile's
  `pdp-type=IPv4v6`, not a per-call flag.
- A plain start (no ip-type) on this modem gave IPv4 only, because the default
  profile's PDP type is IPv4 — hence the sidecar must set it to IPv4v6.

**Consequences / simplifications**: one bearer means the whole separate-v6-session
machinery is gone — no `V6_WDS_CID`/`V6_PKT_HANDLE`/`V6_MODE`, no `qmi_wds_v6`, no
`bring_up_v6`/`v6_teardown_cleanup`, and the entire class of "stale retained v6
identity" bugs disappears. v4 stays healthy whether or not the bearer carries v6.

**Alternatives considered**:
- *Two separate sessions (original design)*: rejected — refused by Jio
  (`multiple-connection-to-same-pdn-not-allowed`).
- *Sidecar does not modify the profile, relies on it already being IPv4v6*:
  rejected as the default — the observed modem profile was IPv4, so v6 would never
  come up without provisioning. The modify is best-effort and logged on failure.

**STILL VERIFY-ON-HW**: the profile→IPv4v6 provisioning path (`--wds-modify-profile`
then `profile-index` dial) yielding BOTH families in one bearer was NOT completed
end-to-end on hardware (the test window closed). The carrier-grants-v6 fact and the
`IPv6 address:` label ARE confirmed; the single-bearer provisioning needs a live
run on the Jio SIM to confirm the dual bearer comes up as designed.

## R2 — Reading the granted IPv6 settings

**Decision**: Parse `qmicli --wds-get-current-settings` for the lines
`IPv6 address:` (form `2401:4900:...:1/64` — address **with** prefix length),
`IPv6 gateway address:`, and `IPv6 primary/secondary DNS:`. Extract the address and
the prefix length from the single `addr/prefix` token. The query uses `qmi_wds` (the
SAME shared client as v4) — the single IPv4v6 bearer's settings carry both families,
so one `--wds-get-current-settings` returns IPv4 *and* IPv6 lines.

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
After the (unchanged) IPv4 probe/redial logic, `refresh_v6` re-reads the current
bearer's settings and applies/clears the global v6 accordingly. It must never
`teardown` or `ip addr flush` the v4 address, never `exit`, and never affect the
healthcheck (which stays byte-for-byte identical, gating on `session_established` =
IPv4 + `probe_dns`; a test asserts it ignores v6).

**Superseded — no backoff (revised after the R1 hardware finding)**: the original
Clarification (2026-08-13) called for a capped backoff because v6 was a *separate
dial* to rate-limit. In the single-bearer model there is no separate v6 dial —
`refresh_v6` is just a `--wds-get-current-settings` read on the existing bearer, so
running it every probe interval is cheap and cannot "hammer" the modem. The backoff,
`V6_NEXT_RETRY`, `V6_RETRY_INTERVAL`, and `INTERNET_IPV6_RETRY_MAX` are therefore
removed. The intent of the clarification (don't churn the modem chasing v6) is met
even more strongly.

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
| R1 | Single IPv4v6 bearer (provision profile `pdp-type=IPv4v6`, dial by `profile-index`, read both families); no separate v6 session, no `ip-type=8` | data-model, dial |
| R2 | Parse `IPv6 address:` (addr/prefix) from current-settings | apply-v6 function |
| R3 | `ip -6 addr add` / `ip -6 route replace default`; flush global on redial | apply/teardown v6 |
| R4 | `is_global_v6()` rejects fe80::/10, ::1, fc00::/7 | status + hook gating |
| R5 | v6 folded into existing loop as a cheap settings re-read; no separate dial, so no backoff; may never touch v4 state or exit | supervise loop |
| R6 | `INTERNET_IPV6_HOOK` fired backgrounded+`timeout`; de-dupe on a success marker, retried until it succeeds | hook contract |
| R7 | v6 is observability-only in status; health stays v4-only | status-file contract, healthcheck (unchanged) |
| R8 | No firewall; no forwarding; document host-firewall responsibility | quickstart |
