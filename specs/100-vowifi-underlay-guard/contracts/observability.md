# Contract: state, events, metrics, alert text and logs

## State: `AgentState.uplink`

Supervisor only. Reported on change, through `TunnelSignal` / `sync_tunnel_signal`, like `child_sa_up`, `tunnel_flapping` and `ike_sa_loss_cause`.

```json
{"uplink":"up"}
{"uplink":"no_route"}
{"uplink":"no_response"}
```

- `skip_serializing_if = "Option::is_none"`, `default`. Absent means "not reported" (an older supervisor, the swu engine, or the guard off), and ingest leaves its state untouched (data-model.md §1 of spec 022).
- With the guard off the supervisor never sends the field.

## Events (`ObservedEvent`, snake_case tag)

```json
{"event":"uplink_hold_started","cause":"no_route"}
{"event":"uplink_hold_ended","cause":"no_response","held_secs":312,"ended_by":"restored"}
```

| Field | Values |
|---|---|
| `cause` | `no_route`, `no_response` |
| `held_secs` | integer seconds, ≥0 |
| `ended_by` | `restored`, `max_hold`, `shutdown` |

All are closed enums, so label cardinality is bounded (spec 022 FR-014).

`TunnelRecovered.reason` gains `ike_sa_negotiation_stuck`. `TunnelRecovered.dead_peer` gains `uplink`.

## Metrics (daemon registry, so a supervisor restart does not rewind them)

| Name | Type | Labels | Updated by |
|---|---|---|---|
| `gsm_sip_bridge_vowifi_uplink_holds_total` | counter | `module`, `cause` | `+1` on `uplink_hold_started` |
| `gsm_sip_bridge_vowifi_uplink_held_seconds_total` | counter | `module`, `cause` | `+held_secs` on `uplink_hold_ended` |
| `gsm_sip_bridge_vowifi_uplink_held` | gauge | `module` | `1` while `AgentState.uplink` is `no_route`/`no_response`, `0` on `up`, untouched when absent |
| `gsm_sip_bridge_vowifi_tunnel_recoveries_total` (existing) | counter | gains `reason="ike_sa_negotiation_stuck"` | `TunnelRecovered` |
| `gsm_sip_bridge_vowifi_dead_peer_losses_total` (existing) | counter | gains `verdict="uplink"` | `TunnelRecovered.dead_peer` |

Only new label values are added to the two existing counters. The provisioned Grafana dashboards have no VoWiFi tunnel panels today (096–098 added none either), so none are added here.

## Alert text (`tunnel_failure_description`)

No new alert type and no new threshold (FR-023). When the supervisor record's last `uplink` is `no_route` or `no_response` at the moment the tunnel-failure alert fires, this suffix is appended to the existing text:

```
; uplink down (no route), recovery on hold
; uplink down (no response), recovery on hold
```

Example:

```
VoWiFi line 0's tunnel non-established for over 300s (last IKE_SA loss: dead-peer timeout); uplink down (no route), recovery on hold
```

When `uplink` is `up` or absent, the text is byte-for-byte what it is today. The "Recovered" alert text is unchanged.

## Log lines (supervisor stdout)

| When | Line |
|---|---|
| Startup | see contracts/config.md |
| Hold starts | `[supervise] line N: uplink down (no_route: no route to ePDG 203.0.113.1); holding recovery` |
| | `[supervise] line N: uplink down (no_response: 2 retransmits unanswered, last life 74s ago); holding recovery` |
| Hold ends | `[supervise] line N: uplink restored after 5m12s (no_route); held: PcscfPoolExhausted, ChildSaStuck` |
| | `[supervise] line N: uplink hold reached 15m (no_response); running one recovery` |
| Liveness check sent | `[supervise] line N: checking the ePDG is alive before PcscfPoolExhausted (DPD sent)` |
| Liveness answered | `[supervise] line N: ePDG answered; proceeding with PcscfPoolExhausted` |
| Held initiation (FR-010a) | `[supervise] line N: no IKE_SA while held; initiating once and leaving retries to charon` |
| Half-open, waiting | `[supervise] line N: ims0 IKE_SA #5 still negotiating; leaving it to charon` (once per `#N`) |
| Half-open, stuck | `[supervise] line N: ims0 IKE_SA #5 negotiation stuck (no IKE activity for 214s); re-initiating` |

Recoveries skipped during a hold are not logged one per tick; they are summarised in the "Hold ends" line (FR-020).
