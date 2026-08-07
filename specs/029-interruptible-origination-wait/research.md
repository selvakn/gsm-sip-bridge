# Phase 0 Research: Interruptible wait for outbound call origination

All findings below come from reading the current tree at `0157fe9`. Two of
them are **not** in the spec or in the triage plan and materially change the
recommended approach; they are marked **NEW FINDING** and each carries an
explicit confidence level and a way to confirm it.

---

## R1 — The triage plan's premise about Agent B is wrong

**Decision**: Both agents are in scope. Agent B needs a change before Agent A's
half can do anything.

**Finding**: `docs/plans/dispatch-loop-interruptible-wait.md` step 2 says the
abandon callback can be built from "Agent B's `ctrl_rx` — currently only polled
once `active_call` exists". That reads as though the hangup signal is produced
and merely not listened for. It is not produced.

While an outbound attempt is in flight, Agent B is inside
`await_place_call_outcome` (`gsm-sip-bridge/src/vowifi/mod.rs:900-926`):

```rust
loop {
    match read_msg(&mut reader) {
        Ok(ControlMessage::CallRinging { .. }) => { let _ = call.answer(180); }
        Ok(ControlMessage::CallPlaced { .. })  => { ... return Placed(reader) }
        Ok(ControlMessage::CallFailed { reason, .. }) => return Committed(reason),
        ...
    }
}
```

