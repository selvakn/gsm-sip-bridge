# Data Model: SIP Server Mode

**Feature**: 024-sip-server-mode | **Date**: 2026-07-31

All state introduced by this feature is **in-memory and process-local**. Nothing
is persisted: registrations are re-established by the phones themselves after a
restart, which is what the SIP registration refresh cycle already guarantees.

---

## 1. Configuration entities

### `SipServerConfig` — runtime view of `[sip_server]`

`gsm-sip-bridge/src/config/mod.rs`, built from `RawSipServer` by
`config/build.rs::build_sip_server`.

| Field | Type | Default | Validation when `enabled` |
|---|---|---|---|
| `enabled` | `bool` | `false` | — |
| `listen_addr` | `String` | `"0.0.0.0"` | must parse as `IpAddr` |
| `listen_port` | `u16` | `5060` | `1..=65535`; must differ from `[sip].local_port` |
| `realm` | `String` | `"gsm-sip-bridge"` | non-empty |
| `ring_aor` | `String` | `""` | non-empty; must equal some `account[i].username` |
| `min_expires` | `u32` | `60` | `30..=86400`; `<= max_expires` |
| `max_expires` | `u32` | `3600` | `30..=86400` |
| `nonce_lifetime_sec` | `u64` | `120` | `10..=3600` |
| `accounts` | `Vec<SipServerAccount>` | `[]` | at least one entry |

When `enabled == false`, only structural parsing applies — a disabled section
containing placeholder values must not block startup, matching `[vowifi]` and
`[volte]`.

### `SipServerAccount`

| Field | Type | Validation |
|---|---|---|
| `username` | `String` | non-empty; unique across all accounts |
| `password` | `Secret<String>` | non-empty; supports `env:VAR` indirection |

Passwords are held in `Secret<String>` (`config/secret.rs`) so they are redacted
from `Debug` output. The key path `sip_server.account.password` is added to
`SECRET_KEY_PATHS` (`config/env.rs:23`) so a failed `env:` lookup is reported as
a missing *secret* rather than a missing string.

### Cross-section rules

Enforced in `build()` after all sections exist, in the shape of the existing
`build_alerts(raw.alerts, &sms)` precedent:

| Rule | Error when violated |
|---|---|
| `sip.server` set | meaningless in server mode — there is no PBX to register to |
| `sip.username` set | as above |
| `sip.password` set | as above |
| `bridge.sip_destination` non-empty | the destination is `sip_server.ring_aor` |
| `sip.local_port == sip_server.listen_port` | names the remedy (`set [sip].local_port = 5062`) |
| `sip.transport != "udp"` | the registrar is UDP-only in this version |

With the mode enabled, `sip.server`/`username`/`password` also cease to be
*required*, which is the reason `build_sip` must take `&SipServerConfig`.

---

## 2. Runtime entities

### `Binding` — one registered phone

`gsm-sip-bridge/src/sip/server/bindings.rs`

| Field | Type | Source |
|---|---|---|
| `aor` | `String` | the authenticated account name (user part) |
| `contact_uri` | `String` | verbatim from the REGISTER's `Contact` header |
| `source` | `SocketAddr` | where the REGISTER actually arrived from |
| `call_id` | `String` | the REGISTER's `Call-ID` |
| `cseq` | `u32` | the REGISTER's `CSeq` sequence number |
| `expires_at` | `Instant` | now + the granted expiry |
| `user_agent` | `Option<String>` | the `User-Agent` header, for diagnostics |

`contact_uri` is stored **verbatim and dialled verbatim**. When its host differs
from `source`, a WARN naming both is logged but the value is not rewritten —
rewriting breaks handsets that listen on a port other than the one they send
from.

### `BindingStore`

`Mutex<HashMap<String, Binding>>` keyed by `aor`, held behind an `Arc` because
the registrar thread writes while the call path reads. Locks are held only
across the map operation itself. Poison is tolerated with the codebase's
existing idiom, `.lock().unwrap_or_else(|e| e.into_inner())`
(`pjsua-safe/src/endpoint.rs:375`).

**One binding per AOR, not a contact set** — see research.md R-005. A second
REGISTER for the same account *replaces* the entry.

```rust
fn upsert(&self, b: Binding) -> Result<(), BindingError>
fn remove(&self, aor: &str)
fn get_live(&self, aor: &str, now: Instant) -> Option<Binding>
fn sweep(&self, now: Instant) -> usize      // drops expired, returns live count
fn live_count(&self, now: Instant) -> usize
```

Expiry is evaluated **lazily on read**; `sweep` runs only on the serve loop's
idle tick so gauges and logs stay honest. There is no background thread. `now`
is a parameter, not an internal clock read, so every rule is testable without
`sleep`.

