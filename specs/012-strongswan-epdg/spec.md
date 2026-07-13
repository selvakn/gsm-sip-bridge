# Feature Specification: strongSwan-Based ePDG Tunnel (Option 2)

**Feature Branch**: `012-strongswan-epdg`
**Created**: 2026-07-13
**Status**: Draft
**Input**: User description: "Replace the SWu-IKEv2 Python dialer with strongSwan (osmocom
foss-ims-client wiki 'Option 2') as the ePDG tunnel engine for the VoWiFi-to-SIP bridge. The
end-to-end pipeline (tunnel → IMS registration → inbound call bridging via
vowifi-ims-agent/vowifi-sip-agent) is already proven on Option 1; this feature swaps the tunnel
layer for reliability: proper IKEv2 rekeying and re-authentication, dead-peer detection, MOBIKE,
and a network namespace that survives reconnects. EAP-AKA must keep authenticating against the
SIM that lives inside the EC200U modem via AT+CSIM (there is no PC/SC card reader). The existing
Rust agents must keep working unchanged. The SWu-IKEv2 path stays available as a deploy-time
fallback until strongSwan is proven against both carriers."

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Tunnel survives unattended long-running operation (Priority: P1)

As the operator of the VoWiFi bridge, I leave the container running unattended for days. The
ePDG tunnel renegotiates its keys on the carrier's schedule, detects and recovers from dead
peers, and never tears down the network namespace the bridge agents live in — so an inbound
VoWiFi call placed on day 2 is answered and bridged exactly like one placed 5 minutes after
startup.

**Why this priority**: This is the entire motivation for the swap. Option 1's dialer cannot
rekey, drops the tunnel when idle or when the IKE SA expires, and destroys/recreates the `ims`
network namespace on every reconnect — severing the veth link the bridge agents depend on. The
bridge is not production-trustworthy until the tunnel layer is.

**Independent Test**: Start the container with the strongSwan engine selected, confirm the
tunnel is up, wait through at least one full IKE rekey cycle (carrier-scheduled, typically
< 4 h) plus one forced network interruption, and verify the tunnel recovers with the namespace
and agent processes untouched.

**Acceptance Scenarios**:

1. **Given** the tunnel is established and both bridge agents are registered, **When** the
   carrier-scheduled IKE rekey time arrives, **Then** the tunnel rekeys in place, the network
   namespace and tunnel interface persist, and neither agent restarts or loses registration.
2. **Given** the tunnel is established, **When** the WAN path is interrupted for under a minute
   and then restored, **Then** the tunnel re-establishes automatically without the namespace
   being deleted, and the bridge agents recover their registrations without manual action.
3. **Given** the tunnel has been up for 24 hours with no calls, **When** an inbound VoWiFi call
   arrives, **Then** it is answered and bridged to the PBX within the same time bound as feature
   011 (5 s).

---

### User Story 2 - SIM authentication still uses the modem-resident SIM (Priority: P1)

As the operator, I don't own a PC/SC card reader and the SIM must stay in the EC200U modem
(the same SIM also serves the circuit-switched bridge). The strongSwan engine authenticates
EAP-AKA against that SIM through the modem's AT interface, both at initial connect and at every
re-authentication the carrier demands, without me extracting keys or moving the SIM.

**Why this priority**: Without SIM auth there is no tunnel at all; this is the one structural
piece Option 2's stock tooling does not provide (it expects a PC/SC reader), so it is the
feature's critical unknown.

**Independent Test**: With only the modem attached (no card reader), initiate the tunnel and
verify EAP-AKA succeeds against both test carriers; then keep the tunnel up past a carrier
re-authentication and verify it succeeds again unattended.

**Acceptance Scenarios**:

1. **Given** the SIM is in the modem and no PC/SC reader exists, **When** the tunnel is
   initiated, **Then** EAP-AKA completes using the SIM via the modem's AT interface and the
   tunnel establishes.
2. **Given** the tunnel is up, **When** the carrier requires IKE re-authentication (which
   re-runs EAP-AKA), **Then** authentication succeeds unattended and the tunnel continues.
