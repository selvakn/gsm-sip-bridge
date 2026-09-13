# Data Model: Advertise a configured public address in SIP/SDP

This feature adds one config field and threads one resolved value through
two existing structs. No database, no new entity with independent identity
or lifecycle — everything here is scoped to a single process's startup
configuration.

## SIP public address setting

The spec's one Key Entity, traced through each layer it actually passes
through.

| Layer | Type | Field | Notes |
|---|---|---|---|
| Raw TOML (`gsm-sip-bridge/src/config/raw.rs`) | `RawSip` | `public_addr: Option<String>` | New field, alongside `display_name: Option<String>` (same optional-string shape). Default `None`. Accepts an IPv4 literal or a hostname, exactly as typed in `[sip]`. |
| Validated config (`gsm-sip-bridge/src/config/mod.rs`) | `SipConfig` | `public_addr: Option<std::net::IpAddr>` | Built by `build_sip` (`build.rs`). Already-resolved — a hostname in the raw string never reaches this type as a string; only the resolved IP does. `None` means "unconfigured," preserving today's `pj_gethostip()`-driven default behavior (FR-005). |
| PJSIP transport config (`pjsua-safe/src/endpoint.rs`) | `EndpointConfig` | `public_addr: Option<std::net::IpAddr>` | Carried into `Endpoint::create`, applied to `tp_cfg.public_addr` (SIP signaling: Contact/Via). Also retained on `Endpoint` itself so `Account::register`/`Account::local` can read it. |
| PJSIP account (`pjsua-safe/src/account.rs`) | `Account` (new private field) | `public_addr: Option<std::net::IpAddr>` | Cached at construction (`register`/`local`) from the owning `Endpoint`. Re-applied to `acc_cfg.rtp_cfg.public_addr` on every account-config rebuild this crate performs, including `set_identity` — the one call site with no `Endpoint` reference to re-read from. |

### Validation rules (owned by `build_sip`, FR-006)

- Absent (`None` after TOML parse) → valid, no resolution attempted,
  `SipConfig.public_addr = None`.
- Present and parses as `std::net::IpAddr`:
  - IPv4 → valid, used directly, no DNS resolution performed (Decision 2).
  - IPv6 → **rejected**. `pjsua-safe::Endpoint::create` only ever builds the
    IPv4 SIP transport variants (`PJSIP_TRANSPORT_UDP`/`_TCP`/`_TLS`), never
    `_UDP6`/`_TCP6`/`_TLS6`, so an IPv6 address here would be advertised in
    Contact/Via/SDP with no matching IPv6 socket listening — config load
    fails with `BridgeError::Config` naming `sip.public_addr` rather than
    shipping that mismatch.
- Present, not an IP literal → resolved via `std::net::ToSocketAddrs`
  exactly once, at config-build time, keeping only IPv4 results:
  - Resolves to at least one IPv4 address → valid; the first such address
    becomes `SipConfig.public_addr` (`Some(ip)`).
  - Resolves only to IPv6 addresses, or fails to resolve at all (NXDOMAIN,
    resolver error, empty result) → config load fails with
    `BridgeError::Config`, naming `sip.public_addr` and the offending
    value — the bridge does not start (FR-006, SC-005).

### Lifecycle

- Resolved exactly once, during `load_config` (process startup), before any
  `Endpoint` or `Account` exists.
- Immutable for the lifetime of the process — there is no config-reload path
  for `[sip]` today, and this feature does not add one.
- Not persisted anywhere beyond process memory (no storage layer involved).

### Relationships

```text
RawSip.public_addr (Option<String>, as typed)
        │  build_sip: parse IP literal, or resolve hostname once
        ▼
SipConfig.public_addr (Option<IpAddr>, resolved)
        │  passed into EndpointConfig at Endpoint::create call sites
        │  (src/vowifi/mod.rs "Agent B", src/sip/mod.rs legacy bridge)
        ▼
EndpointConfig.public_addr (Option<IpAddr>)
        │  Endpoint::create: applied to tp_cfg.public_addr (SIP transport)
        │  also retained on Endpoint for Account to read
        ▼
Endpoint (holds Option<IpAddr>)
        │  read by Account::register / Account::local via &Endpoint param
        ▼
Account.public_addr (Option<IpAddr>, cached per-account copy)
        │  applied to acc_cfg.rtp_cfg.public_addr on every rebuild:
        │    - register()      (initial)
        │    - local()         (initial)
        │    - set_identity()  (every inbound SIP-server-mode call, FR-004)
        ▼
SDP c= line + Contact/Via headers advertise the configured address
```

### Out of scope for this entity

- The carrier-facing IMS/Gm-interface leg (`src/ims/agent`) has no
  `SipConfig`/`Endpoint`/`Account` dependency — it implements its own
  signaling and is untouched (spec Assumptions, FR confirmed by code
  reading in `plan.md`/`research.md`).
- The internal veth-linked leg (`src/ims/agent/veth.rs`) already ignores the
  SDP `c=` line and uses the peer socket address directly — no relationship
  to this entity at all (FR-007).
