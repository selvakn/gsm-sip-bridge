# Contract: Per-line agent topology and status/observability surface

Extends `specs/011-vowifi-sip-bridge/contracts/agent-control-protocol.md` (unchanged wire format)
with the process/threading topology multi-line adds (research.md item 6).

## `vowifi-ims-agent` (Agent A)

```
gsm-sip-bridge --config <path> vowifi-ims-agent --line <index>
```

- New required `--line <index>` flag. Loads `LineResolution.lines[index].config` (data-model.md)
  instead of the single global `config.vowifi` — everything else about `ims::agent::run` is
  unchanged; it already takes `&VowifiConfig` and knows nothing about how many lines exist.
- Still launched via `ip netns exec ims{index} ...` by `entrypoint.sh`, one process per line, one
  supervised restart loop per line (mirrors today's single supervisor loop, replicated N times).
- `VETH_SIP_PORT` / `AGENT_A_STATUS_PORT` constants are unchanged — each line's namespace gives
  each Agent A instance its own port space for free.

## `vowifi-sip-agent` (Agent B)

```
gsm-sip-bridge --config <path> vowifi-sip-agent
```

- No new flag — still exactly one process. Reads the same `LineResolution` to learn how many
  control-channel listeners to start (`LINE_COUNT`, each line's `veth_peer_addr`/`control_port`).
- One PJSIP `Endpoint`/`Account` (unchanged — one registration to the PBX, per the spec's
  Assumptions).
- One accept-loop thread per line, each bound to that line's `(veth_peer_addr, control_port)`.
  Each thread closes over that line's `card_id`; every `tracing` call, `RecentCalls` entry, and
  `sms::record_and_forward` call inside that thread's call path is tagged with it (FR-017).
  `RecentCalls` becomes `HashMap<card_id, RecentCalls>` behind one `Mutex`, replacing today's
  single `Arc<Mutex<RecentCalls>>`.
- `ControlMessage` wire format is **unchanged** — no `card_id` field added to `IncomingCall`
  (research.md item 6): attribution comes from which listener accepted the connection, not from
  message content.

## `vowifi-status`

```
gsm-sip-bridge --config <path> vowifi-status
```

- Iterates every line in `LineResolution.lines`, printing each line's card id, Agent A
  registration state (queried at `veth_local_addr:AGENT_A_STATUS_PORT` inside that line — reached
  from the default netns via the veth link, same as today's single query), and Agent B's
  per-line recent-call history (`CallHistoryReply`, now requested with a line selector — see
  below) — FR-018.
- Exit code: failure (non-zero) if **any** line's queries both fail; success if at least one
  line reports (mirrors today's per-query independent-failure reporting, generalized: one line
  being unreachable must not hide the others' status, matching User Story 3's acceptance
  scenario 1).

### `ControlMessage::StatusQuery` line selection

Agent B already answers `StatusQuery` with whatever its listener has (today: the one and only
`RecentCalls`). Since each line has its own listener/port, `vowifi-status` simply connects to that
line's own `(veth_peer_addr, control_port)` to ask — no protocol change needed here either; the
existing `StatusQuery`/`CallHistoryReply` pair is reused verbatim, once per line.

## Health check

- The container health check (referenced in FR-019, "must consider every line, not only the
  first") queries `vowifi-status`'s exit code (or an equivalent lighter-weight per-line check) and
  reports VoWiFi as degraded/down only when it should — not fatal to the container per the spec's
  clarification (zero usable lines degrades, doesn't crash).

## Metrics (FR-017, new surface — no prior VoWiFi metrics existed)

New `IntGaugeVec`/`IntCounterVec` families, all labeled `card_id`:
- `vowifi_tunnel_up{card_id}` (0/1)
- `vowifi_registration_state{card_id}` (enum as label value or separate gauge per state — follow
  the existing `metrics/mod.rs` `register_gauge_vec!` convention used elsewhere in the file)
- `vowifi_calls_total{card_id, outcome}`

Populated by Agent A (tunnel/registration) and Agent B (call outcomes), each already knowing its
own `card_id` per the topology above.
