# Feature Specification: Disable Circuit-Switched Handling

**Feature Branch**: `026-disable-circuit-switched`
**Created**: 2026-08-04
**Status**: Draft
**Input**: User description: "lets add a flag in the configuration to disable the circuit switching all together. when vowifi or volte is enabled, we dont want the cs to continue probing in the background."

## Overview

On a deployment where every call is carried over VoWiFi or VoLTE, the circuit-switched (CS) path still runs: it scans the USB serial bus at startup, re-scans on a fixed interval forever, opens candidate modem ports, and issues AT commands to decide whether each one is a usable CS card. None of that work can produce a call on such a deployment, but it does produce cost — log noise, AT traffic that can collide with a port another subsystem is mid-transaction on, restart churn, and misleading health/alert signals about "cards" the operator never intended to use.

This feature adds an explicit configuration flag, `[cs].enabled`, that turns the circuit-switched call path off. When it is off, the bridge does no modem discovery, opens no serial port for CS purposes, and carries no CS calls. The shared services hosted alongside it — the metrics endpoint, the operator control interface, the message store, and call history — keep running exactly as before, so the VoWiFi and VoLTE subsystems that depend on them are untouched.

Two consequences are worth stating up front, because they are what make the flag mean what it says rather than being a half-measure:

- **The circuit-switched host stops presenting a telephone-facing side of its own.** With the path off there is nothing behind a trunk registration, so establishing one would advertise capacity that cannot exist and invite an upstream telephone system to route calls into a dead end. When VoWiFi or VoLTE is enabled, that subsystem already owns the telephone-facing side and is entirely unaffected.
- **Hardware reserved for the circuit-switched path is released.** The rule that decides which subsystem gets which modem stops reserving voice-capable modems for a path that is switched off, so VoWiFi and VoLTE may use them.

## Clarifications

### Session 2026-08-04

- Q: Where should the flag live in the configuration? → A: A new `[cs]` section with a single `enabled` key.
- Q: With `[cs].enabled = false`, should audio-capable modems that have no explicit `[[vowifi.line]]` override become VoWiFi/VoLTE candidates? → A: Yes — with the circuit-switched path off, the role assignment offers every AT-capable modem to VoWiFi/VoLTE; nothing is reserved for a path that is off.
- Q: With `[cs].enabled = false` and neither VoWiFi nor VoLTE enabled, should the circuit-switched host still bring up its own telephone-facing SIP side? → A: No — it stays fully down (no upstream trunk registration, no host-side registrar), folded into the existing "another subsystem owns the telephone side" condition. A registrar-only deployment is consequently not supported.
- Q: With `[cs].enabled = false`, how should the metrics endpoint represent the circuit-switched path? → A: Omit the circuit-switched series entirely and export one status gauge stating the path is deliberately disabled, so "disabled" is distinguishable from "daemon down" without existing threshold rules firing.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Silence background modem probing on a VoWiFi-only deployment (Priority: P1)

An operator runs a bridge whose calls are all carried over VoWiFi. They set the new flag to off, restart the service, and the bridge stops touching modems on the circuit-switched path entirely — no startup scan, no periodic re-scan, no AT traffic — while their desk phones stay registered and VoWiFi calls keep flowing in both directions.

**Why this priority**: This is the entire point of the feature and the only story that must ship for it to have value. Everything else is refinement of the same switch.

**Independent Test**: Configure a system with the flag off and VoWiFi on, start it, and observe over several re-scan intervals that no circuit-switched discovery or modem-port access occurs, while an inbound and an outbound VoWiFi call both complete normally.

**Acceptance Scenarios**:

1. **Given** the circuit-switched flag is off, **When** the bridge starts, **Then** it performs no circuit-switched modem discovery and reports at startup that the circuit-switched path is disabled.
2. **Given** the circuit-switched flag is off and the bridge has been running longer than several re-scan intervals, **When** the operator inspects activity on the serial bus, **Then** no circuit-switched re-scan or AT probe has occurred at any point.
3. **Given** the circuit-switched flag is off, **When** a desk phone registers and places a call over VoWiFi, **Then** registration and the call succeed exactly as they did with the flag on.
4. **Given** the circuit-switched flag is off, **When** a call arrives over VoWiFi, **Then** it rings the registered phone exactly as it did with the flag on.
5. **Given** the circuit-switched flag is off and a modem is physically plugged in mid-run, **Then** the bridge does not claim it or probe it for circuit-switched use.

---

### User Story 2 - Existing deployments upgrade with no behaviour change (Priority: P1)

