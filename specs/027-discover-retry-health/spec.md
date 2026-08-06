# Feature Specification: Discovery Retry & Missing-Line Health Reporting

**Feature Branch**: `027-discover-retry-health`
**Created**: 2026-08-06
**Status**: Draft
**Input**: User description: "identify ways to harden the discover. if not in the auto discovery, if config is mentioned, it should be retried, and reported in the health of the system, if not working"

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A slow-enumerating configured modem still comes up on its own (Priority: P1)

An operator has explicitly configured a VoWiFi/VoLTE line (for example, a modem pinned to a specific serial port) in `config.toml`. On a container start, the underlying USB hardware for that line has not finished enumerating by the moment discovery runs — a transient timing race, not a real hardware failure. Today, discovery only ever runs once at startup, so a line missed in that single pass simply never exists for the rest of the container's life: no registration is ever attempted, and nothing about it appears in status output, even though the operator's configuration clearly says it should be there.

**Why this priority**: This is the concrete failure already observed in production: an EC20 modem's VoWiFi line was silently absent for an entire multi-hour container run, with zero log evidence it was even looked for, while the modem itself was working the whole time. Fixing this recovers real call-handling capacity without requiring anyone to notice and manually restart the container.

**Independent Test**: Configure a line pinned to a modem, start the system while that modem is deliberately made to enumerate a short time after container start, and confirm the line comes up and starts registering without any manual intervention (no restart, no operator action).

**Acceptance Scenarios**:

