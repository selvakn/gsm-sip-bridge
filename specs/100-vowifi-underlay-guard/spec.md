# Feature Specification: Hold VoWiFi recovery while the path to the ePDG is down

**Feature Branch**: `099-pbx-reject-reason-header` (written on the current branch at the maintainer's request; no branch of its own yet)  
**Created**: 2026-10-20  
**Status**: Draft  
**Input**: User description: "VoWiFi underlay guard (#141), plus the half-open IKE_SA (CONNECTING) misread fix. Stay on the current branch, don't create a new one, and don't commit."

## Background

#141 asks for a "network guard" for VoWiFi. When the Wi-Fi or the internet behind it goes away, the bridge should hold its IMS state and carry on when the network returns, instead of restarting everything. The reporter suggested a periodic ping. This spec keeps the goal but not the ping.

**What happens today (measured 2026-10-20, Vodafone rig, the 098 probes-off build).** Two 330s outages were injected on the ePDG's outer address. One was a black-holed route, the "Wi-Fi/route gone" case. The other dropped inbound packets from the ePDG, the "upstream dead, Wi-Fi up" case. The supervisor behaved the same in both:

| Time into outage | What happened |
|---|---|
| 62–80s | The IMS agent marks its only P-CSCF unavailable (Gm keepalive / transport failure). |
| 80–86s | The next supervisor tick rebuilds the tunnel as `PcscfPoolExhausted`, and the agent is killed. |
| then every ~80s | The new attempt is half-open (IKE_SA `CONNECTING`). The supervisor reads that as "IKE_SA listed, CHILD_SA missing", re-initiates, and on the second strike (`ChildSaStuck`) terminates the in-flight attempt and starts another. The agent is killed again. |
| ~4m25s | A false "tunnel flapping (3 rebuilds within 300s)" alert. |
| restore +5–9s | Tunnel back: once through a supervisor re-initiate that happened to coincide with the restore, once through charon's own IKE_SA_INIT retransmit. |
| restore +15–18s | IMS registered. |

In 330s the line made 4 IKE_SA attempts and was killed 3–4 times, and an operator got a false alert. Every attempt was futile, because nothing the bridge does can fix a dead uplink. Recovery itself is not the problem: the line comes back within about 20s of the network returning, and charon would have done that unaided (`keyingtries = 0`).

**What strongSwan does and does not do.** Read from the pinned osmocom fork (`022f3e515`) and confirmed in both runs:

- charon did not notice either outage. It logged no roam, stale or "no route" events, and kept sending NAT-T keepalives into the black hole (`error writing to socket: Invalid argument`). In the upstream-dead run its sends simply succeeded. The bridge cannot use charon's own state to learn the uplink is down.
- With MOBIKE off, which we must keep (Vodafone's ePDG advertises MOBIKE and then ignores its exchanges, killing healthy tunnels), charon never defers retransmits for lack of a route. DPD counts down and destroys the IKE_SA after ~165s whatever the route.
- When the path returns through a different local address, charon re-authenticates from scratch. A router reboot or power loss usually means a new IKE_SA regardless of what the bridge does.

**Standards and prior art.**

- **RFC 7296 §2.4:** a peer may be concluded dead only "when repeated attempts to contact it have gone unanswered", never "based on any routing information". Local routing information is a hint that should trigger a liveness check. Receipt of any protected message, ESP included, proves liveness.
- **RFC 4555 §3.5:** unreliable information "SHOULD be treated only as a hint", followed by a liveness check.
- **3GPP TS 24.302 §7.2.2A:** a liveness failure discards the SAs. No re-establishment timer is specified for that case. Backoff timers (§7.2.2.2) apply only to explicit IKE_AUTH rejections.
- **Android IWLAN (`IwlanDataService`, current `main`):** on loss of the underlying network it keeps established tunnels, force-closing only those already being torn down. While the network is down it refuses to start new IKE (`RESULT_ERROR_TEMPORARILY_UNAVAILABLE`). Its signal is the platform's local view of the network, not a probe of the ePDG.

The guard follows that model:
- it learns of the outage from local state and from the ePDG's own silence, never from new probe traffic;
- it treats local state only as grounds to hold back, never as proof the ePDG is dead;
- it leaves re-establishment to charon's own retries.

**Relationship to #120.** #120 is removing the supervisor's P-CSCF probe; 096, 097 and 098 delivered its other replacements. With the probe gone, the agent's pool-exhausted verdict becomes the first trigger in an outage, and it acts in one tick with no debounce. That makes this guard more necessary, not less. The guard adds no traffic, so #120's goal of no non-handset-like traffic is unaffected. This feature should land before the probe is removed.

**The half-open misread (User Story 3).** Since 087, an IKE_SA in `CONNECTING` deliberately reads as "CHILD_SA missing". The aim was that an initiate in flight from the previous tick takes the two-strike CHILD_SA path rather than being re-fired, with an agent restart, every tick. The outage runs show the cost:
- the two strikes span ~60s, while charon's own IKE_SA_INIT retry cycle runs ~165s;
- so the second strike terminates an attempt charon is still working on, and kills the agent again.

This happens whenever the ePDG is slow or unreachable, not only during an outage, so it is fixed in its own right here.

## Clarifications

### Session 2026-10-20

- Q: Should the half-open (`CONNECTING`) misread fix be part of this feature? → A: Yes.
- Q: Should the line be held by an external control (a pause the network manager could call)? → A: No. The guard decides by itself; no new control surface.
- Q: charon has no command to send one liveness check on demand. Patch the fork, wait for charon's own DPD, use a rekey, or skip the check? → A: Patch our strongSwan fork (or add a vici command) so the supervisor can send one DPD on demand. SC-004's +30s bound stays.
- Q: Should operators be able to turn the guard off? → A: Yes. Add `[vowifi].underlay_guard`, default on; off restores today's behaviour exactly, and startup logs which is in effect.
- Q: What should a held line's alerts say? → A: The existing tunnel-down/unhealthy alert fires as today, but while the line is held its text adds "uplink down (no route / no response), recovery on hold". No new alert type.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - An uplink outage doesn't churn the line (Priority: P1)

The host loses its path to the ePDG. Either the route or Wi-Fi is gone, or Wi-Fi stays up but nothing comes back from the internet. The bridge recognises this within about a minute and puts each affected line into a held state. While held it:
- does not tear down or re-initiate the tunnel itself;
- does not restart the IMS agent;
- does not raise a flapping alert.

The status command and metrics show the line as held because its uplink is down, and say which of the two causes applies. When the path returns, the line re-establishes and re-registers promptly, the hold clears, and normal supervision resumes.

**Why this priority**: it is the whole of #141. It removes repeated agent kills, futile SIM authentications and false alerts from every outage. The Jio Pi has one every night (its ~03:00 uplink loop).

**Independent Test**: on a live rig, repeat the two spike outages (black-holed ePDG route; dropped inbound from the ePDG), each for 330s, and compare against the baseline above.

**Acceptance Scenarios**:

1. **Given** a healthy registered line, **When** the host's route to that line's ePDG disappears, **Then** within 60s the line is held with cause "no route", and from then until the route returns the supervisor issues no tunnel teardown, no re-initiate and no agent restart for the line.
2. **Given** a healthy registered line, **When** packets from the ePDG stop arriving while the route stays up, **Then** the line is held with cause "no response" once the ePDG's silence is established (FR-004), and the same no-action guarantee applies from then on.
3. **Given** a held line, **When** the IMS agent marks every P-CSCF unavailable, **Then** no `PcscfPoolExhausted` rebuild is issued, and once the hold ends those marks don't trigger one.
4. **Given** a held line, **When** the outage lasts long enough that the existing flapping rule would have fired, **Then** no flapping alert is raised. The line's existing down/unhealthy reporting continues to show it as down, and the existing down alert, when its threshold is reached, says the uplink is down, with the cause and that recovery is on hold.
5. **Given** a held line, **When** the path to the ePDG returns, **Then** the tunnel is re-established and the line is registered within 30s of the restore, with no manual action. The hold clears and is logged with its duration and cause.
6. **Given** a host with lines on two carriers, **When** only one carrier's ePDG becomes unreachable, **Then** only that carrier's lines are held.

---

### User Story 2 - A real carrier-side fault is still repaired (Priority: P1)

The guard must never hide a fault the bridge can fix. When the ePDG is still answering but the IMS side is broken, recovery runs exactly as today:
- the only P-CSCF has died (Vodafone, 097);
- the route inside the tunnel's namespace is lost (096);
- the carrier deleted the IKE_SA.

**Why this priority**: a guard that holds when it shouldn't turns a 5-minute repair into an indefinite outage. That is worse than the churn it removes.

**Independent Test**: the 097 Vodafone single-P-CSCF black-hole (the P-CSCF only, the ePDG left reachable) and the 096 netns route deletion, both run with this feature. Each must recover within its existing measured bound and never show a hold.

**Acceptance Scenarios**:

1. **Given** the line's only P-CSCF is black-holed while the ePDG still answers, **When** the agent marks the pool exhausted, **Then** the line is not held and the `PcscfPoolExhausted` rebuild happens. If the ePDG's responsiveness had to be checked first, the rebuild is delayed by at most 30s.
2. **Given** the default route inside the line's namespace is deleted, **When** the next tick runs, **Then** the existing millisecond repair happens, whatever the hold state.
3. **Given** charon dies, vici breaks, or the tunnel interface vanishes, **When** the next tick runs, **Then** the existing structural recovery runs, whatever the hold state.
4. **Given** the ePDG sends a DELETE for the IKE_SA while it is otherwise reachable, **When** the IKE_SA is gone, **Then** recovery proceeds as today.

---

### User Story 3 - A slow or in-flight IKE_SA setup is left to finish (Priority: P1)

When the supervisor sees the line's IKE_SA half-open, charon is mid-negotiation and retrying on its own. The supervisor leaves it alone:
- no re-initiate on top of it;
- no terminate under it;
- no agent restart.

It intervenes only if that same attempt is still half-open after charon's own retry schedule should have finished it. This applies whether or not the line is held.

**Why this priority**: it is a bug that reproduces on its own, whenever the ePDG answers slowly or not at all, and it causes most of the churn measured above. It is independently shippable, which is why it is P1 alongside User Story 1.

**Independent Test**: unit tests over the supervisor's per-tick decision with a scripted half-open listing that persists for 1, 2, … N ticks. Live, the outage runs of User Story 1 show no terminate of a `CONNECTING` IKE_SA within its allowed window.

**Acceptance Scenarios**:

1. **Given** the line's IKE_SA is listed as half-open and charon has sent or received an IKE message for it within the allowed window, **When** a steady-state tick runs, **Then** the supervisor issues no initiate, terminate or agent restart. It logs once per attempt that it is waiting for the negotiation.
2. **Given** a half-open attempt that has had no IKE activity for longer than the allowed window, **When** a tick runs and the line is not held, **Then** the supervisor terminates it and re-initiates, logged as a stuck negotiation, distinct from a missing CHILD_SA.
3. **Given** an IKE_SA that is established but has no usable CHILD_SA, **When** ticks run, **Then** the existing two-strike CHILD_SA recovery (086, #107) is unchanged.
4. **Given** a half-open attempt completes, **When** the next tick runs, **Then** the line proceeds as an ordinary recovery, re-syncing the P-CSCF and restarting the agent once.

---

### User Story 4 - An operator can see that the uplink, not the carrier, was at fault (Priority: P2)

After an outage, an operator can tell from the status command, the metrics and the logs:
- that the line was held because the uplink was down;
- for how long, and with which cause;
- that nothing was torn down meanwhile.

These outages are kept separate from carrier-side recoveries and dead-peer verdicts.

**Why this priority**: today an uplink outage reads as a carrier fault, with recoveries, dead-peer counts and a flapping alert. Sorting the two apart is what makes the 098 dead-peer metric and #120's soak data meaningful. It is P2 because the behaviour change in User Story 1 stands without it.

**Independent Test**: integration test through the real control socket and metrics registry. A scripted hold, entered and cleared, appears in status and metrics as specified. Live, the same is checked after each outage run.

**Acceptance Scenarios**:

1. **Given** a held line, **When** the operator runs the VoWiFi status command, **Then** it shows the line as held, with the cause and how long it has been held.
2. **Given** a hold has ended, **When** metrics are scraped, **Then** a per-line counter of holds by cause has gone up by one, and cumulative held time has grown by the hold's duration.
3. **Given** a hold starts and ends, **When** the logs are read, **Then** there is exactly one line for entering it (cause and evidence) and one for leaving it (duration and what ended it).
4. **Given** a dead-peer IKE_SA loss during a hold, **When** it is classified (098), **Then** it is not counted as a real dead peer. It is reported against the uplink instead.

---

### Edge Cases

- **The ePDG itself is down, but the internet works.** From the host this looks the same as "upstream dead". Holding is still correct: each line's ePDG address is fixed for the life of the container, so a rebuild would go to the same silent address. Re-selecting an ePDG is out of scope.
- **The outage outlasts the maximum hold.** After 15 minutes held, the line gets one ordinary recovery attempt. If the path is still down it is held again. A permanent outage therefore costs one agent restart and one rebuild per 15 minutes, not one every 80s, and no hold can last forever.
- **A call is active when the uplink drops.** The call fails regardless. The hold does not end it any sooner or cleanly. The agent is not restarted while held, so it can report the call's end on its own terms.
- **The path returns through a different local address** (new DHCP lease, other Wi-Fi). charon re-authenticates. That is expected, logged as an ordinary re-establishment, and still counts as the end of the hold.
- **A short blip** (shorter than the hold debounce). No hold starts. If it outlasts the agent's own detection, recovery runs as today.
- **The route is fine and inbound ESP is merely quiet** (an idle line between keepalives). The line is never held on quietness alone. A hold for "no response" needs an unanswered request (FR-004).
- **Some lines are held and others are healthy.** Holds are per line. A healthy line's supervision is unaffected.
- **The supervisor restarts during a hold.** The hold state is not persisted. The guard re-evaluates from scratch, and the line is held again if the outage persists.
- **swu engine lines.** Out of scope. Behaviour unchanged.
- **IKE_SA rekey or CHILD_SA rekey in flight during an outage.** charon's own retransmits fail and its SA eventually goes. The guard holds any supervisor-initiated recovery but does not interfere with charon's own handling.

## Requirements *(mandatory)*

### Functional Requirements

**Underlay state**

- **FR-001**: For every strongSwan line, the supervisor MUST maintain an underlay verdict for that line's ePDG: *up*, *down (no route)* or *down (no response)*. Lines sharing an ePDG address MUST share the verdict.
- **FR-002**: *Down (no route)* MUST be derived only from local host state: whether the host has a usable route and source address to the line's ePDG. It MUST be confirmed on 2 consecutive evaluations at least 10s apart before a hold starts.
- **FR-003**: *Down (no response)* MUST be derived only from evidence that the ePDG has stopped answering protected traffic. That means IKE requests to the ePDG going unanswered, with no inbound ESP or IKE from it over the same period. The determination MUST NOT require any new periodic traffic.
- **FR-004**: When the supervisor is about to take a reachability-inferred recovery (FR-007) and the evidence for FR-003 is not yet conclusive, it MUST establish the ePDG's responsiveness before acting:
  - **ePDG answers:** a protected answer or inbound ESP arrives within 30s. The recovery proceeds.
  - **ePDG silent:** no protected message arrives within 30s. The line is held with cause "no response".
  
  Establishing this MUST cost at most one protected IKE liveness exchange (RFC 7296 §2.4, an empty INFORMATIONAL) on the line's existing IKE_SA per suspicion, and no other new traffic. It MUST NOT be a rekey or any exchange that creates or replaces an SA.
- **FR-004a**: The strongSwan build MUST let the supervisor trigger that single liveness exchange on demand for a named connection, and read whether it was answered. A small change to the project's strongSwan fork, or a vici command added to it, is in scope. Its retransmission MUST follow charon's normal schedule, and an unanswered exchange MUST NOT by itself delete the IKE_SA any sooner than charon's DPD would.
- **FR-005**: Local state MUST only ever be used to hold back supervisor actions. It MUST NOT cause any teardown, re-initiate, agent restart or dead-peer verdict.
- **FR-006**: The verdict MUST return to *up* as soon as local state is healthy again (for "no route") or anything protected arrives from the ePDG (for "no response").

**Behaviour while held**

- **FR-007**: While a line is held, the supervisor MUST NOT issue these reachability-inferred recoveries:
  - `PcscfUnreachable`, for as long as the P-CSCF probe exists;
  - `PcscfPoolExhausted`;
  - the re-initiate for a missing IKE_SA, except as FR-010a allows;
  - the CHILD_SA-missing and stuck-CHILD_SA recoveries;
  - the stuck-rekey rebuild.
- **FR-008**: While a line is held, the supervisor MUST NOT restart that line's IMS agent for any reason listed in FR-007.
- **FR-009**: The structural recoveries MUST run regardless of the hold: process death, vici failure, tunnel interface vanished or unavailable, and namespace default route missing.
- **FR-010**: charon's own retries MUST be left to run during a hold. The supervisor MUST NOT terminate charon's in-flight attempts or reconfigure charon during a hold.
- **FR-010a**: If a held line has no IKE_SA at all (for example, charon's DPD cleared it), the supervisor MUST start exactly one initiation, with no agent restart, and then leave charon's own retries to carry it. Without this, nothing would be sent to the ePDG, and a "no response" hold could never see the answer that ends it. It MUST NOT initiate while any IKE_SA for the line exists, half-open included.
- **FR-011**: P-CSCF unavailability marks set by the IMS agent during a hold MUST NOT trigger a `PcscfPoolExhausted` rebuild after the hold ends. Only marks set after the hold ended count.
- **FR-012**: A line MUST NOT stay held longer than 15 minutes continuously. At that bound it gets one ordinary recovery pass, after which it may be held again on fresh evidence.
- **FR-013**: When a line's verdict returns to *up*, the supervisor MUST, on that same evaluation, ensure an IKE_SA initiation is under way for the line if none is established or in flight. It MUST NOT tear down an IKE_SA that charon has already re-established. The one exception to FR-015 is a half-open IKE_SA left over from a *no route* hold: its retransmits all went nowhere and charon's backoff can leave 76s between them, so that IKE_SA is terminated and re-initiated on the evaluation where the route returns (found live on the first Vodafone run, where it cost 46s of SC-003's 30s). A *no response* hold ends only when the ePDG has answered something, so its IKE_SA is progressing and is left alone.

**Half-open IKE_SA**

- **FR-014**: The supervisor MUST classify a listed IKE_SA that is still being negotiated (half-open) distinctly from an established IKE_SA without a usable CHILD_SA.
- **FR-015**: For a half-open IKE_SA, the supervisor MUST take no initiate, terminate or agent-restart action while charon is still working on it, meaning it has sent or received an IKE message for that connection within the last 200s. charon's longest gap between retransmits is ~75s, and with unlimited keying tries it keeps retrying the same half-open IKE_SA indefinitely, so its age alone says nothing.
- **FR-016**: A half-open IKE_SA with no IKE activity for longer than FR-015's 200s MUST, if the line is not held, be terminated and re-initiated. It MUST be logged and reported as a stuck negotiation, a recovery reason distinct from the CHILD_SA ones.
- **FR-017**: The established-IKE_SA, missing-CHILD_SA path (086, #107) MUST keep its existing two-strike behaviour.

**Observability**

- **FR-018**: The VoWiFi status command MUST show, per line, whether it is held, the cause, and for how long.
- **FR-019**: The metrics endpoint MUST expose, per line:
  - a counter of holds by cause;
  - a counter of cumulative held seconds;
  - a gauge of whether the line is held now.
  
  The counters MUST NOT be rewound by a supervisor restart, like the existing tunnel-recovery counters.
- **FR-020**: Entering and leaving a hold MUST each produce one log line. Entering names the cause and evidence; leaving names the duration and what ended it (path returned, maximum hold reached).
- **FR-021**: The flapping alert MUST NOT count recoveries skipped, or charon attempts made, during a hold. The existing down/unhealthy reporting MUST continue to report a held line as down, and the existing tunnel-down/unhealthy alert MUST still fire on its existing thresholds.
- **FR-022**: A dead-peer IKE_SA loss while the line is held MUST NOT be counted as a *real* dead peer in the 098 classification. It MUST be attributed to the uplink.
- **FR-023**: No new Discord alert type is added. When the existing tunnel-down/unhealthy alert fires for a line that is held, its text MUST add that the uplink is down, the cause (no route / no response) and that recovery is on hold. When the line is not held, the alert's text MUST be unchanged.

**Scope and documentation**

- **FR-024**: Lines on the swu engine and VoLTE lines MUST behave exactly as before.
- **FR-025**: One deployment-wide setting, `[vowifi].underlay_guard` (boolean, default on), MUST control the guard. When it is off, every behaviour in FR-001–FR-013 and FR-018–FR-022 MUST be absent, and supervision MUST match the pre-feature behaviour exactly. The half-open fix (FR-014–FR-017) is not affected by this setting. Startup MUST log once whether the guard is on. A non-boolean value MUST fail config loading with an error naming the setting. The hold bound, debounce and liveness wait stay fixed values, documented in the operations guide, not settings.
- **FR-025a**: The configuration reference and example config MUST document `[vowifi].underlay_guard`. The repository's config-docs test MUST cover it.
- **FR-026**: The operations guide MUST explain the hold: what it means in the status output, which recoveries it holds back, which it never holds, and how to read the new metrics. The changelog MUST record both the guard and the half-open fix.
- **FR-027**: The live verification (SC-001 to SC-005, and SC-008) MUST be recorded in the feature's live-verification notes before merge.

### Key Entities

- **Underlay verdict**: per ePDG address: *up*, *down (no route)* or *down (no response)*, with the evidence and when it was established.
- **Hold**: per line: start time, cause, and the recoveries skipped during it. It ends when the verdict returns to *up* or the 15-minute bound is reached.
- **Half-open attempt**: per line: the IKE_SA's identifier and the time of the connection's last IKE activity. It is used to tell an attempt still in progress from a stuck one.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: In a 330s black-holed-ePDG-route outage and a 330s dropped-inbound-from-ePDG outage, each run on Vodafone and on Jio:
  - from the moment the hold starts until the path returns there are **zero** supervisor-issued tunnel teardowns, re-initiates and agent restarts;
  - there are zero flapping alerts;
  - any tunnel-down alert that fires names the uplink as the cause.
  
  Baseline: 4 IKE_SA attempts, 3–4 agent restarts and 1 false flapping alert per run.
- **SC-002**: In the same runs, the hold starts within 60s of the route loss (no-route case) and within 120s of the inbound drop (no-response case).
- **SC-003**: In the same runs, the line is registered within 30s of the path being restored. Baseline: 15–18s; the guard must not make recovery slower.
- **SC-004**: The 097 single-P-CSCF black-hole on Vodafone recovers within its existing measured time plus at most 30s, and the 096 namespace-route deletion is repaired within one tick. Neither shows a hold.
- **SC-005**: In an outage longer than 15 minutes, the line receives exactly one ordinary recovery attempt per 15 minutes held, and recovers within 30s of the path returning.
- **SC-006**: An idle healthy line generates no additional outer packets because of this feature: an idle hour's packet count to and from the ePDG is unchanged within normal variance.
- **SC-007**: In unit tests, a half-open IKE_SA with recent IKE activity causes no initiate, terminate or agent restart however long it lasts, and one with no activity beyond the allowed window causes exactly one stuck-negotiation recovery.
- **SC-008**: With `[vowifi].underlay_guard` off, the two outage runs of SC-001 reproduce the pre-feature baseline (a teardown at ~80s, then rebuilds), apart from the half-open fix.

## Assumptions

- The 098 probes-off configuration is the target. The guard must also behave correctly while the P-CSCF probe still exists, by holding `PcscfUnreachable` during a hold. The probe's removal (#120) is a separate feature, sequenced after this one.
- Each line's ePDG address is resolved once at startup and fixed for the container's life, so a rebuild during an outage can never reach a different ePDG. Re-resolving or re-selecting the ePDG is out of scope.
- charon's retransmit schedule (5 retransmits, ~165s) and `retry_initiate_interval` (10s) stay as they are. Raising the retransmit count so short blips don't cost the IKE_SA (RFC 7296's "a dozen times over several minutes") is a separate decision, because it also slows detection of a really dead ePDG. It is out of scope.
- MOBIKE stays disabled. It is the standards answer to path changes, but Vodafone's ePDG breaks it (see the swanctl template).
- Requesting the carrier's liveness timeout (TS 24.302 `TIMEOUT_PERIOD_FOR_LIVENESS_CHECK`) is out of scope. Neither carrier sends it unasked, and requesting it needs a change to the strongSwan fork.
- charon has no command today to send one liveness exchange on demand, so FR-004a adds one to the project's strongSwan fork. The plan's research chooses its shape (a vici command, or an existing hook exposed). The fork is already built from source in the image, so this adds no new dependency.
- Hold state is in-memory only. After a restart the guard re-derives it from current evidence.
- Live tests use the existing rigs: Vodafone on the local host, Jio on the remote Pi. Outages are injected on the host (route black hole; inbound drop with a dedicated, auto-removed firewall table), as in the spikes.