An operator upgrades a running circuit-switched deployment without editing their configuration. The flag defaults to on, so their bridge behaves exactly as it did before the upgrade: cards are discovered, calls are bridged, nothing needs changing.

**Why this priority**: Equal in priority to Story 1 — a flag that silently changes the behaviour of existing deployments on upgrade is a regression, regardless of how useful the new mode is. This constraint shapes the design, so it must be specified and tested alongside the feature itself.

**Independent Test**: Take an existing production configuration verbatim, run it against the new build, and confirm identical circuit-switched discovery, call bridging, and message forwarding.

**Acceptance Scenarios**:

1. **Given** a configuration that predates this feature (no flag present), **When** the bridge starts, **Then** the circuit-switched path runs exactly as before.
2. **Given** a configuration with the flag explicitly on and VoWiFi also on, **When** the bridge starts, **Then** both the circuit-switched path and VoWiFi run together, as they do today.

---

### User Story 3 - Health, metrics, and operator commands stay coherent with the path off (Priority: P2)

With the circuit-switched path off, an operator checking health, metrics, or running a card-management command gets a clear "circuit-switched path is disabled" answer rather than an error, an empty result they have to interpret, or a warning about missing hardware they deliberately excluded.

**Why this priority**: The feature is usable without this — the bridge still works — but without it the operator is left with alarming or ambiguous signals about a subsystem they intentionally turned off, which erodes trust in the health signals that matter.

**Independent Test**: With the flag off, run the health check and each card-management command, and scrape the metrics endpoint; confirm each communicates "disabled" unambiguously and none reports a fault.

**Acceptance Scenarios**:

1. **Given** the flag is off, **When** the operator runs a health check, **Then** the circuit-switched path reports as intentionally disabled, not as unhealthy or degraded.
2. **Given** the flag is off, **When** the operator requests the list of circuit-switched card slots, **Then** the response states the path is disabled rather than returning an unexplained empty list.
3. **Given** the flag is off, **When** the operator issues a card restart or mode-change command, **Then** the command is rejected with a message naming the disabled flag as the reason.
4. **Given** the flag is off, **When** metrics are scraped, **Then** no circuit-switched card is reported as failed, given-up, or in an active alert state.
5. **Given** the flag is off, **When** the bridge is running, **Then** no scheduled card-restart activity is attempted.
6. **Given** the flag is off, **When** metrics are scraped, **Then** no circuit-switched card series appear at all, and the status indicator reports the path as disabled.
7. **Given** the flag is off and an alert rule exists that fires when no circuit-switched card is ready, **When** the bridge runs for a full day, **Then** that rule does not fire and needed no edit to stay quiet.

---

### User Story 4 - Reuse circuit-switched hardware for VoWiFi (Priority: P3)

An operator with a voice-capable modem that the bridge has always reserved for circuit-switched use turns the flag off. On restart that modem is offered to VoWiFi and, if its SIM is ready and a line slot is free, becomes a VoWiFi line — without the operator having to pin it with an explicit line override.

**Why this priority**: The lowest priority of the four because an operator can already achieve this by pinning the modem explicitly. Without it, though, turning the flag off strands hardware: the modem is no longer probed but nothing else may claim it either, which is a confusing half-state to leave behind.

**Independent Test**: On a system whose only modem is voice-capable and has no explicit line override, turn the flag off, restart, and confirm the modem is resolved as a VoWiFi line and carries a call.

**Acceptance Scenarios**:

1. **Given** the flag is off and a voice-capable modem has no explicit line override, **When** the bridge starts, **Then** the modem is offered to VoWiFi rather than reserved for the circuit-switched path.
2. **Given** the flag is on and the same modem and configuration, **When** the bridge starts, **Then** the modem is reserved for the circuit-switched path exactly as it is today.
3. **Given** the flag is off and freeing modems yields more candidates than the configured line limit allows, **When** the bridge starts, **Then** the limit is honoured, the excess candidates are not promoted, and startup is not treated as an error.

---

### Edge Cases

