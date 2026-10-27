# Contract: `vowifi-status` / `volte-status` registration block

The only external interface this feature changes. Both CLIs render a `registration:` block from the same status reply (`vowifi/control.rs`, rendered by `vowifi/mod.rs` and `volte/bridge.rs`). Existing lines are unchanged; the new lines are added directly under `expires_at`.

## Added lines

```text
  registration:
    state: registered
    registered_at: <timestamp>
    expires_at: <timestamp>
    refresh_mode: in-place            # in-place | fallback
    last_refresh: ok <timestamp>      # ok <ts> | rejected <status> <ts> | timeout <ts> | none
    next_refresh: <timestamp>         # or "none" when not registered
    refresh_rejections: 0             # consecutive rejections of in-place refresh
    gm_connection: up
    ...
```

## Rules

- Timestamps use the existing `format_unix` rendering.
- `refresh_mode` is `fallback` from the moment the second consecutive rejection is recorded until the next successful in-place refresh.
- While a recovery is held for a call, the existing deferred-maintenance indicator reads `registration-recovery` (new value of the existing `Maintenance::as_str`), so a deferral reads as deliberate. An ordinary refresh is never shown as deferred.
- No credentials, nonces, SPIs or subscriber identifiers appear in any added line.
- A status read from an older agent (no new fields) renders `refresh_mode: unknown`, `last_refresh: none`, `next_refresh: none`, `refresh_rejections: 0`.

## Compatibility

The status reply is an internal control message between the agent and the CLI inside one container image, so both change together. Any test that parses the rendered text (`vowifi/control.rs` tests, `tests/test_vowifi_*.rs`) must be updated in the same commit that adds the fields.
