# Phase 0 Research: Gm Connection Liveness & Automatic Reconnect

**Feature**: `028-gm-tcp-reconnect` · **Date**: 2026-08-07

All findings below were confirmed by reading the current tree, not inferred
from the triage document. Where the triage's assumption turned out to be
wrong or incomplete, that is called out explicitly.

---

## R1 — The OPTIONS ping MUST NOT use `send_and_recv`

**Decision**: Send the OPTIONS request fire-and-forget via
`SipTransport::send`, record the pending transaction in `dispatch_loop`'s own
state, and match the response when it arrives on `inbound.rx` at the existing
`SipMessage::Response` arm (`agent.rs:1688-1697`).

**Rationale**: This is the single most important constraint in the feature, and
it is not obvious. `spawn_client_reader` (`session.rs:82`) calls
`session.transport()?.try_clone_reader()` and hands the read half to a
dedicated thread that loops on `recv_message_deadline` forever. That thread
owns reading. `SipTransport::send_and_recv` (`sip_client.rs:706`) writes and
then *reads a response off the same socket*. Calling it from `dispatch_loop`
while the reader thread is live means two readers racing on one TCP stream:
whichever wins consumes the bytes, and a partially-consumed message corrupts
the framing for the other. The failure would be intermittent, would look like
random SIP parse errors, and would be extremely unpleasant to diagnose.

The `SipMessage::Response` arm already exists and currently just logs
`"received response outside an active transaction"` — the OPTIONS response
would land there today. Matching it there is therefore not new machinery, it
is filling in a branch that is already receiving the traffic.

**Alternatives considered**:
- *`send_and_recv` with the reader thread paused* — would need a pause/resume
  handshake with the reader thread. More moving parts, violates Principle V,
  and the pause window is exactly when an inbound INVITE would be dropped.
- *A dedicated ping socket* — a second connection would have its own liveness
  independent of the one we are trying to test. It would answer the wrong
  question.

**Correlation key**: `CSeq`. `SipResponse::header()` (`sip_client.rs:37`)
already exposes it. The session's `cseq` counter (`ims/mod.rs:150`) supplies a
unique value per ping; store the number we sent and compare on arrival. Match
on the numeric part, since the method token in a response's `CSeq` is echoed
from the request (`OPTIONS`).

---

## R2 — Detection covers both failure shapes, and the second is the observed one

**Decision**: Treat a ping as failed on *either* a send error *or* an
unanswered ping older than a response deadline (FR-022).

**Rationale**: A TCP reset is not always visible on the next `send`. If the
peer's RST has not yet been processed, or the connection is blackholed (NAT
dropped the flow silently, no RST at all — the classic idle-NAT case named in
`reconnect_transport`'s own doc comment at `ims/mod.rs:200-203`), the `send`
returns `Ok` and the bytes vanish. Only the absent response reveals it. A
send-error-only check would miss precisely the failure mode this feature
exists for.