- **Nothing left to carry calls**: the flag is off and neither VoWiFi nor VoLTE is enabled. The bridge starts and serves metrics and stored history, but warns prominently that no call path is active and that no telephone-facing registration will be established. Degenerate but valid, so not a fatal error.
- **Telephone system expects a trunk that never appears**: the flag is off on a deployment that previously registered a trunk upstream. The registration is deliberately not established (FR-009a), so the upstream system marks the trunk down and stops routing calls to it. This is the intended outcome — the alternative is calls routed to a bridge with no path to carry them — and the startup log names the flag as the reason (FR-009b).
- **Message forwarding orphaned**: message forwarding is enabled, the flag is off, and no VoWiFi or VoLTE line exists to receive messages. The bridge starts and warns at startup that message forwarding has no active source. Messages already in the store remain readable.
- **Circuit-switched-specific settings present but inert**: the configuration still carries re-scan interval, concurrency, retry/back-off, scheduled-restart, or per-card audio settings while the flag is off. These are accepted without error and silently have no effect — an operator flipping the flag back on must find their tuning intact.
- **Single-card command-line override with the path off**: an operator passes an explicit serial-port/audio-device override on the command line while the flag is off. The override does not resurrect the circuit-switched path; the bridge reports the conflict and continues with the path disabled.
- **Modem freed for another subsystem**: a modem that the circuit-switched path used to claim is no longer claimed once the flag is off, and becomes eligible for VoWiFi or VoLTE without further configuration — including a voice-capable modem that the assignment rule reserves for circuit-switched use while the flag is on.
- **More candidates than lines allowed**: with the flag off, freeing previously reserved modems offers VoWiFi or VoLTE more candidates than its configured maximum line count permits. The excess candidates are simply not promoted to lines, exactly as they are today when candidates exceed the maximum; this is not an error.
- **Freed modem is unusable**: a modem freed by turning the flag off fails the readiness filter (no ready SIM, unreadable card). It is skipped like any other failed candidate and does not prevent the remaining lines from starting.
- **Flag flipped back on**: turning the flag on again and restarting restores full circuit-switched behaviour with no residual state from the disabled run.
- **Alert suppression**: card-lifecycle and registration-loss alerts scoped to circuit-switched cards do not fire while the path is off.

## Requirements *(mandatory)*

### Functional Requirements

**The flag itself**

- **FR-001**: The configuration MUST provide a single boolean flag, `[cs].enabled`, that enables or disables the circuit-switched call path as a whole. `cs` stands for circuit-switched; the section exists solely to hold this flag.
- **FR-002**: `[cs].enabled` MUST default to enabled when the key or the whole `[cs]` section is absent, so that a configuration written before this feature produces identical behaviour after upgrade.
- **FR-002a**: The `[cs]` section MUST be accepted by configuration validation. A configuration naming `[cs]` or `[cs].enabled` MUST NOT be rejected as an unknown key.
- **FR-003**: `[cs].enabled` MUST be independent of `[vowifi].enabled` and `[volte].enabled` — enabling VoWiFi or VoLTE MUST NOT implicitly change it, and any combination of the three MUST be accepted as valid configuration.
- **FR-004**: The bridge MUST report the flag's effective value at startup, in a form an operator can find in the logs without enabling debug logging.
- **FR-004a**: The existing `[modules]` section MUST keep its current name and keys. It holds circuit-switched card-pool tuning that `[cs].enabled` governs; the configuration reference MUST cross-reference the two so an operator looking in either place finds the other.

**What stops when the flag is off**

- **FR-005**: With the flag off, the bridge MUST NOT perform circuit-switched modem discovery at startup.
- **FR-006**: With the flag off, the bridge MUST NOT perform any periodic re-scan for newly attached modems on the circuit-switched path, for the entire lifetime of the process.
- **FR-007**: With the flag off, the bridge MUST NOT open any modem serial port, nor issue any AT command, for circuit-switched purposes.
- **FR-008**: With the flag off, the bridge MUST NOT bridge, originate, or answer any circuit-switched call.
- **FR-009**: With the flag off, the bridge MUST NOT attempt any scheduled or on-demand circuit-switched card restart, including the periodic scheduled-restart cycle.
- **FR-009a**: With the flag off, the circuit-switched host MUST NOT bring up its own telephone-facing SIP side: it MUST NOT register a trunk with an upstream telephone system, and MUST NOT start a registrar of its own. This MUST hold whatever the telephone-side settings say, and MUST reuse the existing "another subsystem owns the telephone side" suppression rather than introducing a second, parallel mechanism.
- **FR-009b**: The suppression in FR-009a MUST be logged at startup with the flag named as the reason, so an operator who loses an expected registration can tell immediately why.
- **FR-009c**: FR-009a MUST NOT affect a telephone-facing side hosted by the VoWiFi or VoLTE subsystem — when either is enabled, it owns that side and is unaffected by the flag (see FR-011).
- **FR-010**: With the flag off, the bridge MUST NOT claim any modem for the circuit-switched path.
- **FR-010a**: With the flag off, the rule that assigns discovered modems to subsystems MUST offer every successfully probed modem to VoWiFi or VoLTE, including modems that would be reserved for the circuit-switched path when the flag is on. Nothing is reserved for a path that is disabled.
- **FR-010b**: FR-010a MUST NOT bypass the existing per-subsystem admission rules — the readiness filter, explicit line overrides, and the configured maximum line count all still apply and still bound how many candidates become active lines.
- **FR-010c**: With the flag on, the modem assignment rule MUST be unchanged from its current behaviour.

