# Contract: Agent → Daemon Observability Protocol

**Transport**: the existing control Unix socket, `[control].socket_path`
(default `/tmp/gsm-sip-bridge.sock`).
**Framing**: one newline-terminated JSON object per message — identical to
`control::protocol::read_cmd` / `write_resp`, which this reuses verbatim.
**Shape**: one-shot. Connect → write one `Observe` → read one response → close.
No session, no keepalive, no ordering guarantee between connections.

Agent A reaches this socket from inside the `ims` network namespace without any
routing: Unix domain sockets are not network-namespaced, and `ip netns exec`
leaves the socket's path visible in the shared mount tree.

---

## Request

`ControlCmd` gains one variant. Existing variants are untouched.

```json
{
  "cmd": "observe",
  "report": {
    "agent": "ims",
    "module_id": "ec20-A1B2C3",
    "state": {
      "active_calls": 1,
      "registered": true,
      "tunnel_up": true
    },
    "events": [
      { "event": "call_completed", "status": "answered", "duration_seconds": 42.5 },
      { "event": "registration_attempt", "status": "success" }
    ],
    "dropped": 0
  }
}
```

A heartbeat with nothing to report is the same message with `"events": []`.

### Fields

| Field | Required | Type | Notes |
|---|---|---|---|
| `agent` | yes | `"ims"` \| `"sip"` | Liveness is tracked per agent kind |
| `module_id` | yes | string | Label applied to every metric this report feeds |
| `state` | yes | object | Absolute gauge values; presence is what makes this a heartbeat |
| `state.active_calls` | no | integer ≥ 0 | Omitted by agents that do not own call state |
| `state.registered` | no | boolean | Omitted ⇒ this agent does not report it (≠ `false`) |
| `state.tunnel_up` | no | boolean | Same omission semantics |
| `state.pbx_registered` | no | boolean | Agent B only |
| `events` | yes | array | Counter deltas since the previous **successful** send; may be empty |
| `dropped` | yes | integer ≥ 0 | Reports this agent discarded on overflow since its last successful send |

### Event objects

Discriminated on `event`, matching the `#[serde(tag = "event", rename_all = "snake_case")]`
convention already used by `vowifi::control::ControlMessage`.

| `event` | Additional fields |
|---|---|
| `call_completed` | `status`: `answered` \| `missed` \| `failed`; `duration_seconds`: number |
| `pbx_leg_completed` | `outcome`: `success` \| `failed` |
| `bridge_failed` | `reason`: `bridge_setup_failed` \| `ring_timeout` \| `caller_cancelled` \| `pbx_declined` \| `agent_unreachable` |
| `sms_received` | — |
| `sms_forwarded` | `outcome`: `sent` \| `failed` |
| `registration_attempt` | `status`: `success` \| `auth_failed` \| `rejected` \| `timeout` |

Every enumerated value above is closed. A value outside the set is a parse error,
and the daemon rejects the whole report rather than minting an unbounded label
(FR-014).

---

## Response

Reuses the existing `ControlResp`:

```json
{"status":"ok"}
```

on success, or `{"status":"error","error":"..."}` if the report could not be
parsed or applied.

**The agent does not act on the response beyond delivery accounting.** A
successful write followed by `ok` means the report is delivered and may be
dropped from the buffer. Any error — connect failure, write failure, parse
rejection, or a closed socket — means the report stays buffered for retry, except
for a parse rejection, which is a permanent failure and is discarded immediately
(retrying malformed data forever would wedge the queue behind a poison message).

---

## Delivery semantics

- **At-most-once per report, with retry.** A report that is written but whose
  response is lost will be retried, so a counter delta can in principle be
  applied twice. This is accepted: the window is a torn connection during the
  daemon's own restart, and the spec's loss policy is explicitly best-effort. It
  is bounded and far smaller than the loss it replaces.
- **No ordering guarantee** between reports. Counter deltas commute, and gauge
  state is absolute-and-latest-wins, so out-of-order arrival is harmless. The
  daemon applies gauges unconditionally rather than trying to detect staleness.
- **Buffer bound**: 1024 reports per agent, discarding oldest-first. See
  research.md § R4.

---

## Routing on the daemon side

`control::server::handle_connection` short-circuits `Observe` **before** the
`cmd_tx` send: the report is applied to the registry directly and answered `ok`.
It never reaches `CardPool`. This keeps a burst of call events off the pool's
mailbox and means observability cannot stall card control (or vice versa).

---

## Compatibility

- Adding an enum variant to `ControlCmd` is backward-compatible with the existing
  CLI client: old commands serialise unchanged, and a daemon that receives an
  unknown `cmd` already responds with a parse error rather than crashing.
- A **new agent against an old daemon** gets a parse error, treats it as a
  permanent failure, discards the report, and keeps running — calls are
  unaffected. This matters because `entrypoint.sh` restarts the agents and the
  daemon independently, so a version skew window exists on every upgrade.
