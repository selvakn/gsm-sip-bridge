# Contract: Operator-Visible Behaviour During a Card-Reader SIM Outage

What this feature changes that an operator, or another process, can see. Paths are relative to `gsm-sip-bridge/src`.

## Supervisor log (`docker logs`, stdout)

| Event | Line |
|---|---|
| Outage begins | `[supervise] line N: SIM card out of reach; agent restarts and tunnel attempts are slowed until it is back` |
| Outage ends | `[supervise] line N: SIM card reachable again after Ns; resuming` |
| Paced agent restart | `[supervise] line N: vowifi-ims-agent exited (SIM card out of reach); restarting in Ns` (today: `exited; restarting in 5s` every time) |
| Paced tunnel initiation | `[supervise] line N: SIM card still out of reach; trying the tunnel once (next try in Ns)` |
| Held tunnel rebuild | nothing at the default level |

No line carries an IMSI, IMEI or MSISDN.

## Agent log (`/tmp/ims-agent-N.out`)

When a card-reader agent exits because its card cannot be reached, the last ERROR line before the exit contains `the SIM card is out of reach; exiting` (`CARD_UNREACHABLE_MARKER`). That line is the contract with the supervisor; tests pin both sides.

## `vowifi-plmn` exit status

| Outcome | Before | After |
|---|---|---|
| PLMN read | 0, stdout `MCC MNC` | unchanged |
| The card cannot be reached (`BridgeError::CardUnreachable`: no reader, no reader holding that IMSI, reset, removed, pcscd down…) | 1 | **3** (`EXIT_CARD_UNREACHABLE`) |
| Any other error | 1 | 1 |

The stderr text is unchanged. The supervisor's start-up MCC/MNC read treats any non-zero status as a failed attempt, as before.

## Tunnel-failure alert text (web UI timeline and Discord)

`tunnel_failure_description` appends one suffix while the line is held for its card:

```text
VoWiFi line's tunnel non-established for over 300s; SIM card out of reach, tunnel attempts slowed until it is back
VoWiFi line's tunnel non-established for over 300s (last IKE_SA loss: dead-peer); SIM card out of reach, tunnel attempts slowed until it is back
VoWiFi line's tunnel non-established for over 300s; uplink down (no route), recovery on hold; SIM card out of reach, tunnel attempts slowed until it is back
```

- Without a card hold, every text is byte-identical to today's.
- The recovery text (`… tunnel re-established`), the thresholds, toggles, raise and recovery are unchanged.
- No new alert category.

## Agent → daemon report (`control/protocol.rs::AgentState`)

`card_hold: Option<bool>` is new and optional, and only the supervisor sets it. It is omitted from JSON when `None`. Old payloads still parse, and an older daemon ignores the field. This is the same pattern as `uplink`.

## Unchanged

- `vowifi-status` output, including the `uplink:` line.
- The `uplink` status file.
- Every metric, including `VOWIFI_UPLINK_HELD`.
- Modem-backed and VoLTE lines' logs, alerts and recovery.
