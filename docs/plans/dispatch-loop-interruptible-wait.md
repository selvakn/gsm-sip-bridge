# Plan: Interruptible wait for outbound origination in `dispatch_loop`

**Triaged**: 2026-08-06 · **Effort**: medium — single-threaded restructuring,
no new concurrency primitives · **Origin**: `docs/todo.md` item 3

## The problem, precisely

`ims::agent::dispatch_loop` (`gsm-sip-bridge/src/ims/agent.rs:1373`) calls
`originate_and_bridge` synchronously (call site `agent.rs:1544`, doc comment
at `agent.rs:1521-1543`) whenever a `PlaceCall` arrives and the line is idle.
That call can block for up to `OUTBOUND_INVITE_TIMEOUT + OUTBOUND_RING_TIMEOUT
+ VETH_INVITE_TIMEOUT` ≈ 15s + 60s + 5s = **80s**. For the whole window:

- No inbound carrier INVITE is dispatched (queued in `inbound.rx`, but the
  caller's own Timer B, 32s, will likely have expired before `dispatch_loop`
  gets back to it).
- A caller/PBX hanging up mid-ring can't trigger a CANCEL toward the carrier
  — `cancel_pending_invite` (`agent.rs:827`, doc at `agent.rs:799-825`) only
  fires on *our own* timeout, never on a phone/PBX-side hangup, because
  nothing can reach `dispatch_loop` to tell it.

Both gaps are already documented in code as known limitations, not silent.

## Why a spawned thread is the wrong fix

`originate_and_bridge` takes `&mut RegisteredSession` (`agent.rs:955`) and
writes to `session.transport_mut()` — the same TCP socket `dispatch_loop`
also writes to for REGISTER renewal (`attempt_renewal`) and BYE/CANCEL on
other paths. Moving origination onto a second OS thread means two writers to
one socket unless `RegisteredSession`/its transport grows real synchronization
(a mutex around the socket, at minimum) — a much bigger change than this gap
warrants, and a step away from this codebase's existing single-threaded,
cooperative-polling style (see `dispatch_loop`'s own `recv_timeout` poll,
and `SipClient::recv_message_deadline`'s doc at `sip_client.rs:795-807`,
which is the same pattern already used for "long wait, but come up for air
periodically").

## Recommended approach: cooperative polling, same thread

`SipClient::recv_final_response_for_origination`
(`gsm-sip-bridge/src/ims/sip_client.rs:764-793`) already loops internally:
each iteration does one `read_more()` (bounded by `RECV_TIMEOUT` = 5s,
`sip_client.rs:13`) and then checks its own deadline. That's already a
"come up for air every 5s" loop — it just doesn't check anything except its
own clock.

1. Add an `on_poll: impl FnMut() -> PollAction` callback parameter to
   `recv_final_response_for_origination`, invoked once per iteration
   alongside the existing deadline check (mirrors the existing
   `on_provisional` callback already threaded through the same function).
   `PollAction` is `Continue | AbandonCall(reason)`.
2. In `originate_and_bridge`, build that callback from two things
   `dispatch_loop` already owns and would need to pass down: the hangup
   signal for *this* call (Agent B's `ctrl_rx` — currently only polled once
   `active_call` exists, i.e. after origination returns) and, separately, a
   way to hand off (not drop) an inbound carrier INVITE that arrived
   mid-origination so `dispatch_loop` can act on it right after this call
   resolves, rather than leaving it to rot in `inbound.rx` until Timer B has
   already fired.
3. On `AbandonCall`, call `cancel_pending_invite` immediately (same function
   that today only fires from our own timeout) and return `None` from
   `originate_and_bridge`, same as any other failure path.
4. Apply the same callback through the `VETH_INVITE_TIMEOUT` wait
   (`agent.rs:1223`, the local leg) for symmetry — it's a shorter window (5s)
   but the same class of gap.
5. Granularity is bounded by `RECV_TIMEOUT` (5s) — a caller hangup is noticed
   within ~5s instead of up to ~80s. Tightening `RECV_TIMEOUT` itself is a
   separate, riskier change (it's used for every Gm read, not just
   origination) — not proposed here.

This keeps everything on `dispatch_loop`'s own thread: no new locks, no
`Arc<Mutex<RegisteredSession>>`, no risk of two writers on the socket.

## What still doesn't get fixed by this alone

An inbound carrier INVITE arriving during the 80s window is *noticed* sooner
(within one 5s tick, once it's threaded through) but `dispatch_loop` still
can't answer it until the outbound attempt resolves — one call is one call at
a time on this line either way, by design (`Admission::RejectBusy`). The
actual improvement is: (a) mid-ring caller hangup now sends CANCEL within
~5s instead of never observing it at all, and (b) a queued inbound INVITE
that's still viable (not yet past Timer B) gets picked up within ~5s of the
outbound attempt ending, instead of only at the top of the next full
`dispatch_loop` iteration after an 80s block — no worse than today, modestly
better, not a full fix for concurrent inbound-while-outbound-pending (that
really would need the two-call-legs-at-once redesign the original comment
alludes to, which is out of scope here).

## Testing

- Unit test on `recv_final_response_for_origination` directly: feed a fake
  transport that never responds, fire `AbandonCall` from the `on_poll`
  callback after N iterations, assert it returns promptly rather than
  waiting the full deadline.
- Extend whichever integration test already exercises
  `cancel_pending_invite`'s own-timeout path (grep `test_outbound` files) to
  add a "caller hangs up mid-ring" case that abandons via the new hook
  instead, asserting a CANCEL is sent.

## Open question for you

`PollAction`'s "hand off a queued inbound INVITE" half (step 2's second
clause) is the part with the most design freedom — it could be as simple as
"do nothing extra, `dispatch_loop`'s next iteration will find it in
`inbound.rx` within ~5s anyway" (simplest, and arguably sufficient given the
gain is already "5s instead of 80s"), or as involved as actively surfacing it
back out of `originate_and_bridge`'s return value. Recommend starting with
the simple version and only building the explicit hand-off if the 5s
residual gap turns out to matter in practice.
