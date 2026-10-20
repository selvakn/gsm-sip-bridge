# Contract: `[vowifi].underlay_guard`

| Key | Type | Default | Scope |
|---|---|---|---|
| `underlay_guard` | boolean | `true` | `[vowifi]`, per deployment, not per line |

- **`true`:** the underlay guard runs for every strongSwan line (spec FR-001–FR-013, FR-018–FR-022).
- **`false`:** supervision matches the pre-feature behaviour exactly, except for the half-open fix (FR-014–FR-017), which always runs (FR-025).
- **Invalid value** (anything that is not a TOML boolean): config loading fails with an error naming `vowifi.underlay_guard`.
- **swu engine:** accepted and ignored. Startup says so once.
- **When it applies:** read at startup. A change takes effect when the container is recreated, like every other `[vowifi]` deployment setting.

## Startup log (exactly once)

Logged with `tracing::info!` from `underlay_guard_log_line(&VowifiConfig)`, beside 098's `dpd_log_line`:

```
underlay guard on
underlay guard off ([vowifi].underlay_guard = false)
underlay guard ignored by the swu engine
```

## Documentation

- `docs/configuration.md`, `[vowifi]` table: one row in the 098 `dpd_delay_sec` style, saying what is held, what never is, and how to turn it off.
- `config.toml.example`: commented `# underlay_guard = true` under the `dpd_delay_sec` line.
- `tests/test_config_docs.rs` must pass, since it rejects an undocumented key (FR-025a).