It only ever *reads* from Agent A. It never calls `call.poll_state()`, and it
never writes. The read timeout on that socket is `CALL_ATTEMPT_TIMEOUT` (90s),
so `run_outbound_listener`'s whole `'outer` loop is parked for the duration.

Contrast the established-call loop at `vowifi/mod.rs:1527-1553`, which polls
*both* sides on a 100ms tick and writes `CallEnded{PBX_HANGUP}` when its own leg
drops. That is the pattern missing from the attempt phase.

**Rationale**: Implementing only Agent A's `on_poll` hook would leave User
Story 1 unachievable — there would be nothing for the hook to observe.

**Alternatives considered**: Deriving abandonment on Agent A from the control
socket closing. Rejected: PJSUA tearing down the phone leg does not close
Agent B's control connection to Agent A, so the socket stays open and readable.

---

## R2 — **NEW FINDING**: two threads read the carrier client connection concurrently

**Confidence**: High on the code structure (verified by reading); **unverified
at runtime** — no live reproduction, and nothing in `specs/025-outbound-calling`
records it. Task T001 exists to settle it before anything is built on top.

**Finding**: `session::start_inbound` spawns a permanent reader thread on the
**client** connection:

```rust
// gsm-sip-bridge/src/ims/session.rs:78-99
fn spawn_client_reader(session: &RegisteredSession, tx: Sender<(SipMessage, SipSink)>) {
    let mut client_reader = session.transport()?.try_clone_reader()?;   // dup(2) of the same fd
    std::thread::spawn(move || loop {
        match client_reader.recv_message_deadline(CLIENT_READ_POLL_INTERVAL) { ... }
    });
}
```

`try_clone_reader` (`sip_client.rs:957`) calls `TcpStream::try_clone` — an
independent handle on the **same socket**, not a separate connection.
`RegisteredSession` holds exactly one `transport: Option<SipTransport>`
(`ims/mod.rs:137`); `transport()` and `transport_mut()` both hand out
references to it.

Meanwhile `originate_and_bridge` reads that same socket directly:

```rust
// gsm-sip-bridge/src/ims/agent.rs:1301
let resp = match transport.recv_final_response_for_origination(...)
```

So while an INVITE is pending there are two threads blocked in `read()` on one
TCP socket. Whichever the kernel wakes takes the bytes. If the background
reader wins, the carrier's `180`/`200` is delivered to `inbound.rx` and shows up
at the dispatch loop's response arm — which logs it and drops it:

```rust
// gsm-sip-bridge/src/ims/agent.rs:1966
tracing::info!(status, reason, "received response outside an active transaction");
```

— while `originate_and_bridge` waits out its full 80s and reports
`CARRIER_TIMEOUT`. That is an exact description of the symptom
`specs/025-outbound-calling` T072 pass 3 chased and attributed to the timeout
being too short.

**Decision**: Origination must receive its responses through `inbound.rx`, the
single queue, rather than reading the socket directly.

**Rationale**: This is already the codebase's own established pattern. The
specs/028 Gm keepalive sends via `session.send_gm_ping()` (write-only) and
correlates the reply at the dispatch loop's response arm
(`agent.rs:1943-1962`) by CSeq. It has exactly one reader. The origination path
predates that convention and never adopted it.

Adopting it here is what makes the rest of this feature cheap — see R3.

**Alternatives considered**:
- *Pause the background reader during origination.* Rejected: needs
  cross-thread coordination (the very thing the triage plan rightly wants to
  avoid), and the reader can already be mid-`read()` holding bytes.
- *Leave the race, add only `on_poll`.* Rejected: keeps a latent correctness bug
  and does not enable FR-011 (see R3).

**How T001 settles it**: assert on the observable consequence, not on thread
scheduling — with a fake carrier and `start_inbound` running, place an INVITE,
have the carrier reply, and assert the reply reaches origination every time
across many iterations. Under the race it will fail intermittently.

---

## R3 — Cooperative polling should reuse the dispatch loop, not a new callback

**Decision**: Model an in-flight origination as **state held by `dispatch_loop`**
(`Option<PendingOrigination>`), advanced by the loop's existing
`inbound.rx.recv_timeout(poll)` pump — rather than adding an `on_poll` callback
to `recv_final_response_for_origination`.

**Rationale**: The callback approach hits a wall on FR-011. To refuse an inbound
INVITE as busy, the callback must drain `inbound.rx` from inside
`originate_and_bridge`. But that queue is FIFO and carries everything —
inbound SMS (`MESSAGE`), the Gm keepalive's `OPTIONS` reply, `BYE`, reg-event
`NOTIFY`. A callback that `try_recv`s looking for INVITEs would **silently
discard inbound SMS**, which the codebase treats as a data-loss bug serious
enough to have its own acknowledge-after-recording ordering rule
(`handle_message`, `agent.rs:809-826`). Handling every variant inside the
callback means duplicating the dispatch loop's match arms.

Holding the origination as loop state instead means every existing arm keeps
working unchanged — SMS, keepalive correlation, BYE — and the origination
response is just one more thing the response arm recognises. Nothing is
duplicated and nothing can be dropped.

Three further things fall out for free:

1. **Interruptibility** is inherent: the loop already ticks on
   `IDLE_POLL_INTERVAL`/`ACTIVE_CALL_POLL_INTERVAL`, so `ctrl_rx` can be checked
   every tick. No new poll mechanism, and granularity is ~100ms rather than the
   ~5s the triage plan settles for (`RECV_TIMEOUT`-bound).
2. **FR-011/FR-012 need no new admission logic** — see R4.
3. The veth wait (FR-008) becomes another state in the same machine rather than
   a second place needing the same callback.

**Cost, stated plainly**: this restructures `originate_and_bridge` (~350 lines)
into a begin/advance/finish trio. It is the largest single change in this
feature. It is justified under Constitution V on the grounds that it *removes*
a blocking call and a duplicate socket reader rather than adding a layer — the
net count of moving parts goes down.

**Alternatives considered**:
- *`on_poll` callback as the triage plan proposes.* Smaller, and genuinely
  simpler in isolation. Rejected because it leaves R2 unfixed and cannot deliver
  FR-011 without duplicating the dispatch loop.
- *Spawn origination on its own thread.* Rejected for the reason the triage plan
  gives: two writers on one socket, needing a mutex on `RegisteredSession`.

---

## R4 — "Busy" during origination needs no new rule

**Decision**: Give `PendingOrigination` a `BridgedCall` lifecycle from the moment
the INVITE is sent, and feed it to the existing admission check.

**Finding**: `Admission::for_current` (`ims/lifecycle.rs:247`) already returns
`RejectBusy` for any `BridgedCall` whose stage is not `Ended`. Today the
outbound path only constructs its `BridgedCall` at the very end, on success
(`agent.rs:1588`) — so during origination there is no lifecycle to consult and
the line reads as idle.

Moving construction to the start makes the inbound-INVITE arm refuse with
`486 Busy Here` through the code path it already runs, and report through
`obs.report_call_not_answered(...)` exactly as it does for a call arriving
during an established one. FR-011, FR-012 and FR-013 are then satisfied by
existing code.

---

## R5 — **NEW FINDING**: the outbound lifecycle never reaches `Bridged`

**Confidence**: High — this is a pure state-machine reading, no timing involved.

**Finding**: `agent.rs:1588-1590` does:

```rust
let mut lifecycle = BridgedCall::new(call_id.clone(), destination.to_string(), None);
lifecycle.advance_to(CallStage::Answering);
lifecycle.advance_to(CallStage::Bridged);
```

`BridgedCall::new` starts at `Offered` (`lifecycle.rs:147`). `can_advance_to`
(`lifecycle.rs:45-52`) permits only `Offered→Answering`, `Answering→PbxRinging`,
`PbxRinging→Bridged`, plus anything`→Ended`. So `Answering→Bridged` is illegal:
`advance_to` returns `false` and the stage silently **stays at `Answering`**.
`reached_bridged` therefore stays `false` for every successful outbound call,
and `CallStage::is_success()` is false.

**Decision**: Fix as part of US3 (FR-018/FR-019, "exactly one recorded outcome
that names how it ended"). The refactor makes the correct progression natural:
the dispatch loop now sees the carrier's `180`, which is precisely the
`PbxRinging` transition that was missing.

`Offered → Answering` (INVITE sent) → `PbxRinging` (carrier `180`) → `Bridged`
(both legs relaying).

**Note**: `advance_to`'s silent `false` return is what let this hide. Not
changing that here — it is a deliberate "refuse an impossible transition"
design — but the tasks add an assertion so a future regression fails loudly.

---

## R6 — Reuse `CallEnded` for abandonment; no new protocol variant

**Decision**: Agent B signals abandonment with the existing
`ControlMessage::CallEnded { call_id, reason }`, using the existing
`reason::CALLER_HANGUP` constant (`vowifi/control.rs:201`).

**Rationale**: `CallEnded`'s documented meaning is already "whichever agent sees
its own leg drop first sends this" (`control.rs:28-33`) — abandonment during the
attempt phase is the same event, one phase earlier. Adding a variant would mean
a wire-compatibility story for mixed-version peers, for no semantic gain
(Constitution V).

`call_id` is what makes FR-010 enforceable: Agent A ignores a `CallEnded` whose
`call_id` does not match the attempt in flight.

**Alternatives considered**: a new `AbandonAttempt` variant. Rejected as above.

---

## R7 — Agent B's short-read problem is already solved in this file

**Decision**: Reuse the `pending_line` accumulation pattern.

**Finding**: Dropping `await_place_call_outcome`'s read timeout from 90s to a
~100ms poll means `read_line` will routinely time out mid-message. The codebase
already hit this and documented the fix: `ActiveOutboundCall.pending_line`
(`vowifi/mod.rs:1033-1041`) exists because "`read_line` documents that any bytes
it already appended stay in the buffer even when it returns an error, but a
fresh `String::new()` per call throws that partial data away".

`read_msg` (`control.rs:220`) allocates a fresh `String` per call and so cannot
be used directly on a short-timeout socket. The attempt-phase loop needs the
same carried buffer.

**Alternatives considered**: keeping the 90s timeout and polling the phone leg on
a separate thread. Rejected — a thread per attempt, to avoid a buffer that
already exists ten lines away.

---

## R8 — Overall detection budget

**Decision**: End-to-end caller-hangup → CANCEL well inside SC-001's 10s.

| Hop | Bound | Source |
|---|---|---|
| PJSUA marks the phone leg `Disconnected` | ~immediate | PJSUA callback |
| Agent B notices on its poll tick | ≤100ms | reuse `PBX_RING_POLL_INTERVAL` |
| `CallEnded` crosses the control socket | ~ms | loopback / veth |
| Agent A's dispatch loop ticks | ≤1s | `IDLE_POLL_INTERVAL`; see below |
| `cancel_pending_invite` sends CANCEL | ~ms | one socket write |

**Sub-decision**: while an origination is pending, the loop must poll at
`ACTIVE_CALL_POLL_INTERVAL` (100ms), not `IDLE_POLL_INTERVAL` (1s) — a pending
origination is a call in progress for polling purposes. That brings the budget
to roughly 200ms, an order of magnitude inside SC-001, and it costs one line:
the existing `let poll = if active_call.is_some() {...}` gains the pending case.

No existing timeout constant changes, satisfying FR-015.

---

## R9 — Baseline coverage (T001)

**What exists today for the outbound path:**

- `tests/test_outbound_diagnostics.rs` — exercises the **observability** path
  only: reports each `OutboundAttemptOutcome` over a real control socket and
  asserts the metric series are distinguishable. Its own header comment states
  plainly that "a real end-to-end call needs real hardware (pjsua + a modem or
  a live VoWiFi/VoLTE line), which this test suite does not have available."
- `tests/test_volte_bridge.rs`, `tests/test_vowifi_call_metrics.rs` — likewise
  metrics/wire-level, not the `dispatch_loop`/`originate_and_bridge` code paths.
- The *decision* logic (which outcome a failure maps to) is unit-tested in-crate
  in `vowifi::mod` (`committed_failure_outcome_distinguishes_...`).

**Consequence for this feature's tests** (important, differs from the tasks'
original file placement): `dispatch_loop`, `originate_and_bridge`,
`RegisteredSession`, `Inbound`, and `start_inbound` are all `pub(crate)`. A
`tests/` integration crate cannot reach them. Therefore:

- Socket-/session-level tests (race, abandon-during-carrier-wait) live **in-crate**
  under `#[cfg(test)]` in `src/ims/agent.rs`, using a new
  `#[cfg(test)]`-only `RegisteredSession` test constructor pointed at a loopback
  `TcpListener` fake carrier (`SipTransport::connect` dials it normally — no
  raw-socket wrapper needed).
- Pure-logic tests (state-machine transitions, `pending_line` accumulation,
  admission-with-pending, lifecycle progression) live next to the code they
  cover, needing no sockets at all.
- Anything requiring a live `pjsua_safe::Call` (Agent B's `poll_state`) needs
  the `pjsip-linked` feature and real audio hardware — not sandbox-runnable, so
  it stays manual (quickstart / T046), matching the existing suite's stance.

`tasks.md` file paths that say `tests/test_outbound_abandon.rs` are read as
"in-crate `#[cfg(test)]` where the item is reachable" wherever the target is
`pub(crate)`; the intent (what is asserted) is unchanged.

**Baseline green?** Yes — `make test` at `0157fe9` + this branch's spec docs:
**1241 passed, 0 failed** across 59 test binaries. This is the number the
restructure must not regress.