3. **Given** the circuit-switched bridge daemon is simultaneously using other modem functions,
   **When** tunnel authentication needs the SIM, **Then** both subsystems continue to work
   (no permanent lockout of either).

---

### User Story 3 - Existing bridge agents work unchanged (Priority: P2)

As the maintainer, the vowifi-ims-agent and vowifi-sip-agent from feature 011 keep working
without code changes: they still find the P-CSCF address where they expect it, still find the
tunnel interface inside the `ims` namespace, and still reach each other over the veth link.

**Why this priority**: Protects the investment in the proven 011 pipeline and keeps this
feature's blast radius confined to the tunnel layer.

**Independent Test**: Bring the tunnel up with the strongSwan engine and verify both agents
start, register, and bridge a live inbound call with zero changes to their code or config
semantics.

**Acceptance Scenarios**:

1. **Given** the strongSwan engine established the tunnel, **When** the agents start, **Then**
   the IMS agent reads a valid P-CSCF address from the same location as before and registers
   over the tunnel.
2. **Given** both agents are running, **When** an inbound VoWiFi call arrives, **Then** it is
   bridged to the PBX end-to-end (signalling and two-way audio) exactly as with Option 1.

---

### User Story 4 - Deploy-time fallback to the old dialer (Priority: P3)

As the operator, I can select at deployment time which tunnel engine runs — the new strongSwan
engine or the existing SWu-IKEv2 dialer — so that if strongSwan misbehaves against a carrier I
can revert with a configuration change, not a rollback of the image.

**Why this priority**: De-risks the migration during the proving period against the two live
carriers; removable once strongSwan is proven.

**Independent Test**: Deploy once with each engine selected and verify the tunnel comes up and
agents bridge a call in both configurations.

**Acceptance Scenarios**:

1. **Given** the deployment selects the legacy engine, **When** the container starts, **Then**
   behavior is identical to feature 011 today.
2. **Given** the deployment selects the strongSwan engine, **When** the container starts,
   **Then** the tunnel is established by strongSwan and the agents operate normally.
3. **Given** no explicit engine selection, **When** the container starts, **Then** a documented
   default engine is used.

---

### Edge Cases

- Carrier's gateway rejects the first IKE proposal set and demands renegotiation with different
  algorithms (observed in the field: gateways request specific DH groups) — the engine must
  retry/renegotiate automatically.
- SIM returns an AKA synchronization failure (AUTS) during EAP-AKA — re-synchronization must be
  handled (the SIM/carrier will do this occasionally after the CS side has also been
  authenticating).