1. **Given** a line is explicitly configured but the matching hardware is not yet visible when the first discovery pass runs, **When** the hardware becomes visible and answers shortly afterward, **Then** the line is resolved and starts normally, without requiring a container restart.
2. **Given** a line is explicitly configured and its hardware is visible and answers on the very first discovery pass (today's working case), **When** discovery runs, **Then** the line resolves exactly as it does today, with no added delay.
3. **Given** two configured lines, one whose hardware is present immediately and one that appears late, **When** discovery runs, **Then** the immediately-available line starts and operates normally without waiting on the late one.

---

### User Story 2 - Operators can see, at a glance, which configured lines aren't actually running (Priority: P2)

An operator (or an on-call person reacting to a report that calls aren't going through) checks the system's status/health output. Today, a configured line that never got discovered is completely invisible — status output only ever describes lines that *did* resolve, and the container's own health check reports "healthy" as long as the lines it knows about are fine, with no way to tell that an entire expected line is missing. Diagnosing this currently requires manually cross-referencing `config.toml` against raw logs.

**Why this priority**: Without this, User Story 1's retry can still fail (real hardware fault, unplugged device, bad config) and nobody would know — the exact blind spot that made the original incident take a manual investigation to even locate. Visibility is what turns "silently broken" into "known and actionable."

**Independent Test**: Configure a line whose hardware is deliberately never made available, start the system, and confirm that both the status query tool and the container health check report that specific configured line as failed/missing — distinct from "not configured" and distinct from "healthy."

**Acceptance Scenarios**:

1. **Given** a configured line whose hardware never becomes discoverable, **When** an operator queries system status, **Then** that line is listed as configured-but-not-running, identified clearly enough to know which config entry it corresponds to.
2. **Given** the same situation, **When** the container's own health check runs, **Then** the overall result reflects the degraded state rather than reporting healthy.
3. **Given** a configured line that resolves successfully, **When** an operator queries system status, **Then** it is reported as healthy/running exactly as it is today, with no change in behavior for the working case.

---

### User Story 3 - The existing alert channel fires for a missing configured line (Priority: P3)

The system already proactively notifies the operator (via an existing alert mechanism) when a running line's registration or tunnel goes unhealthy for a sustained period. A configured line that never starts at all is arguably the more severe case, but today it produces no notification whatsoever — an operator only finds out by being told calls aren't working, or by manually inspecting status.

**Why this priority**: This closes the loop opened by User Story 2 — status visibility only helps if someone looks. Reusing the existing proactive-alert pattern means this failure mode gets the same treatment as other line-health problems the system already knows how to escalate.

**Independent Test**: Configure a line whose hardware never becomes discoverable, start the system, and confirm an alert notification is sent once the failure is confirmed (after retries are exhausted), without needing anyone to check status manually.

**Acceptance Scenarios**:

1. **Given** a configured line that fails to start even after retries, **When** the retry window elapses, **Then** an alert notification is sent through the existing alerting channel.
2. **Given** a configured line that initially fails to be discovered but succeeds during a retry, **When** it comes up successfully, **Then** no failure alert is sent for it.
3. **Given** a configured line that already triggered a failure alert, **When** its hardware later becomes available and the line starts successfully, **Then** a recovery notification is sent through the same alerting channel.

---

### Edge Cases

- What happens when the configured hardware is genuinely absent (unplugged, never connected) rather than just slow to enumerate? Retries must not continue forever — the system needs to eventually settle into a persistent "not found" state that stops consuming retry effort.
- What happens when a configured line's hardware is found and answers, but its SIM is not usable (already-detected today as a distinct condition from "not found at all")? This should also count as "not working" for status/health purposes, not just outright absence.
- What happens when more than one configured line is missing at the same time? Status and alerting must be able to identify each one individually, not just report a single generic degraded flag.
- What happens if the missing hardware appears *after* the system has already declared the line failed and alerted on it? The line should still be able to recover automatically, its degraded state should clear, and a recovery notification should be sent (matching the existing failure/recovered alert pairing).
- What happens to a line that was successfully running and later becomes unreachable during the container's life (for example, the modem is unplugged mid-run)? This is distinct from the startup-discovery problem this feature targets — see clarification below on whether it's in scope.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: System MUST be able to tell, for each line explicitly named in configuration, whether a discovery pass actually found and confirmed matching, working hardware for it.
- **FR-002**: When a configured line's hardware is not found (or found but not usable — e.g., unreadable SIM) on a discovery pass, System MUST retry discovery for that specific line rather than treating a single pass as final.
- **FR-003**: Retries for a still-missing configured line MUST be bounded in time; after the retry window elapses without success, the system MUST settle into a persistent "configured but failed" state for that line rather than retrying indefinitely.
- **FR-004**: If the configured hardware becomes available and usable during the retry window, System MUST resolve and start that line without requiring a manual restart or other operator action.
- **FR-005**: Retrying a missing configured line MUST NOT disrupt, delay, or re-probe any other line that has already been successfully discovered and started.
- **FR-006**: System's status-query output MUST include every line named in configuration, not only the lines that successfully resolved — a configured-but-not-running line MUST be shown as such, identifiable back to its configuration entry.
- **FR-007**: System's status-query output MUST distinguish, for a failed configured line, between "hardware never found" and "hardware found but not usable," to the extent that information is available.
- **FR-008**: System's container-level health check MUST report a degraded/unhealthy result when any explicitly configured line has failed to start, rather than reporting healthy on the basis of the lines it does know about.
- **FR-009**: System MUST notify through its existing alerting mechanism when a configured line's retries are exhausted and it settles into the failed state, matching how other sustained line-health failures are already escalated today.
- **FR-010**: System MUST NOT send a failure alert for a configured line that succeeds during its retry window — only lines that are still failed once the window elapses.
- **FR-011**: If a previously-failed configured line later becomes available, System MUST clear its degraded status/health-check state, MUST reflect recovery consistently across status-query output and health check, and — if a failure alert was already sent for it — MUST send a matching recovery notification through the same alerting mechanism, mirroring how registration-loss and tunnel-failure alerts already pair failure and recovery notices.
- **FR-012**: System MUST expose a configured line's failed-to-start state as a metric on the existing metrics endpoint, consistent with how other per-line health signals (registration, tunnel status) are already exposed there today.

### Key Entities

- **Configured Line**: An entry in configuration declaring an expected VoWiFi/VoLTE line (e.g., a pinned modem serial port, or a card/reader identifier), which exists independently of whether any discovery pass has yet found matching hardware for it.
- **Discovery Pass**: A single scan of currently available hardware, producing a set of resolved lines; today the system runs exactly one of these at startup.
- **Line Status**: The per-line state exposed to operators and to the health check — at minimum: running/healthy, retrying (not yet resolved but still within the retry window), and failed (retry window elapsed without success) — plus, where known, *why* it failed (not found vs. found-but-unusable).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A configured line whose hardware becomes available within the retry window comes up and starts operating automatically, with zero manual restarts needed, in 100% of such cases.
- **SC-002**: An operator checking system status can identify 100% of configured-but-not-running lines from that single check, with no need to cross-reference configuration against raw logs.
- **SC-003**: A configured line that never becomes available is reflected as unhealthy in the container's health check within the bounded retry window, every time.
- **SC-004**: A configured line that never becomes available triggers exactly one proactive alert notification per failure episode (not a flood of repeated alerts, and not silence).
- **SC-005**: Lines that resolve successfully on the first discovery pass show no observable change in startup time or behavior compared to today.
- **SC-006**: An automated monitoring system watching the existing metrics endpoint can detect 100% of configured-but-failed lines without needing to parse status-query text or wait for the Discord alert.

## Assumptions

- The retry window is a bounded, on-the-order-of-minutes duration intended to absorb ordinary USB/device enumeration delays (the observed real-world case), not to wait out a genuinely disconnected device indefinitely; its exact duration is a planning-phase decision, not fixed by this spec.
- "Retried" means the system keeps re-attempting *discovery* for the specific missing configured line (re-scanning for its hardware and re-probing it), not that it changes how an individual probe/AT exchange itself behaves.
- The existing alert mechanism (the one already used for registration-loss and tunnel-failure conditions) is reused for this new failure mode rather than a new notification channel being introduced.
- This feature applies to any explicitly configured line — whether pinned to a modem port or to a card/reader — not only the modem case from the original incident, since the operator's request refers to "config" generically.
- Retrying discovery for a missing line is safe to do concurrently with already-running lines' normal operation (i.e., it will not reopen ports or claim hardware that a running line already holds) — this is the same safety property discovery already has to satisfy today.

## Clarifications

### Session 2026-08-06

- Q: Should the retry-and-report behavior in this feature cover only the startup window (a line missing when the system starts, which may recover shortly after), or should it also continuously watch, for the entire life of the running system, for a line that later drops out (e.g., a modem physically unplugged mid-run) or a configured line that still hasn't appeared long after startup? → A: Startup-only — bounded retry window right after startup; a line that later drops out mid-run is a separate, already-partially-covered concern (existing registration/tunnel alerts) and out of scope here.
- Q: The system already exposes per-line health (registration, tunnel) as metrics on the existing metrics endpoint, alongside status-query output and Discord alerts. Should a configured line's failed-to-start state also be exposed as a metric there, or is status-query + health check + alert enough? → A: Yes — expose it as a metric too, consistent with every other line-health signal already surfaced that way.
- Q: The existing alert pattern (registration-loss, tunnel-failure) always pairs a failure alert with a matching "recovered" notice once the condition clears. Should a configured line that self-heals after its failure alert fired also get an explicit recovery notification, or should its degraded state just clear silently? → A: Yes — send a recovery notification, matching the existing failure/recovered pairing used for registration and tunnel alerts.
