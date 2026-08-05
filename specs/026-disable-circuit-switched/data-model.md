# Phase 1 Data Model: Disable Circuit-Switched Handling

This feature adds one configuration value and one metric. There is no persistent state, no schema migration, and no wire-format change.

## Configuration entities

### `RawCs` — parsed shape (`config/raw.rs`)

Declared with the `section!` macro so its key list is generated alongside the serde struct.

| Field | Type | TOML key | Default | Notes |
|---|---|---|---|---|
| `enabled` | `bool` | `[cs].enabled` | **`true`** | Hand-written `Default`, not derived — see below |

```rust
section! {
    /// Exactly the `[cs]` keys.
    pub struct RawCs {
        pub enabled: bool,
    }
}

// NOT #[derive(Default)]. The `section!` macro applies `#[serde(default)]`,
// so an absent `[cs]` section falls back to this impl. Deriving it would
// yield `false` and silently disable circuit switching for every existing
// deployment on upgrade — the regression User Story 2 exists to prevent.
impl Default for RawCs {
    fn default() -> Self {
        Self { enabled: true }
    }
}
```

**Registration requirements** (each one is a hard failure if missed):
- Field `pub cs: RawCs` added to `RawConfig`.
- `("cs", RawCs::KEYS)` added to `section_key_lists()` — otherwise `collect_unknown_keys` rejects `[cs]` as an unknown section (FR-002a).
- A `| \`enabled\`` row under a `### \`[cs]\`` heading in `docs/configuration.md`, and a `[cs]` block in `config.toml.example` — otherwise `tests/test_config_docs.rs` fails.

### `CsConfig` — runtime shape (`config/mod.rs`)

```rust
#[derive(Clone, Debug)]
pub struct CsConfig {
    pub enabled: bool,
}
```

Added to `AppConfig` as `pub cs: CsConfig`, built by `build_cs` in `config/build.rs` and wired into `build()`. No validation beyond the boolean — there is no invalid value, and FR-003 requires every combination with `[vowifi]`/`[volte]` to be accepted.

### Validation rules

| Rule | Source | Behaviour |
|---|---|---|
| Any combination of `[cs]`, `[vowifi]`, `[volte]` is valid | FR-003 | No cross-section rejection. The pre-existing "VoWiFi and VoLTE not both enabled" rule in `supervise::orchestrate` is untouched. |
| Circuit-switched tuning stays valid when disabled | FR-025 | `[modules]`, `[resilience]`, `[scheduled_restart]`, `[modem_audio]` parse and validate exactly as before; they simply have no effect. |
| No call path enabled is valid | FR-023 | Warning, not error. |

## Metrics entity

### `gsm_sip_bridge_cs_enabled` — new gauge

| Property | Value |
|---|---|
| Type | `Gauge` (no labels) |
| Value | `1` when the circuit-switched path is enabled, `0` when disabled |
| Presence | **Both states** — this is the whole point (FR-021b) |
| Set from | `commands/daemon.rs`, unconditionally, before the pool is gated |

Presence in both states is what lets a consumer distinguish "deliberately disabled" (`cs_enabled 0`) from "process down or scrape failing" (metric absent entirely). A gauge that only appeared when disabled would be indistinguishable from a dead daemon.

### Circuit-switched series that disappear when disabled

Not suppressed explicitly — these are `once_cell::sync::Lazy` statics that register on first dereference, and every dereference is inside `modules/mod.rs`:

| Series | Touch site |
|---|---|
| `gsm_sip_bridge_modules_active` | `modules/mod.rs:460, 536, 687` |
| `gsm_sip_bridge_modules_failed` | `modules/mod.rs:466, 537, 688` |
| `gsm_sip_bridge_module_init_total` | `modules/mod.rs:373, 425, 575` |
| `gsm_sip_bridge_module_retries_total` | `modules/mod.rs:570` |
| `gsm_sip_bridge_scheduled_restart_total` | `modules/mod.rs:1065` |

With `CardPool` not running, none is dereferenced, so none is registered (FR-021a). This is a property of the current code, not a guarantee of the metrics library's API surface — it must be pinned by test, because it breaks silently if any non-CS path ever touches one of these statics.

`gsm_sip_bridge_outbound_attempts_total` is **not** in this list: `modules/mod.rs` increments it for circuit-switched attempts, but `metrics::ingest` also increments it from VoWiFi/VoLTE agent reports, so it stays registered and keeps working.

## State transitions

None. `[cs].enabled` is read once at process start and never changes at runtime (explicitly out of scope). Flipping it requires a restart, consistent with every other section.

## Relationships

```text
config.toml [cs].enabled
        │
        ├─> daemon.rs ──> CardPool spawned?           FR-005..FR-009, FR-021a
        │            └──> disabled responder spawned? FR-019, FR-020
        │            └──> CS_ENABLED gauge set        FR-021b
        │            └──> startup warnings            FR-023, FR-024
        │            └──> CLI-override conflict       FR-026
        │
        ├─> sip/mod.rs owns_sip_side                  FR-009a, FR-009b
        │
        ├─> discover.rs ──> RoleAssignment::from_probed(cs_enabled)
        │                                             FR-010a, FR-010b, FR-010c
        │
        └─> healthcheck.rs ──> report disabled        FR-018
```
