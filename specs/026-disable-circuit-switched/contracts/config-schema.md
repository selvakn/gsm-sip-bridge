# Contract: `[cs]` configuration section

**Feature**: 026-disable-circuit-switched
**Satisfies**: FR-001, FR-002, FR-002a, FR-003, FR-004, FR-004a, FR-025, FR-027

## Schema

```toml
[cs]
enabled = true    # boolean, default true
```

| Key | Type | Default | Meaning |
|---|---|---|---|
| `enabled` | boolean | `true` | When `false`, the circuit-switched call path does not run: no modem discovery, no periodic rescan, no AT traffic, no circuit-switched calls. |

No other keys. The section exists solely to hold this flag.

## Default behaviour

| Config | Effective value |
|---|---|
| No `[cs]` section at all | `true` |
| `[cs]` present, `enabled` absent | `true` |
| `[cs].enabled = true` | `true` |
| `[cs].enabled = false` | `false` |

The first two rows are the backward-compatibility contract: a configuration written before this feature must behave exactly as it did (FR-002). This is the single most important assertion in the feature's test suite.

## Validation

- **Accepted**: every combination of `[cs].enabled`, `[vowifi].enabled`, and `[volte].enabled`, including all-false and all-true (FR-003). No cross-section rejection is introduced.
- **Rejected**: any key other than `enabled` inside `[cs]`, reported by the existing unknown-key walk as `cs.<key>`.
- **Unaffected**: the pre-existing rule that `[vowifi].enabled` and `[volte].enabled` must not both be true. That check lives in `supervise::orchestrate` and this feature does not touch it.
- **Inert but valid**: `[modules]`, `[resilience]`, `[scheduled_restart]`, and `[modem_audio]` keep parsing and validating normally when `[cs].enabled = false`; they simply have no effect (FR-025). An operator who flips the flag back on finds their tuning intact.

## Relationship to `[modules]`

`[modules]` keeps its current name and keys (FR-004a). It holds circuit-switched card-pool tuning — `retry_interval_sec`, `max_concurrent` — which `[cs].enabled` governs. The configuration reference must cross-reference the two in both directions, since an operator looking for the on/off switch may reasonably start in either place.

## Startup reporting

The effective value is logged at startup at a level visible without enabling debug logging (FR-004). Two further warnings are conditional:

| Condition | Message intent | Requirement |
|---|---|---|
| `[cs].enabled = false` and neither VoWiFi nor VoLTE enabled | No call path is active; this process serves metrics and stored history only, and will establish no telephone-facing registration | FR-023 |
| `[cs].enabled = false`, `[sms].enabled = true`, and no VoWiFi/VoLTE line configured | Message forwarding has no active source | FR-024 |

Neither is fatal.

## Documentation coupling

`tests/test_config_docs.rs` asserts that every key in `section_key_lists()` appears as a `` | `key` `` table row in `docs/configuration.md`, and cross-checks `config.toml.example`. Adding `[cs]` therefore **requires**, in the same change:

1. `("cs", RawCs::KEYS)` in `section_key_lists()`
2. A `### \`[cs]\`` section with an `` | `enabled` `` row in `docs/configuration.md`
3. A `[cs]` block in `config.toml.example`

Omitting any of the three fails `make test`.
