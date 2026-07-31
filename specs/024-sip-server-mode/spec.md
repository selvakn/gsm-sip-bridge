# Feature Specification: SIP Server Mode

**Feature Branch**: `024-sip-server-mode`
**Created**: 2026-07-31
**Status**: Draft
**Input**: User description: "For listening as a SIP server as an alternate option to connecting to a PBX. This should allow small deployments to use this component as the server for IP phones to connect to. This would not be the default option and would be behind a flag."

## Clarifications

### Session 2026-07-31

- Q: Should the mode carry calls in both directions, or only the inbound direction the bridge supports today? → A: Inbound only — phones register and are rung; phone-originated dialling out through the mobile network is out of scope (no such capability exists today).
- Q: When a call arrives and several phones are registered, what should happen? → A: Ring one phone, named by configuration. Other phones may register but are never rung.
- Q: How should the bridge authenticate phones that register to it? → A: Digest authentication against per-account credentials held in configuration.
- Q: Which call paths should be able to ring a registered phone? → A: All three (circuit-switched, VoWiFi, VoLTE), by hosting the registrar in whichever component already owns the outbound call leg.
- Q: The registrar and the bridge's outbound calling leg cannot share one network port. How should the second port be handled? → A: The registrar takes the port phones default to; the operator is told, by a startup error, to move the bridge's own calling port.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Ring a desk phone with no PBX in the deployment (Priority: P1)

An operator runs a small site — one or two SIMs and a couple of desk phones —
and has no telephone system to point the bridge at. Today the bridge cannot
be used at all without standing up and maintaining a separate PBX purely to
receive its calls. They want to point the desk phone straight at the bridge
and have incoming mobile calls ring it.

**Why this priority**: This is the entire point of the feature. Without it, a
PBX remains a hard dependency for every deployment, however small.

**Independent Test**: Configure the bridge with the new mode enabled and one
phone account, point a single IP phone at the bridge's address, and place a
call to the SIM. The phone rings and audio flows both ways — with no PBX
present or configured anywhere.

**Acceptance Scenarios**:

1. **Given** the mode is enabled with one phone account configured and a
   phone successfully registered, **When** a call arrives on the mobile
   network, **Then** that phone rings and, once answered, carries two-way
   audio for the duration of the call.
2. **Given** such a call is in progress, **When** either party hangs up,
   **Then** both the mobile leg and the phone leg are torn down, exactly as
   they are when a PBX is in use today.
3. **Given** a call is ringing the phone, **When** the caller's number is
   available from the mobile network, **Then** the phone is presented with
   that number as the calling party.

---

### User Story 2 - Provision and re-provision phones safely (Priority: P2)

An operator adds a phone to the deployment, replaces a broken handset, or
takes one away. They want the bridge to accept only the phones they have
authorised, and to cope with handsets that come and go — a phone that is
unplugged and later plugged back in, or that moves to a different address on
the network, must resume receiving calls without anyone editing configuration
or restarting the service.

**Why this priority**: Without authentication, anyone on the network could
silently take over the line. Without a working registration lifecycle, the
mode would work once and then quietly stop, which is worse than not shipping
it.

**Independent Test**: Register a phone with correct credentials and confirm
it is accepted; retry with a wrong password and confirm it is refused;
unplug the phone, plug it back in, and confirm calls reach it again with no
operator action.

**Acceptance Scenarios**:

1. **Given** a phone presenting credentials that match a configured account,
   **When** it registers, **Then** the bridge accepts it and will direct
   calls to it.
2. **Given** a phone presenting a wrong password or an unrecognised account
   name, **When** it registers, **Then** the bridge refuses it, no call is
   ever directed to it, and the refusal is counted so an operator can see it.
3. **Given** a phone that was registered, **When** it moves to a different
   network address and registers again, **Then** subsequent calls go to its
   new address without operator action.
4. **Given** a phone that was registered, **When** it is switched off and its
   registration lapses, **Then** the bridge stops directing calls to it and
   an operator can see from the service's health surfaces that the phone is
   no longer reachable.

---

### User Story 3 - Use the mode on a VoWiFi or VoLTE deployment (Priority: P2)

An operator whose SIMs carry calls over VoWiFi or VoLTE, rather than over the
plain circuit-switched path, wants the same PBX-free operation. The choice of
how the call reaches the bridge from the carrier should not dictate whether a
PBX is required on the other side.

**Why this priority**: VoWiFi is the more reliable carrier path in practice,
so restricting the mode to circuit-switched calls would leave the most common
production configuration unable to use it. It is P2 only because it is not
needed to demonstrate the feature working end to end.

