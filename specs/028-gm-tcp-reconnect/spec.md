# Feature Specification: Carrier Signaling Connection Liveness & Automatic Reconnect

**Feature Branch**: `028-gm-tcp-reconnect`
**Created**: 2026-08-07
**Status**: Draft
**Input**: User description: "address the gap identified and triaged at docs/plans/vowifi-gm-tcp-reconnect.md, clarify any open questions"

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A line whose carrier signaling connection dies silently recovers on its own (Priority: P1)

A line is registered with the carrier and idle — nobody is calling in or out. The connection that carries signaling to the carrier is dropped without either side saying so (a network reset, a NAT expiring an idle flow, the carrier's own signaling gateway closing it). Today the system does not notice: the reader for that connection ends quietly, the line keeps reporting itself as registered, and the line is simply dead — no incoming call can be delivered and no outgoing call can be placed on it — until either someone tries a call and it fails mid-way, or up to roughly an hour passes and the next scheduled registration renewal happens to rebuild the connection as a side effect. In the observed incident the line stayed dead until the process was restarted by hand.

**Why this priority**: This is the entire failure. Everything else in this feature is about being able to see it; this is about it not happening. A line that is dead but reports itself healthy is worse than a line that is visibly down, because the operator's own monitoring agrees that everything is fine while calls silently go nowhere.

**Independent Test**: Bring a line up, let it register, then kill its carrier signaling connection from outside the process without a graceful close. Confirm that within a bounded, known time the system notices, rebuilds the connection on its own, and the line can place and receive calls again — with no restart and no operator action.

**Acceptance Scenarios**:

1. **Given** a registered, idle line, **When** its carrier signaling connection is dropped without notice, **Then** the system detects the drop within the stated detection window and re-establishes the connection automatically.
2. **Given** the connection has been re-established, **When** an inbound call arrives or an outbound call is placed on that line, **Then** it completes normally, exactly as it would have before the drop.
3. **Given** a registered line whose connection is healthy, **When** time passes with no calls, **Then** nothing about the line's behavior, registration, or call handling changes compared to today.
4. **Given** a line whose connection dropped, **When** the first re-establish attempt itself fails (the carrier is unreachable at that moment), **Then** the system keeps retrying on a backing-off schedule rather than giving up, and recovers as soon as the carrier is reachable again.

---

### User Story 2 - A drop during an active call does not break the call in progress (Priority: P1)

A call is up on a line. Media flows on its own separate path, so the call's audio is unaffected by the signaling connection's state — but the signaling connection is what will eventually carry the call's teardown. If liveness checking or reconnection fires in the middle of a call without regard for it, it can tear down the transport the live call still needs.

**Why this priority**: Same priority as Story 1 because a fix that recovers dead lines but disrupts live calls is a net regression. The system already has an established "maintenance yields to a call in progress" discipline for scheduled renewals; this must follow it rather than inventing an exception.

**Independent Test**: Establish a call on a line, then trigger the liveness/reconnect machinery during the call, and confirm the call's audio and its normal teardown are unaffected — and that the deferred maintenance runs once the call ends.

**Acceptance Scenarios**:

1. **Given** a call is in progress on a line, **When** a connection health check or reconnect would otherwise run, **Then** it is deferred until the call ends, and the deferral is visible in status as a deliberate hold rather than a stall.
2. **Given** a call is in progress and the signaling connection genuinely dies mid-call, **When** the call ends and its teardown signaling fails to send, **Then** the existing reactive recovery still applies and the line is restored — this feature does not regress that path.
3. **Given** maintenance was deferred for a call, **When** that call ends, **Then** the held connection check/reconnect runs promptly rather than waiting for the next full poll cycle.

---

### User Story 3 - Operators can see the connection's health, not just the registration's (Priority: P2)

An operator checking a line's status today sees its registration state, and — on the Wi-Fi calling path — whether the underlying tunnel is up. Neither of those tells them whether the connection that actually carries signaling is alive. In the observed incident, status reported the line as registered the whole time it was dead.

**Why this priority**: Without this, Story 1's recovery is invisible: an operator cannot distinguish "healthy" from "currently reconnecting" from "has been failing to reconnect for twenty minutes," and cannot confirm after the fact that recovery worked. This is also what makes the failure diagnosable the *next* time it appears in a new form.

**Independent Test**: Drop a line's connection and, while it is reconnecting, query status and scrape the metrics endpoint — confirm both report the connection as not-up and reconnecting, and that both return to healthy once it recovers.

**Acceptance Scenarios**:

1. **Given** a line whose connection is healthy, **When** an operator queries status, **Then** the connection is reported as up, alongside the registration and tunnel state already shown.
2. **Given** a line whose connection has dropped and is being re-established, **When** an operator queries status, **Then** it is reported as reconnecting, including since when.
3. **Given** the same line, **When** an automated monitor scrapes the metrics endpoint, **Then** the connection's up/down state is available there per line, without needing to parse status text or wait for a notification.
4. **Given** the connection recovers, **When** status is queried and metrics are scraped, **Then** both reflect the healthy state again, consistently.

---

### User Story 4 - Sustained failure to reconnect raises a proactive alert (Priority: P3)

A line's connection drops and the system cannot get it back — the carrier is refusing, the tunnel underneath is gone, the network is partitioned. Retrying quietly forever means the operator finds out when someone reports that calls aren't working.

**Why this priority**: This closes the loop opened by Story 3 — visibility only helps if somebody looks. The system already escalates sustained registration loss and tunnel failure this way; a signaling connection that cannot be restored belongs in the same category and should reuse the same channel and the same failure/recovery pairing.

**Independent Test**: Make a line's connection unrecoverable, confirm exactly one alert fires once the failure is sustained past the alert threshold; then restore reachability and confirm a matching recovery notice fires.

**Acceptance Scenarios**:

1. **Given** a line whose connection cannot be re-established, **When** the failure persists past the alert threshold, **Then** exactly one alert is sent through the existing alerting channel, identifying which line.
2. **Given** a connection that drops and is re-established on the first or second attempt, **When** recovery completes inside the alert threshold, **Then** no alert is sent — a routine self-healed drop is not an incident.
3. **Given** an alert has already fired for a line, **When** its connection is eventually re-established, **Then** a matching recovery notification is sent through the same channel, mirroring the existing registration-loss and tunnel-failure pairing.
4. **Given** a connection that repeatedly drops and recovers over a short period, **When** each episode resolves inside the threshold, **Then** the operator is not flooded with one alert per flap.

---

### Edge Cases

- **The connection is dead but the process does not realize it, because a second connection is still alive.** A line's signaling runs over two separate connections — the one the line registered over, and the one the carrier opens to deliver things it originates. Today the death of the first is masked by the second still being open: the internal "everything is disconnected, exit and let the supervisor restart us" safety net never fires. Detection must be per-connection, not "at least one is alive."
- **The reverse case: the carrier-facing listener dies but the line-originated connection is fine.** The line would then be able to place calls but never receive one, and would look entirely healthy from the outbound side. In scope (FR-021): each half is detected and recovered independently.
- **Re-establishing the connection succeeds, but signaling still does not work.** The connection is rebuilt on top of a still-live security association negotiated at registration time. If that association is what actually expired, a successful reconnect is a false recovery — the line looks healthy and is still dead. Recovery must be confirmed by something that proves signaling works end-to-end, not merely by the connection opening.
- **The drop happens while a registration renewal is already due or in flight.** Two mechanisms would both try to rebuild the same transport. Only one should act; they must not race or double-reconnect.
- **The health check itself is what the carrier objects to.** Probing too often, or in a way the carrier treats as misbehavior, risks the registration it is meant to protect. Probe frequency must be bounded and the added signaling volume must stay negligible.
- **A drop is detected at the exact moment a call is being set up.** Neither the in-flight call setup nor the recovery should be lost; one must yield to the other deterministically.
- **The process is restarted or the line re-registers from scratch while a reconnect is pending.** Any recorded "reconnecting since" state and any pending alert must not survive into the new registration as a stale failure.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: System MUST actively determine, on a bounded recurring interval, whether each registered line's carrier signaling connection is still alive — rather than only discovering its death when the system next happens to send something on it.
- **FR-002**: The liveness determination MUST be able to distinguish a live connection from one that has been dropped without notice by the far end or the network, including the case where the far end sends nothing at all on a healthy idle connection.
- **FR-003**: System MUST detect a dropped connection within approximately two minutes — bounded, and materially shorter than the scheduled registration-renewal period (currently up to ~55 minutes, which is the status quo this feature replaces).
- **FR-004**: On detecting a dropped connection, System MUST attempt to re-establish it automatically, without waiting for the next scheduled registration renewal and without any operator action or process restart.
- **FR-005**: Re-establishment attempts MUST be rate-limited by a backing-off retry schedule, reusing the retry discipline the system already applies to failed registration renewals rather than introducing a second, independent backoff scheme.
- **FR-006**: System MUST NOT run a liveness check or a re-establishment attempt in a way that disrupts a call in progress on that line; such maintenance MUST be deferred under the same "maintenance yields to an active call" discipline the scheduled renewal path already follows, and the deferral MUST be reported as deliberate.
- **FR-007**: Deferred connection maintenance MUST run once the call that deferred it ends, rather than being dropped or delayed until the next full poll interval.
- **FR-008**: System MUST NOT regress the existing reactive recovery path (rebuilding the transport when sending a call's teardown fails); that path MUST continue to work for the mid-call case.
- **FR-009**: When a re-establishment attempt succeeds, System MUST confirm that signaling actually works over the new connection before reporting the line healthy again — a connection that opens but over which signaling still fails MUST NOT be reported as recovered.
- **FR-010**: If re-establishment repeatedly fails, System MUST escalate to a full re-registration of that line — renegotiating the security association underneath and rebuilding the transport wholesale — rather than retrying the same failing rebuild indefinitely.
- **FR-010a**: Escalation MUST be confined to the affected line: it MUST NOT terminate the process, and MUST NOT interrupt any other line's registration or in-progress calls.
- **FR-010b**: If the escalated re-registration itself fails, System MUST report the line as failed (FR-012, FR-013) and alert (FR-014), and MUST continue re-attempting on the backing-off schedule so the line can still self-heal when the network recovers, without a manual restart.
- **FR-011**: System MUST NOT allow the liveness/reconnect mechanism and the scheduled registration-renewal mechanism to act on the same line's transport concurrently; when both are due, exactly one MUST proceed and the other MUST yield.
- **FR-012**: System's status-query output MUST report each line's signaling-connection health alongside the registration and tunnel state it already shows — at minimum: up, reconnecting (with the time since the drop was detected), and failed.
- **FR-013**: System MUST expose each line's signaling-connection up/down state as a metric on the existing metrics endpoint, consistent with how per-line registration and tunnel health are already exposed there.
- **FR-014**: System MUST send an alert through the existing alerting channel when a line's signaling connection has been unrecoverable for longer than a sustained-failure threshold, identifying the affected line; and MUST NOT alert for a drop that self-heals inside that threshold.
- **FR-015**: System MUST send exactly one failure alert per failure episode — repeated failed retries within one episode, and repeated short-lived drop/recover flaps, MUST NOT produce a flood of notifications.
- **FR-016**: When a line that already alerted recovers, System MUST send a matching recovery notification through the same channel, mirroring the existing registration-loss and tunnel-failure failure/recovery pairing.
- **FR-017**: A line that re-registers from scratch, or a process that restarts, MUST NOT carry stale "reconnecting since" state or a pending unresolved alert episode into the new registration.
- **FR-018**: For a line whose connection is healthy, System MUST show no observable change in behavior — no added call-setup latency, no change to registration or renewal timing, and no change to status output beyond the new health field.
- **FR-019**: The additional signaling traffic introduced by liveness checking MUST be negligible relative to the line's existing signaling volume, and MUST NOT exceed roughly 30 probe exchanges per line per hour, so it cannot be mistaken by the carrier for abusive behavior.
- **FR-020**: System MUST apply this feature to both Wi-Fi calling lines and cellular-data calling lines, consistently across detection, re-establishment, status, metrics, and alerting — neither transport may be left with the gap open.
- **FR-021**: System MUST cover both connections of a line's signaling pair: the line-originated connection it registered over, and the carrier-facing inbound listener whose death would leave the line able to place calls but never receive one. Each MUST be detected, reported, and recovered independently of the other's state.
- **FR-022**: A liveness probe MUST be treated as failed both when it cannot be sent and when it is sent but goes unanswered within the normal signaling response timeout — the second case being the one an open-but-dead connection produces.

### Key Entities

- **Signaling Connection**: The connection carrying a registered line's signaling to and from the carrier. A line has two: the one it registered over (used for anything the line originates) and the carrier-opened one (used for anything the carrier originates, including incoming calls). Their lifetimes are independent of each other and of the security association underneath them.
- **Connection Health**: The per-line, per-connection state exposed to operators, the metrics endpoint, and the alerting channel — at minimum: up, reconnecting (since a given time), and failed (re-establishment exhausted or escalated).
- **Failure Episode**: One continuous span from the first detected drop to the eventual confirmed recovery, however many retries it contains. Alerting is scoped to the episode, not to individual retries, so that one episode produces at most one failure notice and one recovery notice.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A line whose signaling connection is dropped without notice is detected as unhealthy within ~2 minutes in 100% of trials, whether or not the other connection of the pair is still alive, and for either connection of the pair.
- **SC-002**: A line whose connection is dropped while the carrier remains reachable is fully restored — able to place and receive calls — with zero manual restarts and zero operator actions, in 100% of such trials.
- **SC-003**: The time from an unnoticed drop to a restored line is reduced from up to ~55 minutes (or indefinite, if no renewal happens to fix it) to under ~3 minutes: the detection window plus one re-establishment attempt.
- **SC-003a**: A line whose security association is gone — where rebuilding the connection alone cannot work — is restored by escalation to re-registration, without the process exiting and without any other line's call being dropped.
- **SC-004**: No call in progress is dropped, muted, or has its teardown broken by the liveness or reconnect machinery, across repeated trials of a check firing during a call.
- **SC-005**: An operator can determine a line's connection health from a single status query, without cross-referencing logs, in 100% of the drop/reconnect/failed states.
- **SC-006**: An automated monitor scraping the metrics endpoint can detect 100% of down or reconnecting lines without parsing status text and without depending on the notification channel.
- **SC-007**: One unrecoverable failure episode produces exactly one failure notification and, on recovery, exactly one recovery notification — never zero, never a repeated stream.
- **SC-008**: A drop that self-heals within the sustained-failure threshold produces zero notifications.
- **SC-009**: Lines whose connections stay healthy show no measurable change in call setup time, registration timing, or call success rate compared to today.
- **SC-009a**: Liveness probing adds no more than ~30 probe exchanges per line per hour, and a healthy line's registration is never lost as a result of probing.
- **SC-010**: The scenario that originally surfaced this gap — a line up for some minutes after registration, then its connection reset — no longer leaves the line dead, confirmed on real hardware.

## Assumptions

- **Detection window**: ~2 minutes (Clarifications, Q2). A line is therefore never dead for materially longer than that window plus one re-establishment attempt.
- **Sustained-failure alert threshold**: Longer than the detection window and longer than a few reconnect attempts, so that ordinary transient drops that self-heal never page anyone. Its exact value is a planning-phase decision, consistent with how the existing registration-loss and tunnel-failure thresholds were chosen.
- **Probe interval is a fixed constant, not configuration.** The ~2-minute interval ships as a constant alongside the existing registration and renewal timing constants. Making it configurable was considered and deliberately deferred — it adds a config surface and a validation path for a value with no evidence yet that it needs per-carrier tuning. If a carrier turns out to object to the probe rate, that is the point to revisit it.
- **Reuse over invention**: The existing alerting channel, the existing per-line metrics endpoint, the existing status-query output, the existing renewal backoff, and the existing "maintenance yields to an active call" policy are all reused. This feature adds a new failure category to those mechanisms rather than introducing a parallel notification path, a second backoff scheme, or a separate health surface.
- **Re-establishment does not itself repair the security association underneath.** It rebuilds the connection on top of whatever association the registration already negotiated. If that association is itself what expired, re-establishment is expected to fail, and repair comes from the escalation in FR-010 — a full re-registration, which is the path that already knows how to renegotiate one. This is why FR-009's "confirm signaling actually works" matters: without it, a rebuild over a dead association reports a false recovery.
- **This is distinct from startup discovery retry.** The existing discovery-retry feature covers hardware that had not enumerated yet at startup; it has no interaction with an already-registered line's live connection and does not cover this gap.
- **Live verification is the real confirmation.** This failure was only ever observed on real hardware, never reproduced synthetically. Synthetic tests bound the behavior; a hardware re-test of the original scenario is what closes it.

## Clarifications

### Session 2026-08-07

- Q: How should the system detect that an idle carrier signaling connection has died — an application-level periodic probe, or an OS-level socket keepalive with tuned timers? → A: Application-level probe. It gives a bounded, application-controlled detection window instead of depending on OS defaults, is testable against a fake transport, and — unlike a socket-level keepalive — proves that *signaling* still works end-to-end rather than only that the socket is open (which is what FR-009 requires). The mechanism is a request the carrier is obliged to answer, sent on an idle timer; a failed send or an unanswered probe is treated as an unambiguous dead connection.
- Q: How quickly must a silently-dropped connection be noticed, given that recovery time is roughly this window plus one reconnect attempt? → A: About two minutes. This puts the worst-case dead-line duration at ~2–3 minutes, comfortably inside the "some minutes" the observed incident reported, at a cost of roughly 30 extra signaling messages per line per hour — negligible against an hour-long registration lifetime.
- Q: When re-establishment keeps failing — for example because the security association underneath is itself gone, so rebuilding the connection can never succeed — what should happen? → A: Escalate to a full re-registration for that line, which renegotiates a fresh security association and rebuilds the transport wholesale. Explicitly *not* chosen: exiting so the supervisor restarts the process, because that would drop every other line's in-progress calls to fix one broken line; and not retrying indefinitely, because a line whose association is gone would stay dead until the next scheduled renewal.
- Q: What scope should this cover — which transports, and which of the two connections in a line's signaling pair? → A: Both transports (Wi-Fi calling and cellular-data calling lines), since they share the same registration and dispatch machinery and scoping to one would leave the identical latent gap open on the other. And both connections: the line-originated connection that was actually observed to die, *and* the carrier-facing inbound listener, whose death is the symmetric blind spot — it would leave a line able to place calls but never receive one, and is invisible today for exactly the same reason.
