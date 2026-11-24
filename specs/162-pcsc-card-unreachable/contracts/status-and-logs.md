# Contract: Status and Log Output for a Card Outage

The externally visible surface this feature changes. There is no new CLI flag, config key, metric or alert (Clarification Q2). Everything else is internal.

## `vowifi-status` registration block

Extends `specs/123-ims-registration-refresh/contracts/status-output.md`. Only the set of values of an existing line changes:

```text
    last_refresh: awaiting-card <timestamp>   # ok <ts> | rejected <status> <ts> | timeout <ts> | error <ts> | awaiting-card <ts> | none
```

- `awaiting-card` is set when a card-reader line gives up a challenge because its card is unreachable and keeps the registration (US2). The next outcome replaces it: `ok` after the fresh in-place refresh, or the outcome of the full registration.
- `next_refresh` is unchanged. While waiting it shows the refresh's original due time, which is in the past, as it already does during a renewal backoff.
- `last_failure` (an existing line), after a full registration fails because the card is unreachable: `last_failure: <ts> IMS error: SIM card unreachable: <PC/SC detail>`. The detail is the existing pcsc-lite text, for example `PC/SC transmit failed: The smart card has been reset, so any shared state information is invalid`.
- `docs/observability.md` lists the full value set of `last_refresh`, including the existing but undocumented `error`.

## Error text

| Condition | Before | After |
|---|---|---|
| A mapped PC/SC failure (research D1) | `IMS error: PC/SC transmit failed: <pcsc text>` | `IMS error: SIM card unreachable: PC/SC transmit failed: <pcsc text>` |
| No reader holds this line's IMSI | `IMS error: no PC/SC reader has a card matching IMSI …` | `IMS error: SIM card unreachable: no PC/SC reader has a card matching IMSI …` |
| Any other PC/SC failure | unchanged | unchanged |
| The card's own answer (`AUTHENTICATE failed: SW=…`) | unchanged | unchanged |
| Every modem-line error | unchanged | unchanged |

The `IMSI …` part was already present before this feature. The text matches `vowifi-plmn`, the agent start-up PLMN read and every registration path. The classification by `map_registration_error` (`rejected`) and `classify_register_failure` (`NotPcscf`) is unchanged (research D1).

## Log lines (agent log, card-reader lines only)

| Event | Level | Message | Fields |
|---|---|---|---|
| Same challenge put to the card again | INFO | `the SIM is out of reach; asking it the same challenge again` | `attempt`, `error` |
| Window over, registration kept | WARN | `the SIM stayed out of reach; keeping the registration and waiting for the card` | `window_secs`, `expires_in_secs`, `error` (the last card error, or `the SIM did not answer in time`) |
| Card answers a local check | INFO | `the SIM is reachable again; refreshing the registration in place` | `waited_secs` |
| Margin or lapse while waiting, idle | WARN | `the SIM is still out of reach and the registration is about to end; registering in full` | `reason` = `margin` \| `lapsed` |
| Full registration failed, card unreachable | WARN | `the SIM is out of reach; retrying the registration without counting toward the restart budget` | `retry_in_secs`, `error` |

No line carries an IMSI, IMEI or MSISDN. The existing `connected to PC/SC reader` and `read IMSI from SIM` lines from `open_card` are unchanged.

## Compatibility

The status reply is agent-to-CLI inside one image. The value is a free string, so an older CLI renders `awaiting-card` as is. Tests that render `last_refresh` (`vowifi/mod.rs` tests) gain an `awaiting-card` case.
