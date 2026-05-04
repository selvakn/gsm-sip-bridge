# Phase 1 — Data Model

**Feature**: `008-rust-rewrite` (gsm-sip-bridge v5.0.0)
**Date**: 2026-05-05
**Sources**: [spec.md § Key Entities](./spec.md), [research.md R-07, R-12](./research.md)

This document captures the in-process and persisted data model. Field-level Rust types are illustrative — the contracts in `contracts/db.schema.sql` and `contracts/config.toml.schema.md` are the authoritative source for on-disk shapes.

---

## Entities

### Module

Represents one connected Quectel EC20 modem. Lifetime = process lifetime once detected.

| Field | Type | Notes |
|---|---|---|
| `id` | `String` | Stable identifier derived from USB serial number, e.g. `ec20-A1B2C3`. Uppercase last 6 hex chars of the USB serial. Same physical module always gets the same ID across boots. (FR-015) |
| `serial_port` | `PathBuf` | e.g. `/dev/ttyUSB2`. Selected as the AT command port among the module's 4 USB tty endpoints. |
| `audio_device` | `String` | ALSA device spec, e.g. `hw:1,0`. |
| `health` | `Health` | Enum: `Active`, `Failed { reason: String, since: Instant }`. |
| `current_call` | `Option<CallId>` | If `Some`, a call is in progress on this module. |
| `gsm_registration` | `GsmRegistration` | Enum: `Registered`, `Searching`, `Denied`, `Roaming`. Updated by AT command poll. |

**State transitions** (Health):

```text
                 init success                 failure during operation
   [discovered] ─────────────► [Active] ──────────────────────────► [Failed]
                                  ▲                                     │
                                  │ retry success (FR-017)              │
                                  └─────────────────────────────────────┘
                                       background retry, default 30s
```

A `Failed` module has its in-flight call (if any) torn down and is excluded from the round-robin RING dispatch. The retry thread retries `init` every `retry_interval_sec` (config default 30) until success or process exit.

**Invariants**:
- Two `Active` modules MUST NOT share `serial_port` or `audio_device`.
- `id` is unique within the process and stable across `Active` ↔ `Failed` transitions.
- `current_call.is_some()` implies `health == Active`.

---

### Call Record

One incoming GSM call. Lifetime = persisted indefinitely (FR-042).

| Field | Type | Notes |
|---|---|---|
| `id` | `i64` | SQLite rowid, autoincrement. Primary key. |
| `module_id` | `String` | Foreign reference to Module.id; NOT enforced as SQL FK because Module doesn't persist. |
| `caller_id` | `String` | GSM caller's MSISDN as reported by `+CLIP`. May be empty if CLIP is suppressed. |
| `started_at` | `String` (ISO 8601, UTC, `YYYY-MM-DDTHH:MM:SS.fffZ`) | Time the GSM RING was observed. |
| `duration_seconds` | `REAL` | 0.0 for missed calls; otherwise wall-clock seconds from GSM answer to either-side hangup. |
| `status` | `String` | One of: `answered`, `missed`, `failed`. |
| `sip_destination` | `String` | The DID or fixed extension dialed; empty for missed calls (we never made a SIP INVITE). |

**State transitions** (status):

```text
   [created on RING, status=missed, duration=0]
        │
        │ ATA succeeds, SIP INVITE about to be sent
        ▼
   [in_progress (in-memory only)]
        │
        ├── SIP party answers → [status=answered, duration accumulates]
        │       │
        │       └── either side hangs up → [final: answered, duration finalized]
        │
        └── SIP failure (busy/timeout/unreachable) → [status=failed]
```

The `in_progress` state is in-memory; the row is written once with the final status when the call ends (or as `missed` if no SIP attempt is made).

**Invariants**:
- `status = answered` implies `duration_seconds > 0`.
- `status = missed` implies `sip_destination = ""`.
- A row is never updated after insertion — calls are append-only.

---

### SMS Record

One incoming SMS. Lifetime = persisted indefinitely.

| Field | Type | Notes |
|---|---|---|
| `id` | `i64` | SQLite rowid. |
| `module_id` | `String` | The module that received the SMS. |
| `sender` | `String` | MSISDN of the sender. |
| `body` | `String` | Plain-text body. UTF-8. The bridge must handle PDU mode and concatenated SMS internally before persistence. |
| `received_at` | `String` (ISO 8601 UTC) | Timestamp the SMS was read from the SIM via AT command. |
| `forwarding_status` | `String` | One of: `pending`, `sent`, `failed`, `skipped`. |
| `forwarded_at` | `String` (ISO 8601 UTC) | Nullable. Set when `forwarding_status` transitions out of `pending`. |
| `discord_status_code` | `INTEGER` | Nullable. The HTTP status of the Discord POST, when applicable. |

**State transitions** (forwarding_status):

```text
   [pending] (row written before SIM deletion per FR-031)
        │
        ├── webhook configured, POST returns 2xx → [sent]
        ├── webhook configured, POST fails after retries → [failed]
        └── webhook URL empty in config → [skipped]
```

Once a row reaches `sent`, `failed`, or `skipped`, it is terminal — the bridge never re-attempts forwarding. Operators wanting to replay can do so with a manual SQL update plus a custom script (out of scope for v5.0.0).

**Invariants**:
- A row exists in the SMS store before the SIM `+CMGD` (delete) is sent (FR-031).
- `forwarding_status = sent` implies `forwarded_at IS NOT NULL` and `discord_status_code IN (200..300)`.
- `forwarding_status = skipped` implies `discord_webhook_url` was empty when the SMS arrived.

---

### SIP Account

