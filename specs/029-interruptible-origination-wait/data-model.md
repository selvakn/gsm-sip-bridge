# Phase 1 Data Model: Interruptible wait for outbound call origination

Sketches are illustrative of shape and intent, not final signatures.

---

## Agent A — `ims::agent`

### `PendingOrigination` (new)

Everything an in-flight outbound attempt needs to survive across dispatch-loop
ticks. Today all of this lives in `originate_and_bridge`'s stack frame; making
the wait interruptible is precisely the act of moving it out.

```rust
struct PendingOrigination {
    step: OriginationStep,

    // --- identity, for correlating responses and control messages ---
    call_id: String,        // matches inbound responses (Call-ID) and CallEnded
    from_tag: String,
    branch: String,         // reused by CANCEL/ACK per RFC 3261 §9.1 / §17.1.1.3
    invite_cseq: u32,
    callee_uri: String,
    route_headers: Vec<String>,
    via_transport: &'static str,
    destination: String,

    // --- peers ---
    control: TcpStream,                      // to Agent B (writes)
    ctrl_rx: mpsc::Receiver<ControlMessage>, // from Agent B (reads) — spawned at
                                             // begin, not at success as today
    rtp_socket: UdpSocket,                   // bound before the INVITE went out

    // --- timing ---
    deadline: Instant,      // OUTBOUND_INVITE_TIMEOUT, then OUTBOUND_RING_TIMEOUT
    any_response_seen: bool,// the deadline switch, lifted out of sip_client
    ringing_relayed: bool,  // CallRinging is sent at most once

    // --- bookkeeping ---
    lifecycle: BridgedCall, // Answering from the moment the INVITE is sent (R4)
    dialog: Option<DialogInfo>, // Some once the carrier answered
}
```

**Why `ctrl_rx` moves earlier.** Today `spawn_control_reader` is called at
`agent.rs:1569`, *after* the call is fully bridged. Nothing can hear Agent B
before that. Spawning it in `begin_origination` is the single change that makes
abandonment observable on this side; `control.try_clone()` already gives an
independent read handle, so writes on `control` and reads on the clone do not
conflict.

### `OriginationStep` (new)

```rust
enum OriginationStep {
    /// INVITE sent; waiting for a final response from the carrier.
    AwaitingCarrier,
    /// Carrier answered 200 OK and was ACKed; waiting for Agent B's veth leg.
    /// This is the FR-008 window (VETH_INVITE_TIMEOUT, 5s), previously a
    /// blocking `veth_rx.recv_timeout` at agent.rs:1461.
    AwaitingVeth { veth_rx: mpsc::Receiver<...> },
}
```

### Transitions

Driven entirely by the existing `dispatch_loop` pump. No new loop.