**Deadline**: reuse the existing SIP response timeout the transport already
applies (~5s, per `unregister`'s doc comment at `ims/mod.rs:277-279`) with
headroom. Concretely: consider a ping dead if no matching response arrives
within `PING_RESPONSE_TIMEOUT` (proposed 10s), which is generous relative to a
P-CSCF's normal response time and still 12× inside the 2-minute ping period.

---

## R3 — Why the process never notices today (confirming the triage)

**Confirmed**. `Inbound.tx` (`session.rs:67`) is cloned into both
`spawn_client_reader` and `spawn_gm_server` (`session.rs:112,115`). The mpsc
channel reports `Disconnected` only when *every* sender is dropped. So when the
client reader thread returns (`session.rs:93-95`, logging `"Gm client
connection reader stopped"`), its `tx` clone drops but the server's clone is
still alive — `dispatch_loop`'s `Disconnected` branch (`agent.rs:1699-1704`),
which would return an error and let `supervise::orchestrate` respawn the
process, never fires. The loop keeps spinning on `recv_timeout`, status keeps
saying `Registered`, and the line is dead.

**Implication for design**: detection must be per-connection. A channel-level
signal is structurally incapable of expressing "one of the two halves died,"
which is the exact case.

---

## R4 — The inbound listener's death is detectable cheaply, and needs no probe

**Decision**: Add an `alive: Arc<AtomicBool>` to `GmServer`
(`sip_client.rs:937`), set `false` by the accept loop on its way out, and
expose it as `GmServer::is_alive()`. Poll it from `dispatch_loop` alongside the
ping.

**Rationale**: `spawn_gm_tcp_server`'s accept loop already has exactly one
fatal exit — `tracing::warn!(... "Gm server accept failed; stopping"); return;`
(`sip_client.rs:1023-1026`). Today that `return` is silent to the rest of the
process: `GmServer` is just a stop flag held in `Inbound._server`, so nothing
observes the thread's death. Flipping a flag on the way out costs one atomic
store and turns an invisible failure into a polled one.

**Why not probe the listener by connecting to it**: we would be connecting to
our own protected `port-s` from inside the same netns. That traffic would have
to match the installed XFRM policy's selector to be representative, and a
self-connect that bypasses it would prove nothing about network reachability.
It is more machinery for a weaker signal.

**Known limitation, accepted**: this detects "the accept loop died," not
"the listener is alive but the network can no longer reach it." The latter is
indistinguishable from "nobody is calling" without carrier cooperation, and the
reg-event SUBSCRIBE/NOTIFY flow is the only existing thing that would reveal
it. Out of scope; recorded here so it is a known gap rather than an assumed
capability.

**Recovery**: mirror `restart_client_reader` with a `restart_gm_server` that
re-runs `spawn_gm_server` on the same `gm_server_addr()` and replaces
`inbound._server`. The port is free by then — the `TcpListener` is moved into
the accept thread, so its `return` drops it.

---

## R5 — VoLTE is covered for free

**Confirmed**: `volte::carrier_agent::run` calls
`ims::agent::serve_inbound` (`carrier_agent.rs:173`), which runs the same
`dispatch_loop` (`agent.rs:450`). There is exactly one dispatch loop in the
tree. Implementing liveness inside it satisfies FR-020 for both transports with
no VoLTE-specific code — which is the same reasoning that motivated extracting
`ims::session` in spec 017 (`ims/mod.rs:1-15`).

**Consequence for testing**: no separate VoLTE test path is needed for the
detection logic itself. The metrics/alerting surface does need the VoLTE
`module` label checked, because `ingest.rs:396` already carries a caveat about
`gsm_sip_bridge_vowifi_tunnel_up{module="volte"}`.

---

## R6 — Escalation reuses the renewal path, gated by a force flag

**Decision**: After `MAX_RECONNECT_ATTEMPTS` consecutive failed
`reconnect_transport` calls, set a `force_renewal` flag that bypasses only the
`renewal_due(...)` early-`continue` at `agent.rs:1714`. Everything downstream
of that check — the maintenance deferral, the modem lock, the pre-renewal
attach hook, `attempt_renewal`, the backoff, the status updates — runs
unchanged.

**Rationale**: The renewal branch already does exactly what escalation needs,
and does it correctly. On success it runs `session.cleanup(); *session =
new_session; *inbound = start_inbound(session)?` (`agent.rs:1770-1774`) — a
fresh Gm SA on fresh ports, with *both* readers rebuilt. That single line is
why re-registration is the right escalation for both halves of FR-021: it
repairs the client reader and the server listener together, and it renegotiates
the SA, which is the only thing that can fix R7's false-recovery case.

Bypassing one `if` is a far smaller change than a parallel escalation path, and
it inherits the "don't renew mid-call" discipline (FR-006) and the existing
`next_renewal_attempt`/`backoff` rate limiting (FR-005) without restating them.

**Alternatives considered**:
- *Return `Err` from `dispatch_loop` so `supervise::orchestrate` respawns the
  process* (`orchestrate.rs:1381`) — rejected per the spec's clarification: it
  drops every other line's in-progress calls to fix one line.
- *A separate re-registration routine* — would duplicate the attach hook, modem
  lock and status handling. Straight Principle V violation.

---

## R7 — Confirming recovery, not just reconnection (FR-009)

**Decision**: After a successful `reconnect_transport` +
`restart_client_reader`, do **not** mark the connection up. Instead send a
fresh OPTIONS ping immediately and mark up only when its response arrives.

**Rationale**: `reconnect_transport` (`ims/mod.rs:235-262`) rebinds `port-c`
and reconnects to `remote_s` on the strength of the *existing* `gm_state`. Its
own doc comment is explicit that this works because "the still-live IPsec SA
(its lifetime is independent of any one TCP connection) still applies to the new
socket." The corollary is the failure case: if the SA is what expired, the TCP
connect can still succeed (the XFRM policy matches, the packets go out
encrypted under a dead SA) and every subsequent request is silently dropped by
the P-CSCF. The line would then report itself recovered and be just as dead as
before — a strictly worse outcome than not reconnecting, because it also resets
the failure timer and suppresses the alert.

Requiring a successful round-trip before declaring recovery is what makes the
escalation in R6 reachable at all.

---

## R8 — Alerting slots into `metrics::ingest`, not into the agent

**Decision**: Report connection health as agent state; evaluate the alert at
scrape time in `metrics::ingest`, exactly as `registered`/`tunnel_up` are.

**Rationale**: `ingest.rs` already owns this pattern completely: an
`unhealthy_since` timestamp per signal, an `AlertPhase` state machine
(`Idle → Pending → Alerted → Idle`, `ingest.rs:49,241-256`) that yields the
one-alert-per-episode property in FR-015 and the paired recovery in FR-016 for
free, plus `record_suppressed` for the flap case. Agent A serves no `/metrics`
endpoint of its own (`protocol.rs:128-133`), so a gauge written in the agent's
process would land in a registry nothing scrapes — the mistake already recorded
in `docs/greptile-review-learnings.md`.

The `unreachable!` at `ingest.rs:313` ("only RegistrationLoss/TunnelFailure
transitions are produced here") is a live tripwire: adding a third category
without extending that `match` panics the ingest path. Must be handled.

**Config plumbing** is six mechanical touch points, all confirmed present for
the `line_discovery_failed` precedent added by spec 027:
`config/raw.rs:351` (field), `config/raw.rs:596` (known-keys table),
`config/build.rs:490` (defaulting), `config/mod.rs:403` (typed field),
`config/mod.rs:482` (disabled default), `alerts/mod.rs:57,150`
(`as_str` + `category_config`). Plus `AlertCategory` itself at
`alerts/mod.rs:24` and the critical-alert allowlist at `alerts/mod.rs:190`.

**Default**: enabled. Unlike `line_discovery_failed` (defaulted off, since a
deliberately unconfigured line would page needlessly), a Gm connection that
cannot be restored on a line that *is* registered is unambiguously an incident.

---

## R9 — `can_answer` must account for the connection

**Decision**: Add `gm_connection_up` to `ims::lifecycle::ServiceHealth` and
include it in `can_answer()`, with a `blocked_reason()` string.

**Rationale**: `can_answer`'s doc comment (`lifecycle.rs:419-433`) states the
governing rule: it "must never be optimistic," because a card on this path has
no circuit-switched fallback, so a false yes means calls are silently missed.
A line whose Gm connection is dead cannot answer — that is precisely the
observed incident. Leaving `can_answer` true through a reconnect would
reproduce the original bug in the health surface itself.

Ordering in `blocked_reason` matters: place the connection check *after*
`attached` and `registered` (both are underneath it — a down attachment
explains a down connection, and reporting the symptom over the cause would
mislead) and before `pbx_registered`.

**Accepted consequence**: `can_answer` will now flap false for the ~2-3s of a
successful reconnect. This is correct — it is briefly true that a call could
not be answered — and matches the existing treatment of a deferred renewal.

---

## R10 — Probe cost and carrier safety

**Decision**: `PING_INTERVAL = 120s`, fixed constant next to
`RENEWAL_HEADROOM` (`agent.rs:99`).

**Numbers**: 30 OPTIONS + 30 responses per line per hour, against a
registration that already carries a REGISTER per hour, a reg-event SUBSCRIBE,
and its NOTIFYs. OPTIONS is the RFC 3261 §11 method a UA is *required* to
answer, and is what `sip::server` itself already answers for IP phones
(`sip/server/mod.rs:14,265`) — so this is the same keepalive discipline the
project already applies in the other direction, not a novel behaviour toward
the carrier.

**Deliberately not configurable** — see the spec's Assumptions. Adding a config
key means raw/build/mod/env/docs/test touch points for a value with no evidence
it needs per-carrier tuning (Principle V, YAGNI).

**Idle-only**: the ping is skipped entirely while a call is in progress
(FR-006). A call generates its own Gm traffic, so liveness is being proven
continuously by the call itself; pinging on top of it adds risk for no signal.

---

## R11 — Renewal and ping must not race (FR-011)

**Decision**: Check `renewal_due` first in the idle branch, as today. Run the
ping only when a renewal is *not* proceeding this iteration, and drop any
pending ping when a renewal succeeds (the transport it referenced no longer
exists).

**Rationale**: A renewal replaces `*session` and `*inbound` wholesale
(`agent.rs:1770-1774`). A ping CSeq recorded against the old session would
never be answered on the new one, and would then be scored as a failure ~10s
after a *successful* renewal — spuriously driving a reconnect on a
freshly-healthy line. Clearing pending-ping state on session replacement is
mandatory, not defensive.

Same reasoning covers FR-017: pending-ping state, `reconnecting_since`, and the
reconnect attempt counter all live in `dispatch_loop`'s stack, so a process
restart or a re-registration naturally starts clean.

---

## R12 — Testing strategy under the Integration-First constitution

**Decision**: Test at the seams that already exist, with a real TCP peer rather
than a mocked transport.

- **Liveness/reconnect logic**: extract the ping-and-decide step into a small
  pure function over (last ping sent, pending CSeq, now) → `PingVerdict`, unit
  tested directly. This is state, not I/O, so a real-component test is trivial.
- **Detection end-to-end**: stand up a real `TcpListener` acting as a P-CSCF,
  let a `SipTransport` connect to it, answer one OPTIONS, then close abruptly,
  and assert the next ping is scored dead. Real sockets, no mock — the
  constitution's Principle I is satisfied without a hardware carrier.
- **Listener death**: `spawn_gm_server` against a real port, force the accept
  loop's fatal path, assert `is_alive()` flips.
- **Metrics + alert pairing**: extend `tests/test_vowifi_health_metrics.rs`
  (145 lines, the closest analog) and `tests/test_ingest_critical_alerts.rs`
  for the new category's failure/recovery transitions and one-per-episode
  property.
- **Config**: `tests/test_config.rs` + `tests/test_config_docs.rs` — the latter
  enforces that every config key is documented, so the new alert key will fail
  the suite until `docs/configuration.md` is updated.

**Not synthetically reproducible**: SC-010. The original incident was a live
carrier resetting an idle Gm connection some minutes after registration. The
synthetic tests bound the logic; only a hardware re-run of the T072 pass-1
scenario confirms the fix.
