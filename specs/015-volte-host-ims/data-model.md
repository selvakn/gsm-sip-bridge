# Data Model: Host-Side IMS Registration over LTE

**Feature**: `015-volte-host-ims` | **Date**: 2026-07-22

All state in this feature is **in-memory and process-scoped**. No new
persisted schema is introduced — registration outcomes reuse the existing
store, and PDN/discovery state is rebuilt by querying the modem rather than
cached across restarts. That is deliberate: the modem is the authority on
whether a PDN is active (FR-004 requires detecting and reusing an existing
attachment), so caching it would create a second source of truth.

---

## ImsPdn

The IMS network attachment (spec entity: *IMS Network Attachment*).

| Field | Type | Notes |
|---|---|---|
| `cid` | context id | Which PDP context carries the IMS PDN. Distinct from the internet context |
| `apn_requested` | string | What was asked for, e.g. `ims` |
| `apn_assigned` | string | What the network resolved it to, e.g. `ims.mnc043.mcc404.gprs`. Evidence the PDN is genuinely the carrier's IMS APN (FR-003) |
| `bearer_id` | integer | Network-assigned bearer. Distinguishes the IMS bearer from the internet bearer |
| `address` | IP address | Carrier-assigned. **May be IPv6-only** — validation must not require an IPv4 address |
| `family` | `V4` \| `V6` \| `DualStack` | Reported to the operator (FR-003) |
| `host_iface` | interface name | The host-side interface bound to this PDN |
| `displaced_cid` | optional context id | What the host data path was bound to before, so teardown can restore it (FR-005) |

**Validation rules**

- `apn_assigned` must be non-empty; an empty value means the network did not
  actually grant the PDN and must be reported as a refusal (US1 scenario 3).
- `address` must be a routable address. An all-zero IPv4 address alongside a
  valid IPv6 address is **normal, not an error** — this is the observed
  carrier behaviour.
- `bearer_id` must differ from the internet context's bearer id; equality
  indicates the modem silently reused the default bearer rather than granting
  a real IMS PDN.

### State transitions

```
Absent ──request──> Requested ──network grants──> Active ──bind host──> Bound
   ▲                    │                            │                    │
   │                    └── network refuses ──> Refused                   │
   │                                                                      │
   └────────────────────── release (restores displaced_cid) ──────────────┘
```

- `Active` → `Bound` is the step that makes the PDN usable from host software
  (FR-002).
- Re-requesting while `Active` or `Bound` returns the existing PDN rather than
  creating a second one (FR-004).
- Any transition to `Absent` must restore `displaced_cid` (FR-005).

---

## PcscfEndpoint

The IMS entry point (spec entity: *IMS Entry Point*).

| Field | Type | Notes |
|---|---|---|
| `address` | IP address | The P-CSCF |
| `port` | integer | Defaults to the SIP default unless discovery supplies one |
| `source` | `DiscoveryMethod` | Which method produced it — reported per FR-009 |

### DiscoveryMethod

Ordered; the chain attempts each in turn and stops at the first success
(FR-008). Order and rationale are fixed in `research.md` R2.

| Variant | Order | Notes |
|---|---|---|
| `ConfigOverride` | tried first | Operator-supplied; when present, skips discovery entirely (FR-010) |
| `Dhcpv6` | 1 | RFC 3319 options 21/22 |
| `RouterAdvertisement` | 2 | Also the source of the host's global address |
| `DnsNaptr` | 3 | On the home-network realm |

## DiscoveryReport

Produced by every discovery run, successful or not. Exists specifically to
satisfy FR-011 and SC-005 — a failure must be diagnosable without re-running.

| Field | Type | Notes |
|---|---|---|
| `attempts` | list of `MethodAttempt` | One per method tried, in order |
| `outcome` | optional `PcscfEndpoint` | Absent when every method failed |

### MethodAttempt

| Field | Type | Notes |
|---|---|---|
| `method` | `DiscoveryMethod` | |
| `result` | `Found(addr)` \| `NoResult` \| `Error(detail)` | `NoResult` (method ran, carrier returned nothing) is distinct from `Error` (method could not run). The distinction is the diagnostic value |
| `duration` | duration | Bounds troubleshooting of a hanging method |

---

## ImsIdentity

Spec entity: *IMS Identity*. **Derived by existing code** — this feature adds
nothing here, it reuses the rules already proven on the VoWiFi path (FR-013).

| Field | Notes |
|---|---|
| `impi` | Private identity, always IMSI-derived |
| `impu` | Public identity; IMSI-derived by default, MSISDN if configured |
| `realm` | `ims.mnc<MNC>.mcc<MCC>.3gppnetwork.org` |

---

## RegistrationAttempt

Spec entity: *Registration Session*, plus the diagnostics FR-015 requires.

| Field | Type | Notes |
|---|---|---|
| `stage` | `RegistrationStage` | Where the attempt got to — the core of FR-015 |
| `outcome` | `Accepted` \| `Rejected(reason)` \| `Failed(detail)` | |
| `expires` | optional duration | From the network; drives renewal (FR-016) |
| `transport` | `Epdg` \| `LteImsPdn` | Which transport carried it |

### RegistrationStage

Ordered stages. On failure the bridge reports the **last stage reached**, which
is what lets an operator distinguish a transport problem from a credential one.

```
AttachingPdn → DiscoveringPcscf → Connecting → Challenged
             → EstablishingSecurity → Authenticated → Accepted
```

Mapping to the FR-015 required distinctions:

| Failure at | Reported as |
|---|---|
| `AttachingPdn` | attachment failure |
| `DiscoveringPcscf` | entry-point discovery failure |
| `EstablishingSecurity` | signalling-protection failure |
| `Challenged` / `Authenticated` | credential or identity rejection |

### Registration state (US4)

```
Unregistered ──register──> Registered ──renew──> Registered
     ▲                          │  │                  │
     │                          │  └── renewal fails ─┘ (bounded retry)
     └── expiry lapsed / network deregisters ──────────┘
```

A network-initiated deregistration immediately after acceptance must move the
state back to `Unregistered` and be surfaced, never reported as success (spec
Edge Cases).

---

## Relationships

```
ImsPdn ──provides address for──> PcscfEndpoint discovery
   │                                     │
   └──────────┬──────────────────────────┘
              ▼
        ImsTransport  (the substitutable seam — see contracts/)
              │
              ▼
     RegistrationAttempt ──uses──> ImsIdentity
```

`ImsTransport` is what the registration machinery consumes. It is satisfied by
the LTE path (`ImsPdn` + `PcscfEndpoint`) and, equivalently, by the existing
ePDG path — which is precisely what makes FR-018 hold.