| From | Trigger | To | Side effects |
|---|---|---|---|
| *(none)* | `PlaceCall`, line idle | `AwaitingCarrier` | `CallAttempting` → B; bind RTP; send INVITE; spawn control reader; lifecycle `Offered→Answering` |
| `AwaitingCarrier` | matching `1xx` (first) | `AwaitingCarrier` | deadline switches to `OUTBOUND_RING_TIMEOUT` |
| `AwaitingCarrier` | matching `180`, first only | `AwaitingCarrier` | `CallRinging` → B; lifecycle `Answering→PbxRinging` **(R5 fix)** |
| `AwaitingCarrier` | matching `200 OK` | `AwaitingVeth` | ACK; build `DialogInfo`; `CallPlaced` → B |
| `AwaitingCarrier` | matching non-2xx final | *cleared* | ACK; `CallFailed{"<status> <reason>"}` → B |
| `AwaitingCarrier` | `CallEnded{call_id}` from B | *cleared* | `cancel_pending_invite`; `CallFailed{CALLER_HANGUP}` → B |
| `AwaitingCarrier` | `ctrl_rx` disconnected | *cleared* | as abandonment, reason `TRANSPORT_ERROR` |
| `AwaitingCarrier` | deadline passed | *cleared* | `cancel_pending_invite`; `CallFailed{CARRIER_TIMEOUT}` → B *(today's only path)* |
| `AwaitingVeth` | veth leg arrives | → `ActiveCall` | negotiate codec; spawn relay; lifecycle `PbxRinging→Bridged` |
| `AwaitingVeth` | `CallEnded{call_id}` from B | *cleared* | `hangup_answered_carrier_leg` (the carrier leg is up) |
| `AwaitingVeth` | 5s elapsed | *cleared* | `hangup_answered_carrier_leg{VETH_LEG_FAILED}` *(today's behaviour)* |
| any | cleared, any reason | — | lifecycle `end(...)`; exactly one outcome reported (FR-019) |

**Invariant (FR-010)**: a `CallEnded` whose `call_id` differs from
`pending.call_id` is logged and ignored — never acted on.

**Invariant (FR-019)**: exactly one of `CallPlaced`/`CallFailed` is written to
Agent B per attempt, and exactly one outcome is reported. Enforced by clearing
`pending` and reporting in the same place.

### Admission (no rule change — R4)

```rust
// before
Admission::for_current(active_call.as_ref().map(|c| &c.lifecycle))
// after
Admission::for_current(
    active_call.as_ref().map(|c| &c.lifecycle)
        .or(pending.as_ref().map(|p| &p.lifecycle)),
)
```

`Admission::for_current` already treats any non-`Ended` lifecycle as busy
(`lifecycle.rs:247`). Because `PendingOrigination` carries a `BridgedCall` from
the moment the INVITE goes out, an inbound INVITE during an attempt is refused
`486` through the existing arm, and reported through the existing
`report_call_not_answered`. FR-011, FR-012 and FR-013 need no new code.

### Poll interval

```rust
let poll = if active_call.is_some() || pending.is_some() {
    ACTIVE_CALL_POLL_INTERVAL   // 100ms
} else {
    IDLE_POLL_INTERVAL          // 1s
};
```

A pending origination is a call in progress for polling purposes (R8). No
constant changes value.

---

## Agent B — `vowifi`

### `PlaceCallOutcome` (one new variant)

```rust
enum PlaceCallOutcome {
    Placed(BufReader<TcpStream>),
    Committed(String),
    Unavailable(String),
    Abandoned,          // NEW: our caller hung up during the attempt
}
```

`run_outbound_listener` treats `Abandoned` as terminal for the whole request —
`continue 'outer` without trying the next line (FR-004). No `call.answer(...)`
is owed: the caller is already gone.

### Attempt-phase loop

`await_place_call_outcome` today: one blocking `read_msg` with a 90s socket
timeout. After:

```rust
let deadline = Instant::now() + CALL_ATTEMPT_TIMEOUT;   // unchanged budget
reader.get_ref().set_read_timeout(Some(ATTEMPT_POLL_INTERVAL))?;  // ~100ms
let mut pending_line = String::new();   // carried across timeouts — R7
loop {
    match read_line_or_timeout(&mut reader, &mut pending_line) {
        Some(CallRinging) => { let _ = call.answer(180); }
        Some(CallPlaced)  => { ...; return Placed(reader) }
        Some(CallFailed{reason}) => return Committed(reason),
        None => {}   // nothing complete yet, fall through to the checks
    }
    if call.poll_state() == CallState::Disconnected {
        let _ = write_msg(&mut writer, &CallEnded {
            call_id: call_id.to_string(),
            reason: reason::CALLER_HANGUP.to_string(),
        });
        return PlaceCallOutcome::Abandoned;
    }
    if Instant::now() >= deadline { return Committed("attempt timed out".into()); }
}
```

`CALL_ATTEMPT_TIMEOUT` keeps its 90s value — it moves from being a *socket read
timeout* to being an *overall deadline*, which is what its doc comment already
describes it as. FR-015 holds.

---

## Cross-agent

### Outcome vocabulary

| Concept | Where | Value |
|---|---|---|
| Abandonment reason on the wire | `vowifi::control::reason` | `CALLER_HANGUP` — **exists already**, `control.rs:201` |
| Failure marker to Agent B | `CallFailed.reason` prefix | `CALLER_HANGUP` (mirrors today's `CARRIER_TIMEOUT` marker convention) |
| Metric / call-record outcome | `control::protocol::OutboundAttemptOutcome` | `CallerAbandoned` → `"caller_abandoned"` — **new variant** |

`OutboundAttemptOutcome` is `#[serde(rename_all="snake_case")]` and crosses a
process boundary to the daemon's metrics. Adding a variant is backward
compatible for *senders*; an older receiver would fail to deserialize it. Same
process tree is deployed together, so this is noted rather than mitigated.

### Lifecycle progression (R5 fix)

```text
before:  Offered → Answering → [Answering]      ← advance_to(Bridged) silently refused
after:   Offered → Answering → PbxRinging → Bridged
                     INVITE      carrier 180     both legs relaying
```

`reached_bridged` becomes true for successful outbound calls, so
`CallStage::is_success()` and the success-recording path finally hold for the
outbound direction as they already do for inbound.