**What keeps running when the flag is off**

The circuit-switched path is hosted alongside several services that are *not* part of it. Which of those services the circuit-switched host process owns depends on the deployment: when VoWiFi or VoLTE is enabled, that subsystem already owns the telephone-facing side, and the circuit-switched host's own telephone-facing side is already inert regardless of this flag. The requirements below are therefore stated as system-level guarantees — the observable behaviour must not change — rather than as claims about which process does what.

- **FR-011**: With the flag off **and VoWiFi or VoLTE enabled**, the telephone-registration service that subsystem hosts MUST continue to accept and maintain registrations from desk phones exactly as it does with the flag on. (With no call path enabled there is no such service — see FR-009a and FR-023.)
- **FR-012**: With the flag off, outbound dialing over VoWiFi and VoLTE MUST remain available and functional.
- **FR-013**: With the flag off, inbound VoWiFi and VoLTE calls MUST continue to ring registered phones.
- **FR-014**: With the flag off, the metrics endpoint MUST continue to serve, including all metrics the VoWiFi and VoLTE subsystems report into it.
- **FR-015**: With the flag off, the operator control interface MUST continue to accept connections and serve every command that is not specific to circuit-switched cards — including the metric reports the VoWiFi and VoLTE subsystems send through it, which are their only route to the metrics endpoint.
- **FR-016**: With the flag off, the message store MUST remain open and readable, and message forwarding from VoWiFi or VoLTE lines MUST continue to work.
- **FR-017**: With the flag off, call-history recording for VoWiFi and VoLTE calls MUST continue unchanged.

**Operator-facing behaviour**

- **FR-018**: With the flag off, the health check MUST report the circuit-switched path as intentionally disabled and MUST NOT report it as unhealthy, degraded, or failed.
- **FR-019**: With the flag off, a request for the list of circuit-switched card slots MUST return a response that states the path is disabled, distinguishable from "enabled but no cards found".
- **FR-020**: With the flag off, any command that targets a circuit-switched card MUST be rejected with a message that names the flag as the reason, rather than failing obscurely or hanging.
- **FR-021**: With the flag off, no circuit-switched card MUST be reported in a failed, given-up, or active-alert state by the metrics or alerting subsystems.
- **FR-021a**: With the flag off, circuit-switched card metrics MUST NOT be exported at all — neither with a value nor as a zero-valued series. This is what keeps an existing "no cards ready" style threshold rule from firing continuously against a path the operator switched off on purpose.
- **FR-021b**: The metrics endpoint MUST export a single status indicator reporting whether the circuit-switched path is enabled, present in both states. Its purpose is to let a consumer distinguish "deliberately disabled" from "process down or scrape broken", which the absence of the circuit-switched series alone cannot express.
- **FR-021c**: With the flag on, the exported circuit-switched metrics MUST be unchanged from their current set, names, and labels, so existing dashboards and alert rules keep working untouched.
- **FR-022**: With the flag off, circuit-switched card-lifecycle alerts MUST NOT fire.

**Configuration validity and warnings**

- **FR-023**: The bridge MUST start successfully when the flag is off and neither VoWiFi nor VoLTE is enabled, emitting a prominent startup warning that no call path is active and that no telephone-facing registration will be established. This configuration serves metrics and stored history only; it is degenerate but valid, and MUST NOT be a fatal error.
- **FR-024**: The bridge MUST emit a prominent startup warning when message forwarding is enabled, the flag is off, and no VoWiFi or VoLTE line is configured to supply messages.
- **FR-025**: Circuit-switched-specific configuration settings MUST remain valid and MUST NOT cause a configuration error when the flag is off; they simply have no effect.
- **FR-026**: A command-line single-card override supplied while the flag is off MUST NOT re-enable the circuit-switched path; the bridge MUST report the conflict and honour the flag.

**Documentation**

