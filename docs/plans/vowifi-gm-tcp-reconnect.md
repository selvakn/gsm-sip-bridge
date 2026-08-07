# Plan: Reconnect logic for a silently-dropped Gm TCP connection

**Triaged**: 2026-08-06 · **Effort**: medium · **Origin**: `docs/todo.md`
item 4 (found live, specs/025-outbound-calling T072 pass 1, 2026-08-03)

## Current behavior (confirmed still present)

- The Gm client reader thread (`spawn_client_reader`,
  `gsm-sip-bridge/src/ims/session.rs:78-99`) exits silently on a read
  error/EOF — logs `"Gm client connection reader stopped"` and returns.
- `Inbound.tx` also has a clone held by the Gm-server accept loop
  (`start_inbound`, `session.rs:109-129`), so the mpsc channel isn't fully
  disconnected when only the client reader dies —
  `dispatch_loop`'s `Disconnected` branch
  (`gsm-sip-bridge/src/ims/agent.rs:1699-1704`, which would otherwise cause
  the process to exit and get respawned by
  `supervise::orchestrate` at `orchestrate.rs:1381`) never fires.
- Nothing actively probes the idle connection: no `SO_KEEPALIVE`, no SIP
  OPTIONS ping, anywhere in `src/ims`.
- The only paths that rebuild the transport are reactive, not proactive:
  - Scheduled registration renewal (`attempt_renewal` +
    `restart_client_reader`) — at most every ~55 minutes
    (`DEFAULT_EXPIRES`=3600s − `RENEWAL_HEADROOM`=300s, `ims/mod.rs:61`,
    `agent.rs:99,1714`).
  - `RegisteredSession::reconnect_transport` (`ims/mod.rs:235-262`) — only
    invoked from `hangup_carrier` (`agent.rs:2375-2388`) when a BYE-send
    fails mid-call.
- Net effect: a Gm TCP reset between renewals leaves the line silently dead
  — not reflected in `vowifi-status`, not alerted — until either a call is
  attempted mid-call and fails outbound, or up to ~55 minutes pass and the
  next renewal happens to reconnect it.

Note: `specs/027-discover-retry-health`'s retry loop is a different
mechanism — it retries the *startup* discovery pass for hardware that
wasn't yet enumerated. It has no interaction with an already-registered
line's live Gm socket, so it doesn't cover this.

## Plan

1. **Active health check.** Add a lightweight keepalive on the idle Gm
   connection — either `SO_KEEPALIVE` on the socket (cheapest, but relies on
   OS-level TCP timers, which can be slow — hours by default on Linux unless
   `TCP_KEEPIDLE`/`TCP_KEEPINTVL`/`TCP_KEEPCNT` are also tuned), or an
   application-level ping (a SIP `OPTIONS` sent on an idle timer from
   `dispatch_loop`, since RFC 3261 UAs must respond to it, and a failed send
   or absent response is an unambiguous "the connection is dead" signal).
   Recommend the OPTIONS approach: it reuses the existing SIP transport
   machinery, and gives a bounded, application-controlled detection window
   instead of depending on OS defaults.
2. **On detected death, reconnect proactively** — call the same
   `RegisteredSession::reconnect_transport` (`ims/mod.rs:235-262`) that
   `hangup_carrier` already uses reactively, from `dispatch_loop`'s idle-poll
   branch, gated behind the same `MaintenancePolicy`/`AttachmentWatch`
   "don't interrupt an active call" discipline the renewal path already
   respects (`agent.rs`'s `maintenance` handling).
3. **Surface it.** Follow the pattern `specs/027-discover-retry-health`
   established for a different failure kind: a status field
   (`vowifi-status`/`volte-status` already print per-line state — add
   "Gm connection: reconnecting since <time>" alongside), a Prometheus gauge
   analogous to `gsm_sip_bridge_vowifi_tunnel_up`, and (if the reconnect
   itself fails repeatedly) a Discord alert via the existing
   `CategoryAlertConfig`/`AlertPhase` pattern (`alerts/mod.rs`) — same shape
   as `registration_loss`/`tunnel_failure`, new category
   (e.g. `gm_connection_lost`).
4. **Bound the retry.** Reuse the existing `RETRY_INITIAL_BACKOFF`/backoff
   state already in `dispatch_loop` (`agent.rs:1391`) rather than inventing
   a second backoff scheme.

## Testing

- Unit test: fake transport that fails a send after N successful ones,
  assert the OPTIONS-ping path detects it within one ping interval and
  calls `reconnect_transport`.
- Integration test alongside `test_vowifi_health_metrics.rs` (the closest
  existing analog per the `027` plan's own test list) for the new gauge and
  alert pairing.
- Hardware re-verification against the same scenario that surfaced this
  (T072 pass 1: line up for "some minutes" post-registration, connection
  resets) — since this was only ever caught live, not reproduced
  synthetically, a live re-test is the real confirmation.

## Open questions for you — **resolved 2026-08-07**

Specced as `specs/028-gm-tcp-reconnect` (branch `028-gm-tcp-reconnect`).
Full rationale, including rejected alternatives, is in that spec's
Clarifications section.

1. **OPTIONS ping vs. `SO_KEEPALIVE`+tuned intervals** → **OPTIONS ping.**
   Bounded, application-controlled detection window; testable against a fake
   transport; and — decisively — a socket keepalive only proves the socket is
   open, not that SIP still works over it, which is exactly the false-recovery
   case when the Gm SA is what died.
2. **Ping interval** → **~2 minutes** (fixed constant, not config). Worst-case
   dead-line duration ~2–3 min, at ~30 extra messages/line/hour. Making it
   configurable was deliberately deferred until a carrier objects to the rate.

Two further questions surfaced during specification and were resolved with
the same session:

3. **Escalation when reconnect keeps failing** → **full re-registration for
   that line**, not process exit. Exiting and letting `supervise::orchestrate`
   respawn would drop every other line's in-progress calls to fix one broken
   line. Re-registration renegotiates a fresh Gm SA, which is the only thing
   that can fix the case where the SA itself expired.
4. **Scope** → **VoWiFi *and* VoLTE** (they share `ims::agent`, so scoping to
   one leaves the identical gap open on the other), and **both halves of the
   Gm pair** — the client connection observed to die *and* the protected
   server-port listener, whose death is the symmetric blind spot (line can
   place calls, never receive one) and is invisible today for the same reason.
