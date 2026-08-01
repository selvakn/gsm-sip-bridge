# Contract: `[sip_server]` configuration schema

**Feature**: 024-sip-server-mode

This is the operator-facing contract. It is enforced twice: by
`#[serde(deny_unknown_fields)]` on the raw structs, and by
`config/build.rs::build_sip_server` plus the cross-section rules in `build()`.

`gsm-sip-bridge/tests/test_config_docs.rs` mechanically checks that every key
here appears in `docs/configuration.md` and that `config.toml.example` sets only
keys that exist.

---

## Shape

```toml
[sip_server]
enabled            = false          # bool
listen_addr        = "0.0.0.0"      # string, must parse as an IP address
listen_port        = 5060           # u16
realm              = "gsm-sip-bridge"  # string, shown in the phone's auth prompt
ring_aor           = "1001"         # string, must match an account username
min_expires        = 60             # u32, seconds
max_expires        = 3600           # u32, seconds
nonce_lifetime_sec = 120            # u64, seconds

[[sip_server.account]]
username = "1001"
password = "env:PHONE_1001_PASSWORD"   # or a literal

[[sip_server.account]]
username = "1002"
password = "env:PHONE_1002_PASSWORD"
```

## Defaults

| Key | Default | Notes |
|---|---|---|
| `enabled` | `false` | The mode is opt-in (FR-001). |
| `listen_addr` | `"0.0.0.0"` | All interfaces. |
| `listen_port` | `5060` | The port IP phones default to. |
| `realm` | `"gsm-sip-bridge"` | Appears in the handset's credential prompt. |
| `ring_aor` | `""` | No default is possible; required when enabled. |
| `min_expires` | `60` | Below this, a phone is told `423 Interval Too Brief`. |
| `max_expires` | `3600` | Above this, the request is clamped. |
| `nonce_lifetime_sec` | `120` | After this, a challenge is stale and the phone silently retries. |
| `sip_server.account` | `[]` | At least one entry required when enabled. |

## Validation

Applied only when `enabled = true`. A disabled section is parsed structurally and
otherwise ignored, matching `[vowifi]` and `[volte]`.

| # | Rule | Error message shape |
|---|---|---|
| 1 | `realm` non-empty | `required field sip_server.realm is missing` |
| 2 | `ring_aor` non-empty | `required field sip_server.ring_aor is missing` |
| 3 | `listen_port` in `1..=65535` | `field sip_server.listen_port must be in 1..=65535, got {v}` |
| 4 | `listen_addr` parses as `IpAddr` | `field sip_server.listen_addr must be an IP address, got {v:?}` |
| 5 | at least one account | `sip_server: at least one [[sip_server.account]] is required when enabled = true` |
| 6 | account username non-empty | `sip_server.account[{i}]: username must not be empty` |
| 7 | account password non-empty | `sip_server.account[{i}]: password must not be empty` |
| 8 | usernames unique | `sip_server.account[{i}]: duplicate username {u:?} (also used by account[{j}])` |
| 9 | `ring_aor` matches an account | `sip_server.ring_aor {v:?} matches no configured account (available: {list})` |
| 10 | `min_expires <= max_expires` | `sip_server.min_expires must not exceed sip_server.max_expires` |
| 11 | `min_expires`, `max_expires` in `30..=86400` | `field sip_server.{k} must be in 30..=86400, got {v}` |
| 12 | `nonce_lifetime_sec` in `10..=3600` | `field sip_server.nonce_lifetime_sec must be in 10..=3600, got {v}` |

## Cross-section rules

Checked in `build()` once every section exists.

| # | Rule | Rationale |
|---|---|---|
| 13 | `[sip].server` must be unset | Meaningless — there is no PBX to register to (R-010). |
| 14 | `[sip].username` must be unset | As above. |
| 15 | `[sip].password` must be unset | As above. |
| 16 | `[bridge].sip_destination` must be empty | The destination is `sip_server.ring_aor`. |
| 17 | `[sip].local_port != [sip_server].listen_port` | Two SIP endpoints cannot bind one UDP port. Message names the remedy. |
| 18 | `[sip].transport == "udp"` | The registrar is UDP-only in this version. |
| 19 | `listen_port != 5072` when `[vowifi].enabled` | 5072 is `vowifi-sip-agent`'s own SIP port, in the same (host) namespace as the registrar. A fixed constant an operator cannot move, so the message says to move `listen_port`. |
| 20 | `listen_port != 5073` when `[volte].enabled` **and** `bridge_inbound` | Likewise the VoLTE telephony side's SIP port. |
| 21 | `listen_port` outside `5074..=5074+4×max_lines-1` when the VoLTE telephony side runs | Its per-line loopback ports are strided by 4 and the line count is discovered at runtime, so the whole span is reserved rather than only the first three. |
| 22 | not (`[vowifi].enabled` and VoLTE telephony running) | Both would host a registrar on one port and only one can bind it. |

Rules 19–22 exist because these ports are otherwise discovered at `bind`, as an
`EADDRINUSE` inside a supervised child that then restarts while that carrier path
silently carries no calls (PR #21 review).

**Agent A's 5070/5071 are deliberately absent** from rules 19–21: those are bound
on a veth address inside each line's own `ims` namespace, so they cannot collide
with the registrar's host-namespace socket.

Every one of these is gated on the subsystem that owns the port actually
*running* — `[volte].bridge_inbound` without `[volte].enabled` spawns nothing
(`supervise::orchestrate` starts VoLTE on `enabled`), so on its own it reserves
nothing. A deployment with no agents may use 5072 or 5073 freely.

Rules 13–15 also **relax** the existing requirement that those three keys be
present: with the mode enabled they are forbidden rather than mandatory. This is
why `build_sip` takes `&SipServerConfig`.

### Rule 17's message

Both keys default to `5060`, so every operator enabling the mode hits this once:

```
configuration error: [sip].local_port and [sip_server].listen_port are both 5060,
but they are two different SIP endpoints and cannot share one UDP port.
[sip_server].listen_port is the port your IP phones register to — leave it at
5060 and move the bridge's own calling port instead, e.g. [sip].local_port = 5062.
```

## Secrets

`sip_server.account.password` is added to `SECRET_KEY_PATHS`
(`gsm-sip-bridge/src/config/env.rs:23`). `env::resolve_in_place` assigns array
elements the array's own path, so this single entry covers every account. A
failed `env:` lookup is then reported as a missing secret rather than a missing
plain string.

Passwords are stored as `Secret<String>` and are redacted from `Debug` output.

## Registration points

Adding this section touches the three places `config/raw.rs` documents:

1. `RawConfig` — `pub sip_server: RawSipServer`
2. `section_key_lists()` — **two** entries: `("sip_server", RawSipServer::KEYS)`
   and `("sip_server.account", RawSipServerAccount::KEYS)`
3. `AppConfig` (`config/mod.rs`) and `build()` (`config/build.rs`)

The field is named `account`, not `accounts`, because the field name *is* the
TOML key — the same rationale recorded for `RawVowifi::line`.