- **FR-027**: The configuration reference MUST document `[cs].enabled`, its default, what stops and what keeps running when it is off, and the recommendation to turn it off on VoWiFi-only or VoLTE-only deployments.
- **FR-028**: The documentation MUST call out the two non-obvious consequences of turning the flag off, since neither is predictable from the flag's name: that the circuit-switched host establishes no telephone-facing registration of its own (FR-009a), and that modems otherwise reserved for the circuit-switched path become available to VoWiFi and VoLTE (FR-010a).
- **FR-029**: The metrics documentation MUST describe the status indicator from FR-021b and state which circuit-switched series disappear when the flag is off, so an operator can adjust dashboards and alert rules before flipping it.

### Key Entities

- **Circuit-switched path**: the subsystem that discovers modems, owns their serial ports and audio devices, and carries voice calls over the cellular circuit-switched network. This is the unit the flag governs.
- **Shared services**: the metrics endpoint, operator control interface, message store, and call history — hosted alongside the circuit-switched path but not part of it, and unaffected by the flag. On a VoWiFi or VoLTE deployment these are the *only* things the circuit-switched host process contributes beyond the circuit-switched path itself.
- **Telephone-facing side**: the registration service desk phones bind to and the outbound-dialing entry point. Owned by whichever subsystem is active — the VoWiFi or VoLTE subsystem when either is enabled, otherwise the circuit-switched host. This ownership rule already exists and is not changed by the flag.
- **Modem claim**: the exclusive ownership of a physical modem by exactly one subsystem. Turning the flag off removes the circuit-switched path from the set of possible claimants.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: On a VoWiFi-only deployment with the flag off, zero modem-probe operations occur over a 24-hour run — down from one full bus scan every re-scan interval (roughly 2,880 scans per day at the default 30-second interval).
- **SC-002**: An operator can disable the circuit-switched path by adding a single two-line section (`[cs]` and `enabled = false`) and restarting, with no other configuration edits required.
- **SC-003**: Every existing deployment configuration works unchanged after upgrade, with identical circuit-switched behaviour — verified by running the current production configurations against the new build.
- **SC-004**: With the flag off, inbound and outbound VoWiFi and VoLTE call success rates and setup times are indistinguishable from the same deployment with the flag on.
- **SC-005**: With the flag off, the health check and metrics report zero faults attributable to the absent circuit-switched path — no false alerts reach the operator's alert channel over a 24-hour run, with no alert-rule or dashboard edits required to achieve that.
- **SC-005a**: A metrics consumer can tell "circuit-switched path deliberately disabled" apart from "process down or scrape failing" from a single scrape, with no access to the configuration file or logs.
- **SC-006**: Startup log output on a flag-off deployment contains no circuit-switched discovery or modem-probe messages, making it obvious at a glance which paths are live.
- **SC-007**: An operator reading the configuration reference can determine what turning the flag off will and will not stop, without reading source code.

## Assumptions

- The flag is a plain on/off switch. There is no partial mode (for example, "messages only, no voice"); that was considered and explicitly excluded from scope.
- Message forwarding follows the circuit-switched path: with the flag off, no modem is held open for message polling on the circuit-switched side. VoWiFi and VoLTE lines retain their own independent message paths.
- The flag governs a running deployment's background behaviour. Diagnostic and discovery commands an operator invokes explicitly and interactively are out of scope — the flag suppresses *background* and *automatic* modem access, not an operator's deliberate one-off inspection.
- Turning the flag off does not require the deployment to have VoWiFi or VoLTE enabled, but with no call path enabled the result serves only metrics and stored history — it establishes no telephone-facing registration. A registrar-only deployment (phones binding to a bridge with no call path) is explicitly not supported.
- The flag takes effect at process start. Changing it at runtime is out of scope; a restart is required, consistent with how every other section of this configuration behaves.
- Existing circuit-switched-specific settings keep their current names, defaults, and validation rules. This feature adds a flag; it does not reorganise or rename anything around it.
- The existing rule that VoWiFi and VoLTE cannot both be enabled at once is unchanged by this feature.

## Out of Scope

- Runtime toggling of the flag without a restart.
- Any change to how VoWiFi or VoLTE themselves are enabled, supervised, or admitted as lines. The one deliberate exception is FR-010a: with the flag off, the shared modem-assignment rule offers them modems it would otherwise reserve for the circuit-switched path. Their own readiness filter, overrides, and line limits are untouched.
- A messages-only mode that keeps a modem open for message polling with voice disabled.
- Automatically deriving the flag's value from the VoWiFi or VoLTE flags — explicitly rejected in favour of an explicit, backward-compatible opt-out.
- Removing or renaming any existing circuit-switched configuration setting.
