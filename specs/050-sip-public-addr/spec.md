# Feature Specification: Advertise a configured public address in SIP/SDP

**Feature Branch**: `050-sip-public-addr`
**Created**: 2026-09-13
**Status**: Draft
**Input**: User description: "prepare for fixing the public_addr handling and fixing the bug."

## Why this exists

[GitHub issue #77](https://github.com/selvakn/gsm-sip-bridge/issues/77) reports
complete audio silence, in both directions, whenever a SIP softphone reaches
this bridge over a routed network — specifically Tailscale, but the same
applies to any VPN/mesh setup — while registering, ringing, and answering all
work normally. A softphone on the *same* host as the bridge has no problem.

The root cause, confirmed by reading the current tree: neither
`pjsua_transport_config` (the SIP transport, `pjsua-safe/src/endpoint.rs`) nor
`pjsua_acc_config.rtp_cfg` (the per-account RTP media config,
`pjsua-safe/src/account.rs`) ever sets `public_addr`. With that unset, PJSIP's
`create_rtp_rtcp_sock()` falls back to `pj_gethostip()`, which resolves the
container's default-route interface — a private address like `10.0.2.15` —
and that address is what ends up in the SDP `c=` line and in the SIP
Contact/Via headers. A remote softphone gets told to send its RTP to that
private address, which it cannot route to, so its outbound audio is dropped
before it ever reaches the bridge; with no inbound RTP, PJSIP's symmetric-RTP
latching never triggers, so the bridge never learns the softphone's real
address either. Dead air, both ways.

There is a second, sharper edge to this: `Account::set_identity` (used to
rewrite the caller-ID display name on inbound calls in SIP-server mode, called
from `src/vowifi/mod.rs`) rebuilds the account config from
`pjsua_acc_config_default()` and re-applies it via `pjsua_acc_modify()`. Any
media configuration not explicitly re-applied at that point is silently reset
to PJSIP's defaults — so even a deployment that found some other way to get
`public_addr` set once could lose it again the moment an inbound call
triggers that rewrite.

This is a real, previously-undiagnosed gap, not a duplicate of prior work.
Git history was checked (`2a04eae`, "fix(docker): drop pjsip's per-call
local-hostname lookup") — that commit only disabled the blocking
hostname-resolution *candidate* inside `pj_gethostip()`'s address-discovery
chain (a DNS-timeout bug that could cause the carrier/PBX to abandon an
INVITE), via `PJ_GETHOSTIP_DISABLE_LOCAL_RESOLUTION`. It left the
default-route candidate — the private address that reaches the SDP — fully
intact, and explicitly documented that this is "what actually gets picked
anyway." Setting `public_addr` bypasses `pj_gethostip()`'s candidate selection
entirely when configured, so this feature is additive and does not conflict
with or undo that earlier fix; when this feature's new setting is left unset,
`pj_gethostip()`'s existing (already-fixed) behavior is exactly what still
runs.

The issue includes a community-contributed, AI-assisted patch sketch. Its
diagnosis was verified against the current code and is accurate (the call
sites and line numbers match). Its proposed *fix* is directionally right —
add `public_addr` to both the transport and every account-config rebuild
site — but reads the address from a raw `SIP_PUBLIC_ADDR` environment
variable, bypassing this project's existing `[sip]` TOML config section
(`src/config/raw.rs` `RawSip` → `src/config/mod.rs` `SipConfig`) that every
other SIP setting (`local_port`, `transport`, `tls_verify`, ...) already goes
through. It also bundles an unrelated Opus-codec change the submitter
themselves flagged as out of scope. This feature keeps the diagnosis, routes
the fix through the project's normal config path, and drops the unrelated
change.

## Clarifications

### Session 2026-09-13

- Q: The configured public address may be an IP literal or a hostname (e.g.
  Tailscale MagicDNS). Given `Account::set_identity` re-applies the address
  on every inbound call in SIP-server mode, and `2a04eae` already fixed one
  blocking-DNS-in-the-call-path bug, should a hostname be resolved once at
  startup and cached, resolved on every use, or disallowed entirely (IP
  literal only)? → A: Hostname allowed, resolved once at startup and
  cached; every subsequent use (including `set_identity` rebuilds) reuses
  the cached IP — no resolution work ever occurs in the call path.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A remote SIP client gets working audio over a VPN (Priority: P1)

An operator runs the bridge under Docker with host networking, reachable to
their SIP clients only over Tailscale (or an equivalent routed/VPN network).
A softphone on a phone or laptop, connected only via that network, registers,
places or receives a call, and now hears — and is heard by — the far end,
instead of the current complete two-way silence.

**Why this priority**: This is the exact failure GitHub issue #77 reports,
and the entire reason this feature exists. Without it, the bridge is
unusable for any SIP client that isn't on the same host/LAN segment as the
bridge itself.

**Independent Test**: Register a softphone to the bridge from a device
reachable only via a routed/VPN network (not the bridge's local LAN
segment), with the new public-address setting configured; place a call in
each direction and confirm two-way audio, versus confirmed silence with the
setting absent.

**Acceptance Scenarios**:

1. **Given** the public-address setting is configured to the bridge's
   Tailscale (or equivalent routable) address, **When** a remote softphone
   places an outbound call through the bridge, **Then** the SDP the bridge
   sends carries that configured address, and audio flows in both
   directions.
2. **Given** the same configuration, **When** an inbound call is routed to
   that remote softphone, **Then** the SDP the bridge sends carries the
   configured address, and audio flows in both directions.
3. **Given** the same configuration, **When** a softphone on the bridge's
   own host/LAN (the previously-working case) places or receives a call,
   **Then** audio continues to work exactly as it does today — the fix for
   remote clients does not regress the local case.

---

### User Story 2 - The setting lives in the bridge's normal config, not an ad hoc env var (Priority: P2)

An operator sets the public address the same way they already set every
other SIP transport setting — one field in the `[sip]` section of the
bridge's config file — rather than needing a separate environment variable
that isn't visible alongside the rest of the SIP configuration.

**Why this priority**: Consistency with the existing config surface is what
makes this maintainable and discoverable; a one-off environment variable
would be invisible to config validation, config-file-driven deployment
tooling, and anyone reading the `[sip]` section to understand how the bridge
is set up.

**Independent Test**: Set the new field in the `[sip]` section of a test
config file, start the bridge, and confirm (via logs or behavior) that the
configured address is the one advertised — with no environment variable
involved.

**Acceptance Scenarios**:

1. **Given** a config file with the new `[sip]` field set, **When** the
   bridge starts, **Then** it advertises that address without requiring any
   environment variable to be set.
2. **Given** a config file where the new field is malformed (not a usable
   address), **When** the bridge starts, **Then** it fails fast with a clear
   error identifying the offending setting, rather than starting up with
   silently broken audio.

---

### User Story 3 - Inbound calls keep working audio after caller-ID rewrite (Priority: P1)

An inbound call arrives while the bridge is running in SIP-server mode, which
triggers a caller-ID display-name rewrite (`Account::set_identity`) before
the call is offered to the local extension. The configured public address
is still advertised in that call's SDP — it does not get silently dropped by
the rewrite.

**Why this priority**: This is the specific regression class the root-cause
analysis in issue #77 calls out by name: `set_identity` rebuilds the account
config from PJSIP defaults, so a fix that only touches the "happy path"
account setup would still break inbound audio the moment this rewrite runs.
It's equally load-bearing as User Story 1 — a fix that covers outbound calls
but not this inbound path is an incomplete fix, not a partial one.

**Independent Test**: Configure the public-address setting, place an
inbound call in SIP-server mode (triggering the caller-ID rewrite), and
confirm the SDP the bridge sends for that call still carries the configured
address and audio flows both ways.

**Acceptance Scenarios**:

1. **Given** the public-address setting is configured, **When** an inbound
   call triggers the SIP-server caller-ID rewrite, **Then** the SDP offered
   to the local extension still carries the configured address.
2. **Given** the same scenario, **When** the call is answered, **Then**
   audio flows in both directions exactly as in User Story 1.

---

### Edge Cases

- The new setting is left unconfigured: the bridge's behavior must be
  unchanged from today, including the existing (already-fixed) blocking
  hostname-lookup workaround (`PJ_GETHOSTIP_DISABLE_LOCAL_RESOLUTION`)
  remaining fully in effect.
- The configured value is syntactically malformed (not a usable
  address/hostname form): the bridge must fail to start with a clear,
  specific error rather than silently falling back to default addressing
  and producing a deployment with no obvious cause for broken audio.
- The configured value is a well-formed hostname that fails to resolve at
  startup (DNS failure, no records, etc.): the bridge must fail to start
  with a clear, specific error, exactly as for a syntactically malformed
  value — resolution is only ever attempted once, at startup, never
  deferred into the call path.
- An account's config is rebuilt for any reason during the lifetime of the
  account (not just the known `set_identity` rewrite): the
  already-resolved, cached address must still end up applied — no fresh
  resolution occurs at that point — since the underlying cause (PJSIP
  config rebuilds default to no media addressing) is general, not specific
  to one call site.
- The internal veth-linked leg between the bridge's two internal agents
  (Agent A/Agent B) must be completely unaffected — it already determines
  its RTP destination from the peer socket address, not the SDP `c=` line,
  and this feature must not change that.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST provide a setting in the bridge's existing
  `[sip]` configuration section through which an operator can specify a
  public/routable address for SIP signaling and RTP media.
- **FR-002**: When that setting is configured, the system MUST advertise the
  configured address in the SIP transport's signaling (Contact/Via) instead
  of an address derived from local interface or hostname discovery.
- **FR-003**: When that setting is configured, the system MUST advertise the
  same configured address in the SDP connection information for RTP media,
  for every call — both calls this bridge places and calls it answers — that
  uses the bridge's PBX/softphone-facing PJSIP transport.
- **FR-004**: The configured address MUST remain in effect after any
  operation that rebuilds or reinitializes an account's PJSIP configuration
  during that account's lifetime, including (but not limited to) the inbound
  caller-ID identity rewrite performed in SIP-server mode.
- **FR-005**: When the setting is left unconfigured, the system MUST behave
  exactly as it does today — including the existing blocking-hostname-lookup
  fix remaining fully effective — introducing no change in default behavior.
- **FR-006**: The system MUST validate the configured address at startup —
  including resolving it if it is a hostname — and refuse to start, with a
  clear and specific error, if the value is syntactically malformed or fails
  to resolve — never silently ignoring it and falling back to default
  addressing.
- **FR-007**: This feature MUST NOT change how the internal veth-linked leg
  between the bridge's two internal agents determines its RTP destination —
  that leg continues to rely on the peer socket address, not the negotiated
  SDP, exactly as it does today.
- **FR-008**: A hostname value MUST be resolved at most once, at startup;
  the resolved address MUST be cached and reused for every subsequent SIP/
  SDP advertisement and account-config rebuild (FR-004) — no DNS resolution
  may occur anywhere in the per-call code path, including the
  `set_identity` rewrite.

### Key Entities

- **SIP public address setting**: A single operator-configured value in the
  `[sip]` section, alongside existing settings like `local_port` and
  `transport`, holding the routable address (IP literal or hostname) the
  bridge should advertise for both SIP signaling and RTP media. When a
  hostname, resolved to an IP exactly once at startup; that resolved IP —
  not the original hostname — is what every SIP transport and account
  rebuild actually applies. Absent by default, preserving today's behavior.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A SIP client reachable only over a routed/VPN network and
  registered to the bridge gets two-way audio on every call, matching the
  reliability already seen for a client on the bridge's own host/LAN.
- **SC-002**: Enabling this capability for a deployment requires editing
  exactly one field in the bridge's existing config file — no environment
  variable, code change, or per-deployment patch is needed.
- **SC-003**: An inbound call that triggers the SIP-server caller-ID rewrite
  delivers audio with the same reliability as an inbound call that doesn't —
  the regression class identified in the root-cause analysis no longer
  exists.
- **SC-004**: A deployment that leaves the new setting unconfigured shows no
  change in registration behavior, call setup, or audio path compared to
  before this fix.
- **SC-005**: Starting the bridge with an invalid or unresolvable value for
  the new setting fails immediately with a clear error, instead of
  producing a running deployment with silent one-way/no-audio calls.
- **SC-006**: An inbound call in SIP-server mode never incurs a DNS lookup
  as part of handling that call — the call-answering path has identical
  latency whether the public address is unconfigured, an IP literal, or a
  hostname.

## Assumptions

- Scope is limited to the bridge's PBX/softphone-facing PJSIP transport —
  the component built via `pjsua-safe::Endpoint`/`Account`, used by the
  VoWiFi/VoLTE SIP-server-mode bridge (`src/vowifi/mod.rs`, "Agent B") and
  the legacy circuit-switched SIP bridge (`src/sip/mod.rs`). It does not
  touch the carrier-facing IMS/Gm-interface leg (`src/ims/agent`), which
  implements its own signaling outside `pjsua-safe` and already handles its
  own tunnel/NAT addressing separately. Confirmed by reading both call sites
  of `Endpoint::create` in the current tree — both are on the PBX-facing
  side, neither is carrier-registration code.
- One configuration value applies bridge-wide to the whole PJSIP transport,
  matching how `[sip].local_port` and `[sip].transport` already apply,
  rather than being scoped per-account — the SIP-server registrar and the
  outbound trunk share the same `Endpoint` in this bridge's architecture.
- The configured value is accepted as a plain address string — an IPv4
  literal or a resolvable hostname (e.g. a Tailscale MagicDNS name) —
  matching how the existing `[sip].server` setting is already handled,
  rather than being restricted to IPv4-literal syntax only. A hostname is
  resolved exactly once, at startup, and that resolved IP is cached and
  reused thereafter (Clarifications, FR-008) — never re-resolved from the
  call path, so this cannot reintroduce the per-call blocking-DNS class of
  bug `2a04eae` fixed.
- IPv6 is out of scope and rejected at config load, not silently accepted:
  `pjsua-safe::Endpoint::create` only ever builds the IPv4 SIP transport
  variants (`PJSIP_TRANSPORT_UDP`/`_TCP`/`_TLS`), never `_UDP6`/`_TCP6`/
  `_TLS6`, so an IPv6 `public_addr` would be advertised in Contact/Via/SDP
  with no matching IPv6 socket listening — breaking registration and every
  call rather than fixing anything. A hostname that resolves only to IPv6
  addresses is treated the same as one that fails to resolve at all
  (FR-006). Extending the SIP transport itself to support IPv6 is a
  separate feature, not part of this one.
- No environment-variable-based configuration path is introduced. The
  community-submitted patch's `SIP_PUBLIC_ADDR` env var is superseded by a
  proper `[sip]`-section field, consistent with every other bridge setting.
- The GitHub issue's incidentally-bundled Opus-codec change is out of scope
  for this feature and is not carried forward.
