# Phase 1 Data Model: Card Phone Number and Instance Identity in Alerts

All changes are additive `Option`/string fields on existing types — no new
entities, no persistence, no schema migration.

## Config entities

### `AlertsConfig` (`config/mod.rs`)
- **New**: `instance_name: Option<String>` — from `[alerts].instance_name`.
  `None` or empty ⇒ fall back to the system hostname. Trusted, displayed as-is.

### `VowifiLineOverride` (`config/mod.rs`)
- **New**: `msisdn: Option<String>` — from `[[vowifi.line]].msisdn`. Mirrors the
  existing `VolteLineOverride.msisdn`. Advertised as this line's phone number in
  alerts. Optional.

### `VolteLineOverride.msisdn` (existing)
- Reused unchanged as the VoLTE line's phone number for alerts.

## Runtime / payload entities

### `CriticalEvent` (`alerts/mod.rs`)
- **New**: `phone_number: Option<String>` — the affected card/line's number, or
  `None` when the origin cannot determine it. Rendered as `unknown` when `None`.
- Existing: `category`, `unit_id`, `description`, `at`, `kind` (unchanged).

### `DiscordClient` (`alerts/discord.rs`)
- **New**: `instance: String` — the resolved instance label (config or hostname),
  computed once when the client is built. Rendered in every embed footer.

### SMS forward path (`sms/mod.rs`, `alerts/discord.rs`)
- `record_and_forward` and `DiscordClient::forward_sms` gain a
  `phone_number: Option<&str>` argument — the receiving card/line's number.

## Derived structures

### `unit_id → msisdn` map (`metrics/ingest.rs`)
- Built once at `init_alerts` from the resolved VoLTE + VoWiFi lines
  (`unit_id`/line id → configured `msisdn`). Read-only after init. Used by
  `dispatch_transition` for daemon-detected categories (RegistrationLoss,
  TunnelFailure, GmConnectionLost). Missing id ⇒ `None` ⇒ `unknown`.

## Field resolution rules (per FR-001…FR-008)

| Origin | Phone source | Instance source |
|--------|--------------|-----------------|
| CS SMS (pool) | `SlotState.phone_number` (AT+CNUM) | client `instance` |
| CS ModuleLifecycle (worker) | cached AT+CNUM | client `instance` |
| CS ModuleLifecycle (pool) | `SlotState.phone_number` | client `instance` |
| VoWiFi SMS | configured line `msisdn` | client `instance` |
| RegistrationLoss / TunnelFailure / GmConnectionLost | `unit_id → msisdn` map | client `instance` |
| LineDiscoveryFailed | `resolution.lines` `msisdn` | client `instance` |

**Rendering (both embeds)**: `Phone` field = value if present & non-empty, else
literal `unknown`; `footer.text` = `"gsm-sip-bridge · {instance}"`.

## Validation rules

- No validation of `instance_name` or `msisdn` beyond non-empty (Assumptions).
- No real subscriber identifiers in fixtures/examples — use `+919000000000`
  placeholders (CLAUDE.md; FR-010).
