# Contract: Configuration

**Feature**: `028-gm-tcp-reconnect`

## New section

```toml
[alerts.gm_connection_lost]
enabled = true
discord_webhook_url = "https://discord.com/api/webhooks/..."   # optional
unhealthy_sec = 300
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | **`true`** | Differs from `line_discovery_failed`, which defaults `false`. A line that is *registered* but whose signaling connection cannot be restored is unambiguously an incident, whereas a deliberately unconfigured line is not. |
| `discord_webhook_url` | string | inherits the global alerts webhook | Same semantics as every other category. |
| `unhealthy_sec` | u64 | `300` | Validated `30..=3600` via `in_range_or_warn`. |

## Touch points

Six mechanical edits, all with a working precedent in
`line_discovery_failed` (spec 027) — plus the threshold plumbing, which
`line_discovery_failed` does *not* have but `registration_loss` and
`tunnel_failure` do.

| # | File | Edit |
|---|---|---|
| 1 | `config/raw.rs:~351` | `pub gm_connection_lost: Option<RawUnhealthyCategory>` — note `RawUnhealthyCategory` (has `unhealthy_sec`), not `RawAlertCategory` |
| 2 | `config/raw.rs:~596` | `("alerts.gm_connection_lost", RawUnhealthyCategory::KEYS)` in the known-keys table |
| 3 | `config/build.rs:~490` | `gm_connection_lost: category(raw.gm_connection_lost, true)` — **`true`**, not `false` |
| 4 | `config/build.rs:~472` | `gm_connection_lost_thresholds` block, mirroring `registration_loss_thresholds` including the `30..=3600` range check |
| 5 | `config/mod.rs:~403` | `pub gm_connection_lost: CategoryAlertConfig` + `pub gm_connection_lost_thresholds: GmConnectionLostThresholds` (new struct, `unhealthy_sec: u64`, `Default` = 300) |
| 6 | `config/mod.rs:~482` | default entry — `CategoryAlertConfig::enabled()` equivalent, and the thresholds default |

Plus `alerts/mod.rs`: `AlertCategory::GmConnectionLost` variant (`:24`),
`as_str` → `"gm_connection_lost"` (`:57`), `category_config` arm (`:150`), and
the critical allowlist (`:190`).

## Unknown-key validation

`config/raw.rs`'s known-keys table is what makes a typo in `config.toml` a
warning rather than silence. Missing edit #2 means
`[alerts.gm_connection_lost]` parses but is reported as an unknown section.

## Documentation gate

`tests/test_config_docs.rs` asserts every config key is documented. The suite
**will fail** until `docs/configuration.md` gains the new section. This is the
gate working as intended — sequence the docs task before claiming the phase
green, per Constitution Principle II.

## Backward compatibility

An existing `config.toml` with no `[alerts.gm_connection_lost]` section gets
the defaults above — alerting on, 300s threshold, global webhook. Operators who
want it off must add `enabled = false` explicitly. This is a deliberate
behaviour change on upgrade and belongs in the release notes.