- The modem AT port is momentarily busy (held by the CS daemon or the IMS agent's own AKA) when
  the tunnel needs to authenticate — authentication must wait/retry rather than fail the tunnel
  permanently or corrupt the other user's AT session.
- ePDG DNS resolution fails or returns multiple gateways — engine must use the configured
  override or try resolved addresses.
- Carrier assigns only IPv6 (observed) or only IPv4 inner addresses — both must be handled as
  they are today.
- The tunnel drops while a bridged call is in progress — the call is lost (accepted, same as
  Option 1), but the tunnel must re-establish and the line must return to service unattended.
- Container restart while a previous run's namespace/interface still exists — startup must be
  idempotent.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST establish the ePDG tunnel using strongSwan as the IKEv2/IPsec
  engine, selectable at deploy time; the existing SWu-IKEv2 dialer MUST remain selectable as a
  fallback engine during the proving period.
- **FR-002**: EAP-AKA authentication MUST use the SIM inside the EC200U modem via its AT
  command interface (`AT+CSIM`), with runtime USIM application discovery (different operators'
  SIMs have different AIDs) — no PC/SC hardware reader and no extracted key material.
- **FR-003**: The tunnel MUST survive carrier-scheduled IKE rekeying and re-authentication
  unattended, including re-running EAP-AKA against the SIM when required.
- **FR-004**: The engine MUST detect a dead peer / broken path and re-establish the tunnel
  automatically, indefinitely (no bounded retry count that gives up permanently).
- **FR-005**: The `ims` network namespace, the tunnel interface inside it, and the veth link to
  the default namespace MUST persist across tunnel reconnects and rekeys (only the tunnel's
  addresses/keys may change).
- **FR-006**: The P-CSCF address(es) assigned by the carrier during tunnel establishment MUST
  be published to the same location the bridge agents already read
  (`/tmp/pcscf` by default), refreshed on every (re)connect.
- **FR-007**: The vowifi-ims-agent and vowifi-sip-agent MUST require no source changes; any
  adaptation happens in the tunnel/orchestration layer.
- **FR-008**: The circuit-switched GSM-to-SIP daemon's behavior MUST remain unchanged,
  including its use of the modem; concurrent SIM access between subsystems MUST NOT deadlock or
  permanently starve either side.
- **FR-009**: Both engines MUST run in the existing single container image (Alpine/musl based)
  with the existing privilege model; image size growth is acceptable but the Python runtime MUST
  be removable once the legacy engine is retired.
- **FR-010**: Tunnel state transitions (connecting, established, rekeyed, re-authenticated,
  disconnected, retrying) MUST be observable from container logs with enough detail to
  diagnose a carrier-side failure after the fact.
- **FR-011**: Startup MUST be idempotent: a restart with leftover namespace, interface, or
  IPsec state from a previous run converges to a clean working tunnel.
- **FR-012**: The keepalive mechanism that prevents carrier-side idle timeout of the tunnel
  MUST be preserved (operators drop idle tunnels; ICMP is filtered, so the existing TCP-based
  keepalive to the P-CSCF stays).

### Key Entities

- **Tunnel Engine**: The subsystem that authenticates to the carrier's gateway and maintains
  the encrypted tunnel; one of `strongswan` (new) or `swu` (legacy), selected at deploy time.
- **VoWiFi Line**: The carrier-facing service made available through the tunnel — characterized
  by carrier identity (MCC/MNC), the SIM identity (IMSI, read at runtime), assigned inner
  address(es), and assigned P-CSCF address(es).
- **SIM Authentication Channel**: The path by which EAP-AKA challenges reach the SIM — the
  modem's AT port, shared with other subsystems, exclusive per transaction.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: The tunnel remains continuously usable for ≥ 24 hours unattended, spanning at
  least one carrier-scheduled rekey, with zero bridge-agent restarts attributable to the tunnel
  (measured on at least one live carrier).
- **SC-002**: After a forced ≤ 60 s WAN interruption, the VoWiFi line returns to service
  (registered, callable) within 90 s of connectivity returning, with no manual intervention —
  matching feature 011's SC-003 recovery bound.
- **SC-003**: An inbound VoWiFi call placed ≥ 12 hours after startup is answered and bridged
  within 5 s, with two-way audio, identical to a call placed immediately after startup.
- **SC-004**: EAP-AKA succeeds against both test carriers (Vi India 404/043 for tunnel
  establishment; Airtel India 404/094 for tunnel + full call bridging) using only the
  modem-resident SIM.
- **SC-005**: Switching engines requires editing only deployment configuration (no image
  rebuild), verified by bringing the same image up once per engine.
- **SC-006**: With the legacy engine selected, observable behavior is unchanged from feature
  011 (regression-checked via the existing quickstart flow).

## Assumptions

- The two test SIMs/carriers remain available for live verification; Vi India is expected to
  keep blocking IMS registration at the SIP layer (per prior findings), so full end-to-end call
  verification happens on Airtel while Vi verifies tunnel establishment only.
- The carrier ePDGs accept a software IKEv2 client with EAP-AKA (proven by Option 1 on both
  carriers) and standard rekey/re-auth behavior (proven for strongSwan generally by the osmocom
  community's documented use).
- The container keeps its current privilege grants (network administration, namespace
  creation); no new host-level privileges are assumed beyond what feature 011 already requires.
- The 24 h soak (SC-001) and long-idle call (SC-003) are operator-run live verifications, not
  CI — consistent with how features 003–011 validated carrier-facing behavior.
- One tunnel/one SIM at a time stays the scope; multi-line support remains out of scope.
- IPv4 remains the preferred inner address family when the carrier offers both (matches current
  agent behavior of preferring the IPv4 P-CSCF).