**Independent Test**: Enable the mode on a deployment configured for VoWiFi
(or VoLTE inbound bridging), register a phone, and place a call to the SIM.
The phone rings and carries audio, with no PBX configured.

**Acceptance Scenarios**:

1. **Given** the mode is enabled alongside VoWiFi or VoLTE inbound bridging,
   **When** a phone registers, **Then** it is accepted, exactly as it is on a
   circuit-switched deployment.
2. **Given** such a deployment with a phone registered, **When** a call
   arrives over the carrier's packet-switched path, **Then** the registered
   phone rings and carries two-way audio.

---

### User Story 4 - Diagnose a deployment that is not ringing (Priority: P3)

An operator whose phone is not ringing wants to tell, without packet capture,
whether the phone never registered, registered but was refused, or registered
and then lapsed.

**Why this priority**: Valuable for day-two operation but not required for the
feature to deliver its value.

**Independent Test**: Query the service's existing health and metrics surfaces
with no phone registered, with one registered, and after a refused attempt,
and confirm the three states are distinguishable.

**Acceptance Scenarios**:

1. **Given** the mode is enabled, **When** an operator inspects the service's
   metrics, **Then** they can see how many phones are currently registered and
   whether the phone designated to ring is among them.
2. **Given** a call arrives while the designated phone is not registered,
   **When** the operator inspects logs and metrics, **Then** the cause is
   stated plainly, names the account that was expected, and is counted
   separately from other call failures.

---

### Edge Cases

- **Designated phone not registered when a call arrives**: the mobile call is
  left to ring out rather than being answered into silence, matching what the
  bridge does today when it cannot place the outbound leg. The existing
  missed-call notification path therefore still reports it.
- **Two accounts configured with the same name**: refused at startup with a
  clear error, rather than leaving it undefined which one authenticates.
- **The designated phone name matches no configured account**: refused at
  startup — otherwise the mode would start cleanly and silently never ring.
- **A phone asks to stay registered for an implausibly short or long period**:
  the bridge negotiates it into a supported range and tells the phone what it
  actually granted, rather than accepting and quietly ignoring the request.
- **A phone re-sends a registration it already sent** (a normal consequence of
  packet loss): treated as the same registration, not as a new or conflicting
  one.
- **A phone explicitly un-registers**: honoured immediately; calls stop being
  directed to it.
- **A phone replays a previously valid authentication attempt**: refused.
- **A phone's authentication challenge has gone stale** because too much time
  passed: the phone is asked to retry in a way that does not prompt a human
  for a password.
- **A phone sends requests the bridge does not support** (dialling out,
  subscribing to presence, keepalives): each gets a definite answer, because
  silence causes phones to retransmit and then drop their registration.
- **Settings that would silently do nothing in this mode** (a PBX address, a
  fixed PBX destination): refused at startup rather than accepted and ignored.
- **The mode's listening port collides with the port the bridge already uses**
  for its own calling leg: refused at startup with a message naming the fix.

## Requirements *(mandatory)*

### Functional Requirements

#### Mode selection and configuration

- **FR-001**: The system MUST provide a SIP server mode that is disabled by
  default, so that existing deployments are unaffected by its introduction.
- **FR-002**: The system MUST allow the operator to configure a set of phone
  accounts, each with a name and a password, and MUST support supplying those
  passwords indirectly from the environment as it already does for other
  secrets.
- **FR-003**: The system MUST allow the operator to designate exactly one
  account as the one to ring.
- **FR-004**: The system MUST reject at startup, with an actionable message, a
  configuration in which the mode is enabled but no account is configured, two
  accounts share a name, or the designated account matches none of them.
- **FR-005**: The system MUST reject at startup, with an actionable message,
  any setting that would have no effect in this mode — specifically a PBX
  address, PBX credentials, or a fixed PBX destination — rather than accepting
  and ignoring it.
- **FR-006**: The system MUST reject at startup, with a message naming the
  remedy, a configuration in which the mode's listening port is the same as
  the port the bridge uses for its own outgoing calls.

#### Accepting phones

- **FR-007**: The system MUST accept registrations from IP phones on a
  configurable address and port, defaulting to the port IP phones use by
  convention.
- **FR-008**: The system MUST challenge every registration attempt and MUST
  accept it only when the phone proves knowledge of a configured account's
  password. Passwords MUST NOT be transmitted or required in clear text.
- **FR-009**: The system MUST refuse an unrecognised account name and an
  incorrect password indistinguishably to the phone, so that the mode cannot
  be used to discover which account names are valid, while still recording the
  two cases separately for the operator.
- **FR-010**: The system MUST reject a repeated or replayed authentication
  attempt.
- **FR-011**: The system MUST record, for each accepted phone, where to reach
  it and how long its registration remains valid, and MUST stop directing
  calls to it once that period lapses.
