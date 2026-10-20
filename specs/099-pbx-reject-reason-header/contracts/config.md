# Contract: `[sip].reject_statuses`

```toml
[sip]
# Extra PBX final statuses to treat as "the person rejected the call"
# (carrier gets 486 Busy Here). Added to the built-in 486, 600, 603.
# A Reason: Q.850;cause=21 (or 17) already counts, whatever the status.
reject_statuses = [403]
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `reject_statuses` | list of integers | absent (defaults only) | Each value 400-699. Adds to the defaults; cannot remove them. VoWiFi/VoLTE only. Global: all lines. |

Errors (startup fails, nothing is started):
- `[sip].reject_statuses: 200 is not a failure status (expected 400-699)`
- `[sip].reject_statuses: expected integers`

Docs: add a row to the `[sip]` table in `docs/configuration.md` and a
FreePBX example (a PBX that rejects with a bare `403`). Mention that 403 is
deliberately not a default: it can also mean the bridge is not authorised.