The single shared SIP server registration used by every module's bridged calls. In-process only.

| Field | Type | Notes |
|---|---|---|
| `server` | `String` | PBX hostname or IP. |
| `port` | `u16` | Default 5060. |
| `username` | `String` | SIP account username. |
| `password` | `Secret<String>` | Resolved at startup; never logged. |
| `transport` | `Transport` | Enum: `Udp`, `Tcp`, `Tls`. |
| `local_port` | `u16` | Default 5060 (matches v4.1.x to avoid stale registrations). |
| `display_name` | `String` | Defaults to `username`. |
| `tls_verify` | `TlsVerify` | Enum: `Strict` (default), `Skip`. (R-03) |
| `registration_state` | `RegistrationState` | Updated by PJSIP callback: `Unregistered`, `Registering`, `Registered`, `RegistrationFailed { code, reason }`. |

`Secret<T>` is a newtype that:
- Implements neither `Display` nor `Debug` directly; instead its `Debug` and `Display` impls return the literal `[REDACTED]`.
- Provides `expose_secret(&self) -> &T` for the few call sites that legitimately need the value (PJSIP credentials struct, Discord webhook URL formatting).
- Is the type of all sensitive config keys regardless of whether they were supplied as literal strings or `env:VAR_NAME` references at load time.

---

### Bridge Configuration

The full operator-supplied configuration loaded from `config.toml` at startup. Fields drive everything else.

See `contracts/config.toml.schema.md` for the field-by-field authoritative definition. Highlights:

- `[sip]` section → SipAccount fields above.
- `[bridge]` section → `sip_destination` (empty = DID passthrough), `sip_dial_timeout_sec` (5..120, default 30).
- `[sms]` section → `enabled`, `discord_webhook_url` (`Secret<String>`, may be empty), `db_path`.
- `[metrics]` section → `port` (default 9091, override via `METRICS_PORT` env var or this key — env var wins).
- `[modules]` section → `retry_interval_sec` (default 30), `max_concurrent` (informational, must be ≤8).

**Loading rules**:
- File missing → bridge refuses to start with a clear error pointing at the expected path.
- Required fields missing (any in `[sip]` other than the optional `display_name`/`tls_verify`/`local_port`) → refuse to start, naming each missing field.
- Sensitive values referencing `env:VAR_NAME` where the env var is unset/empty → refuse to start (FR-077), naming the env var.
- Unknown keys → log a `WARN` but proceed (forward-compatibility with future versions).

---

### Migration Guide

Operator-facing Markdown document at `docs/migrating-from-v4.1.x.md`. Not a runtime entity but a deliverable artifact (FR-074, R-15). Required content:

| Section | Maps which v4.1.x → v5.0.0 surface |
|---|---|
| Configuration | INI keys → TOML keys, with `env:` reference syntax explained |
| Database | SQL `ALTER`/`INSERT…SELECT`/`DROP`/`VACUUM` snippets producing `store.db` from old `sms.db` |
| Metrics | `gsm_bridge_*` → `gsm_sip_bridge_*` rename map |
| CLI | flag-by-flag mapping |
| Docker Compose | full new compose file shown verbatim |
| Roll-back | steps to revert if v5.0.0 misbehaves |

---

## Cross-Entity Relationships

```text
                                ┌────────────┐
                                │ SipAccount │ (single, shared)
                                └─────┬──────┘
                                      │ each Module's call uses this account
                                      │ for SIP INVITE / registration
                                      ▼
   ┌──────────┐  1                * ┌──────┐  1            *  ┌─────────────┐
   │ CardPool │ ──────────────────► │Module│ ─────────────────►│ Call Record │ (persisted)
   └──────────┘                     └──────┘                   └─────────────┘
                                       │
                                       │ 1
                                       │
                                       ▼ *
                                  ┌─────────────┐
                                  │ SMS Record  │ (persisted)
                                  └─────────────┘
```

- One `SipAccount` per process.
- One `CardPool` per process; owns 0..8 `Module`s.
- One `Module` per physical EC20 module; can have at most one `current_call` at a time.
- `Call Record` and `SMS Record` are append-only persistent rows tagged by `module_id`.

## Concurrency model overview (data-flow oriented)

```text
   tokio runtime                          dedicated OS threads
   ┌────────────────────────────┐         ┌─────────────────────────────────┐
   │ axum metrics server        │         │ per-Module ALSA capture thread  │
   │ reqwest Discord client     │         │ per-Module ALSA playback thread │
   │ DB-writer dispatcher       │         │ per-Module AT commander thread  │
   │ retry timer                │         │ PJSIP internal worker threads   │
   │ graceful shutdown handler  │         └──────────────┬──────────────────┘
   │ SMS reader (per-module     │                        │ SPSC (crossbeam-queue)
   │   coroutine; AT calls via  │ ◄──── tokio mpsc ─────►│
   │   spawn_blocking proxy)    │                        ▼
   └────────────────────────────┘         ┌──────────────────────────────────┐
                                          │ pjsua-safe AudioMediaPort        │
                                          │ frame callback (PJSIP thread)    │
                                          └──────────────────────────────────┘
```

The audio frame path (ALSA capture thread → SPSC ring → AudioMediaPort callback → PJSIP) carries no locks. Backpressure manifests as ring overflow (counted in the `gsm_sip_bridge_audio_errors_total` metric, label `kind="overrun"` / `"underrun"`).

The DB-writer is a single dedicated OS thread that owns the `rusqlite::Connection`; tokio tasks send work items over a `crossbeam::channel`. This keeps SQLite writes serialized (avoiding `SQLITE_BUSY`) without making the rest of the system synchronous.