- **FR-012**: The system MUST negotiate a registration lifetime within a
  configurable supported range and MUST inform the phone of the value actually
  granted.
- **FR-013**: The system MUST honour an explicit un-registration immediately.
- **FR-014**: The system MUST treat a retransmitted registration as the same
  registration rather than as a new one.
- **FR-015**: The system MUST answer every request a phone sends it — including
  keepalives, presence subscriptions, and attempts to dial out — with a
  definite response, refusing those it does not support rather than ignoring
  them.

#### Placing calls to a phone

- **FR-016**: When a call arrives from the mobile network and the mode is
  enabled, the system MUST direct that call to the designated account's
  currently registered location instead of to a PBX.
- **FR-017**: The system MUST present the mobile caller's number to the phone
  by the same means it already uses when calling a PBX.
- **FR-018**: When a call arrives and the designated account has no valid
  registration, the system MUST leave the mobile call unanswered, log the
  cause naming the designated account, and count the occurrence separately
  from other call failures.
- **FR-019**: The system MUST support the mode on all three inbound call paths
  — circuit-switched, VoWiFi, and VoLTE — with no difference in behaviour
  visible to the phone.
- **FR-020**: The system MUST NOT support calls originated by a phone. Such an
  attempt MUST be explicitly refused and logged.
- **FR-021**: Call setup, audio handling, and teardown behaviour, once the
  destination has been determined, MUST be unchanged from the PBX case.

#### Observability

- **FR-022**: The system MUST expose how many phones are currently registered
  and whether the designated account is among them, as separate signals from
  the existing PBX-registration and carrier-registration health signals.
- **FR-023**: The system MUST count registration outcomes by category —
  accepted, challenged, refused for bad credentials, refused for an unknown
  account, refused as stale, refused for an unacceptable lifetime, and
  un-registered.

#### Compatibility

- **FR-024**: With the mode disabled, the system's behaviour MUST be
  byte-for-byte unchanged from before this feature, on all three call paths.

### Key Entities

- **Phone account**: an operator-configured identity a phone may claim,
  consisting of a name and a password. Purely configuration; never created at
  runtime.
- **Registration**: the record that a particular phone, having authenticated
  as a given account, is currently reachable at a particular network location
  until a particular time. Created and refreshed by the phone, expires on its
  own, and is replaced — not accumulated — when the same account registers
  again.
- **Designated account**: the single account name that inbound calls are
  directed to. One per deployment.
- **Authentication challenge**: a short-lived, single-use token the system
  issues to a phone so it can prove knowledge of its password without sending
  it. Expires on its own and cannot be reused.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A small deployment can take an incoming mobile call on an IP
  phone with no PBX present anywhere in the deployment.
- **SC-002**: An operator can go from a working PBX-less configuration file to
  a ringing phone by setting one flag, one account, and one designated name —
  and any mistake in that trio is reported at startup with a message that
  names the fix, never by silent failure to ring.
- **SC-003**: A phone that is power-cycled, or that changes network address,
  resumes receiving calls with no operator action and no service restart.
- **SC-004**: A phone presenting incorrect credentials never receives a call,
  and the operator can see the refusal in the service's metrics.
- **SC-005**: The three failure states an operator cares about — phone never
  registered, phone refused, phone registered then lapsed — are
  distinguishable from logs and metrics alone, without packet capture.
- **SC-006**: Every deployment that works today continues to work identically
  with the feature present and disabled, on all three call paths.
- **SC-007**: The mode's behaviour toward a phone is verified automatically on
  every commit, without requiring a physical phone, a SIM, or a modem.

## Assumptions

- **Phones and bridge share a local network.** Address translation between the
  phone and the bridge is out of scope; this targets small single-site
  deployments, which is the stated use case.
- **One phone rings.** Ringing several phones at once and letting the first to
  answer win is deliberately excluded, consistent with the bridge's existing
  one-call-at-a-time design.
- **No phone-originated calls.** The bridge has never been able to place a
  call out over the mobile network, and this feature does not add that. The
  mode is strictly about receiving.
- **The operator provisions phones by hand.** Automatic provisioning of
  handsets is out of scope.
- **Encrypted signalling to the phone is out of scope for this version.**
  Local-network deployments are assumed; the existing PBX-facing transport
  options are unaffected.
- **The designated account is fixed at startup.** Changing which phone rings
  requires a configuration change, as it does for the existing PBX
  destination.
- **The registrar's listening port and the bridge's own calling port must
  differ**, and the operator is expected to set the latter explicitly when
  enabling the mode. This is surfaced as a startup error rather than resolved
  silently, so the ports remain something an operator can reason about and
  configure a firewall around.