#### Binding state transitions

```
        (none) ──REGISTER accepted──────────────▶ Live
          ▲                                        │
          │                                        ├─ REGISTER accepted ──▶ Live (replaced, expiry extended)
          │                                        │
          ├── Expires:0 accepted ──────────────────┤
          │                                        │
          └── expires_at reached (lazy, on read) ──┘
```

Only `Live` bindings are ever dialled. A lapsed binding is indistinguishable
from an absent one to the call path.

### `NonceEntry` / `NonceStore`

`gsm-sip-bridge/src/sip/server/auth.rs`

| Field | Type | Purpose |
|---|---|---|
| `issued_at` | `Instant` | compared against `nonce_lifetime_sec` |
| `last_nc` | `u32` | highest nonce-count seen; replay guard under `qop=auth` |

`Mutex<HashMap<String, NonceEntry>>` keyed by the nonce string, **capped at 256
entries with oldest-first eviction** so an unauthenticated peer cannot grow it
without bound. A nonce is issued on `401`, consumed on successful
authentication, and swept on the idle tick.

#### Nonce state transitions

```
   issued (on 401) ──▶ Live ──┬── correct response ─────────▶ consumed (removed)
                              ├── wrong response ──────────▶ Live (401, no stale)
                              ├── replayed nc ─────────────▶ Live (401, no stale)
                              ├── lifetime elapsed ────────▶ expired (401 stale=true)
                              └── evicted by cap ──────────▶ gone (401 stale=true)
```

### `CallTarget` — where an inbound call should go

`gsm-sip-bridge/src/sip/target.rs`. Replaces the duplicated rule currently in
`SipBridge::compute_destination_uri` (`sip/mod.rs:172`) and `vowifi::pbx_dest_uri`
(`vowifi/mod.rs:1037`).

```rust
enum CallTarget<'a> {
    Pbx { server: &'a str, port: u16, sip_destination: &'a str },
    RegisteredPhone { bindings: &'a BindingStore, aor: &'a str },
}

fn uri_for(&self, caller_did: &str, now: Instant) -> Result<String, String>
```

| Variant | Rule |
|---|---|
| `Pbx` | **Unchanged from today.** Empty `sip_destination` means DID passthrough (dial the caller's own number at the PBX); otherwise the fixed extension. Leading `+` is stripped. Yields `sip:{dest}@{server}:{port}`. |
| `RegisteredPhone` | `bindings.get_live(aor, now)` → that binding's `contact_uri`. `Err` when there is no live binding, carrying a message that names `aor`. |

Making this fallible is the only signature change that propagates: `SipBridge::compute_destination_uri`
becomes `Result<String, String>`, matching the error shape of its neighbours
`set_sound_device` and `make_call`.

---

## 3. Relationships

```
  SipServerConfig
     ├── accounts: Vec<SipServerAccount>   ──┐ credential lookup, by username
     ├── ring_aor ──────────────────────────┐│
     └── realm, expiry bounds, nonce life  ││
                                            ││
  Registrar (one per hosting process)       ││
     ├── UdpSocket (listen_addr:listen_port)││
     ├── NonceStore ────────────────────────┘│  challenge/verify
     └── BindingStore ───────────────────────┘  keyed by aor
              │
              │ get_live(ring_aor, now)
              ▼
      CallTarget::RegisteredPhone ──▶ contact_uri ──▶ Call::make (unchanged)
```

Exactly one process hosts a `Registrar`, chosen by the existing `register_trunk`
arbitration (research.md R-003). `BindingStore` is shared between that process's
registrar thread and its call path via `Arc`, never across processes.

---

## 4. Metrics

| Series | Type | Labels |
|---|---|---|
| `gsm_sip_bridge_sip_server_bindings` | Gauge | — |
| `gsm_sip_bridge_sip_server_ring_aor_registered` | Gauge | — |
| `gsm_sip_bridge_sip_server_registrations_total` | CounterVec | `outcome` |
| `gsm_sip_bridge_sip_server_requests_total` | CounterVec | `method`, `status` |
| `gsm_sip_bridge_sip_server_ring_target_missing_total` | Counter | — |

`outcome` ∈ { `accepted`, `challenged`, `rejected_auth`, `rejected_unknown_user`,
`rejected_stale`, `rejected_interval`, `deregistered` }.

`ring_aor_registered` is deliberately a **separate** gauge from `SIP_REGISTERED`
and `VOLTE_REGISTERED`, following the rationale already recorded at
`metrics/mod.rs:76-78`: an operator needs to see *which* registration is down,
not an aggregate that hides it.
