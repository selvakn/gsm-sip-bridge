# Contract: Observability Protocol & SIP Wire Behaviour

**Feature**: `028-gm-tcp-reconnect`

## 1. `AgentState` — Agent A → metrics ingest

`control::protocol::AgentState` gains one field:

```rust
#[serde(skip_serializing_if = "Option::is_none", default)]
pub gm_connection_up: Option<bool>,
```

| Value | Meaning | Ingest action |
|---|---|---|
| `Some(true)` | Both halves of the Gm association are healthy | Gauge → 1; clear `gm_connection_unhealthy_since` |
| `Some(false)` | Either half is down | Gauge → 0; set `gm_connection_unhealthy_since` if not already set |
| `None` / absent | Not reported (older peer, or a report that carries only other fields) | **No change** to gauge or timestamp |

The `None` case is load-bearing: every existing sibling field uses
`skip_serializing_if`, so partial reports are normal, not exceptional. Treating
absent as `false` would report every line down on any report that happens not
to carry this field.

**Producer**: `AgentObservability::set_gm_connection_up(bool)`, mirroring
`set_tunnel_up` (`observability.rs:81-88`) — set the state field, then `push`.

**Call sites in `dispatch_loop`**, all state-change-triggered rather than
per-poll (the reporter coalesces, but a push per second per line is waste):
- `true` on a confirming ping success after a reconnect (R7)
- `false` on a dead verdict or a listener death
- `true` on a successful forced re-registration, alongside the existing
  `set_registered(true)` / `set_tunnel_up(true)` calls

## 2. SIP `OPTIONS` on the Gm client connection

**Direction**: Agent A → P-CSCF, on the connection the line registered over.

**Framing** — out-of-dialog, built by `sip_client::build_options()`, modelled on
`build_in_dialog_request` (`call.rs:792`):

```
OPTIONS sip:<pcscf-realm> SIP/2.0
Via: SIP/2.0/<TCP|UDP> <local_addr>;branch=z9hG4bK<random>;rport
Max-Forwards: 70
From: <sip:<public_uri>>;tag=<from_tag>
To: <sip:<public_uri>>
Call-ID: <session call_id>
CSeq: <session cseq> OPTIONS
Content-Length: 0
```

| Field | Source | Note |
|---|---|---|
| `CSeq` number | `session.cseq`, incremented | The correlation key (R1) |
| `Call-ID` / `from_tag` | the session's own | Keeps the ping inside the registration's identity |
| `To` | the public URI, no tag | Out-of-dialog: a keepalive, not a dialog-forming request |
| `Via` `local_addr` | `session.local_addr` | Must be re-read after a reconnect — `reconnect_transport` updates it (`ims/mod.rs:259`) |

**Send path**: `SipTransport::send` only. `send_and_recv` is **prohibited**
here — the reader thread owns the read half, and a second reader corrupts SIP
framing (R1). This is the single most important constraint in the feature.

**Response handling**: arrives on `inbound.rx` at the existing
`SipMessage::Response` arm (`agent.rs:1688`). Match the numeric part of the
response's `CSeq` header against `PendingPing.cseq`.

| Response | Verdict | Rationale |
|---|---|---|
| Any final response, **including 4xx/5xx** | **Alive** | The question is whether the connection carries signaling, not whether the carrier likes the request. A `405 Method Not Allowed` is a perfectly good liveness proof. |
| Non-matching CSeq | Ignored | A late response to a superseded ping must not revive a connection already scored dead. |
| Nothing within `PING_RESPONSE_TIMEOUT` | **Dead** | The blackholed-connection case this feature exists for (R2). |
| Send error | **Dead** | The RST case. |

**Rate**: one request + one response per line per 120s while idle; none during
a call. ≈30 exchanges/line/hour (FR-019).

## 3. `GmServer` liveness

```rust
impl GmServer {
    pub fn is_alive(&self) -> bool;
}
```

`alive: Arc<AtomicBool>` initialised `true`, stored `false` by the accept loop
immediately before its fatal-error `return` (`sip_client.rs:1023-1026` for TCP;
the UDP loop's equivalent).

**Distinct from the existing `stop` flag.** `stop` is an instruction *to* the
loop (set by `Drop`); `alive` is a report *from* it. Conflating them would make
a deliberate shutdown indistinguishable from a crash — and `Drop` sets `stop`
on every normal teardown, so the conflated version would report a crash on
every clean re-registration.

**Recovery**: `session::restart_gm_server(session, inbound)` re-runs
`spawn_gm_server(session.gm_server_addr()?, session.use_tcp, inbound.tx.clone())`
and replaces `inbound._server`. The port is free by then: the `TcpListener` is
moved into the accept thread, so its `return` drops it.
