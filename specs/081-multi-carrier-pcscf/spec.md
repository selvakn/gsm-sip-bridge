# Feature Specification: Multi-Carrier VoLTE P-CSCF Priming

**Feature Branch**: `081-multi-carrier-pcscf`
**Created**: 2026-09-23
**Status**: Draft
**Input**: User description: "a solution to address this gap and make multiple carriers setups to work" (closing the gap identified after specs/080-volte-pcscf-auto-prime: every VoLTE line reads P-CSCF from one shared cache location today, so a deployment with different-carrier SIMs on different modems silently applies one carrier's address to every line)

## Clarifications

### Session 2026-09-23

- Q: Should automatic capture run for every discovered line that lacks a
  usable address (full zero-touch multi-carrier priming), or only fix where
  each line's address is read from? → A: Full zero-touch — the bridge's
  orchestrated startup automatically runs the capture procedure for every
  discovered line that lacks a usable address, not just line 0.
- Q: When priming succeeds for some lines and fails for others in the same
  startup, should successful lines register immediately or should VoLTE hold
  back every line until all are primed? → A: Per-line independence —
  successful lines proceed to registration immediately; failed lines retry
  on their own without blocking anyone else.
- Q: Should the system detect a physical SIM swap (cached address no longer
  matches the carrier now in that line's modem) and auto-re-prime? → A: Out
  of scope, same as specs/080-volte-pcscf-auto-prime's existing single-line
  limitation — a stale cache after a SIM swap remains an operator
  responsibility to clear, now per line instead of globally.
- Q: Should per-line capture run sequentially (one line fully primed before
  the next starts) or concurrently across lines? → A: Concurrent — every
  line that needs priming starts its capture at the same time, independent
  of the others, consistent with the per-line independence already required
  elsewhere in this spec (FR-005, FR-009) and the existing per-line network-
  namespace isolation (specs/020-volte-line-netns).
- Q: Should the per-line cache be identified by a stable per-modem/card
  identity or by discovery-order position (index 0, 1, 2, ...)? → A: Stable
  per-modem/card identity — a captured address stays bound to the specific
  line/modem it was captured for, and survives a modem being added,
  removed, or re-enumerated in a different order across restarts.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Mixed-carrier fleet primes itself (Priority: P1)

An operator runs several modems in one deployment, each carrying a SIM from a
different carrier (for example, one Jio line and one Vodafone line). They
enable VoLTE across all lines with no per-carrier P-CSCF address configured.
Today, only the first discovered line gets a correct, automatically captured
address; every other line silently reuses that same address even though it
belongs to a different carrier's network, so those other lines fail to
register. With this feature, each line captures and uses the address that
belongs to its own carrier.

**Why this priority**: This is the exact gap this feature exists to close.
Without it, automatic priming (specs/080-volte-pcscf-auto-prime) only ever
produces a working deployment when every line happens to share one carrier —
a mixed-carrier fleet cannot reach a fully-registered state without manual,
per-line intervention today.

**Independent Test**: Start the bridge with `[volte].enabled = true`, two or
more auto-discovered lines on different carriers, no explicit per-line
overrides, and no pre-existing captures. Observe that every line ends up
registered using an address that actually belongs to its own carrier's
network, not another line's.

**Acceptance Scenarios**:

1. **Given** VoLTE is enabled with multiple auto-discovered lines on
   different carriers and no per-line override or cache exists for any of
   them, **When** the bridge starts, **Then** each line captures its own
   address before attempting registration, and the operator can see this
   happening per line (not a single, ambiguous "priming" event).
2. **Given** each line's capture succeeds, **When** that line's VoLTE
   registration then runs, **Then** it uses the address captured for that
   specific line, never an address captured for a different line.
3. **Given** priming for one line is in progress, **When** other lines are
   already registered or are running unrelated bridge functions (e.g. the
   circuit-switched GSM-to-SIP path on any line), **Then** those are not
   disrupted by another line's capture activity.

---

### User Story 2 - Cache lost after a redeploy, mixed carriers (Priority: P2)

An operator recreates the container for a mixed-carrier deployment (new
image, `--force-recreate`, or any redeploy that wipes the previous
container's filesystem). Every line's previously captured address is gone.
With this feature, the next startup notices each line's cache is gone and
re-captures it independently, the same way single-line redeploys already
recover today.

**Why this priority**: This is the multi-line extension of the single most
common operational failure called out in docs/operations.md and already
solved for the single-carrier case by specs/080-volte-pcscf-auto-prime.
Without this story, a mixed-carrier fleet's recovery still requires an
operator to know which line is which carrier and manually redo the priming
dance for each one.

**Independent Test**: Prime a multi-line, mixed-carrier deployment
successfully, delete every line's capture, restart the bridge, and confirm
every line re-captures its own address and re-registers without the operator
doing anything per-line.

**Acceptance Scenarios**:

1. **Given** every line's previously-captured address is missing at startup,
   **When** the bridge starts with VoLTE enabled and no overrides, **Then**
   each line independently re-captures its own address, the same as a
   first-time setup.
2. **Given** only some lines' caches are missing or invalid while others
   still hold a valid captured address, **When** the bridge starts, **Then**
   only the lines that actually need it are re-primed — lines with a still-
   valid address see no new capture activity.

---

### User Story 3 - Some lines already pinned, others not (Priority: P3)

An operator has already pinned a permanent address for one or more lines
(the existing "make it permanent" option), while other lines in the same
deployment have never been primed. With this feature, pinned lines continue
to work exactly as they do today — no new activity, no risk of a pinned
line's address being touched by another line's capture — while only the
unpinned lines go through automatic per-line priming.

**Why this priority**: A "do no harm" requirement, same rationale as User
Story 3 in specs/080-volte-pcscf-auto-prime, extended to the case where
pinned and unpinned lines coexist in one deployment.

**Independent Test**: Configure an explicit per-line override (or a valid
pre-existing per-line capture) for one line while leaving a second line
unconfigured, start the bridge, and confirm the first line shows no capture
activity while the second line is primed automatically.

**Acceptance Scenarios**:

1. **Given** a line has an explicit override configured, **When** the bridge
   starts, **Then** that line skips capture entirely and uses the override,
   regardless of what other lines in the same deployment are doing.
2. **Given** a line has no override but its own cache already contains a
   valid address, **When** the bridge starts, **Then** that line skips
   capture entirely and uses its own cached address.

### Edge Cases

- What happens when priming succeeds for some lines but fails for others in
  the same startup (e.g. one SIM has no signal, or that carrier's tunnel
  negotiation fails)? The operator must be able to tell, from output alone,
  exactly which line/carrier failed versus which succeeded.
- What happens when two lines happen to carry SIMs from the *same* carrier?
  Both still end up with a correct, usable address for their shared carrier
  — each line captures independently (per the Clarifications above), so
  this case is handled the same way as any other multi-line deployment,
  just with two lines' addresses happening to match.
- What happens when the number of discovered lines changes across restarts
  (a modem is added, removed, or re-ordered by the discovery process)? A
  line's captured address must stay bound to that specific line/modem, not
  silently apply to whatever now occupies the same position in the list —
  see Clarifications and FR-003 (cache keyed by stable per-modem/card
  identity, not discovery-order position).
- What happens if a line's SIM is physically swapped for a different
  carrier's SIM without clearing that line's cache? Out of scope (see
  Clarifications and FR-008) — mirrors the same accepted limitation from
  specs/080-volte-pcscf-auto-prime.
- What happens when automatic priming is not applicable (standalone CLI
  diagnostic invocations)? Unchanged from specs/080-volte-pcscf-auto-prime —
  those keep today's explicit, per-invocation behavior.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST determine, per line, whether a usable P-CSCF
  address is already available for that specific line (an explicit override
  scoped to that line, or a cache holding a valid address scoped to that
  line) before attempting any capture activity for it.
- **FR-002**: When the bridge's own orchestrated startup sequence brings up
  VoLTE, the system MUST automatically run the capture procedure for *every*
  discovered line that lacks a usable address — not only the first
  discovered line — so a mixed-carrier fleet reaches a fully registered
  state with no manual per-line priming step. Lines needing capture MUST be
  primed concurrently, each independent of the others, rather than one at a
  time.
- **FR-003**: The system MUST persist each line's captured address to a
  location scoped to that specific line's stable per-modem/card identity
  (not its discovery-order position), so no two lines ever read or
  overwrite each other's captured address, and a line's address stays
  correctly bound to it even if a modem is added, removed, or
  re-enumerated in a different order across restarts.
- **FR-004**: A line's VoLTE registration MUST use only that line's own
  resolved address (override, own cache, or own fresh capture) and MUST
  NEVER fall back to a different line's address.
- **FR-005**: The system MUST let each line proceed to VoLTE registration as
  soon as that specific line's address is resolved, independent of the
  state of any other line — a line whose priming fails MUST NOT hold back or
  delay a line whose priming already succeeded.
- **FR-006**: The system MUST make per-line priming activity and its outcome
  (success or failure) visible in the bridge's normal operator-facing
  output, identifying which specific line/carrier each event belongs to.
- **FR-007**: The system MUST NOT perform capture activity for a line that
  already has a usable address (override or valid own-line cache), so
  already-primed or explicitly-pinned lines see no new startup activity
  regardless of what other lines in the same deployment require.
- **FR-008**: Detecting that a line's cached address no longer matches the
  SIM currently in that line's modem (e.g. after a physical SIM swap) is
  OUT OF SCOPE. The system is not required to notice this mismatch; a
  cached address is trusted until it goes missing or becomes structurally
  invalid, the same limitation specs/080-volte-pcscf-auto-prime already
  accepts for the single-line case, now carried forward per line.
- **FR-009**: A line's failed priming attempt MUST retry on the same cadence
  already used elsewhere for VoLTE/VoWiFi startup failures, and that retry
  MUST NOT block or delay any other line's priming, registration, or
  unrelated bridge functions.
- **FR-010**: The system MUST leave the existing rule that VoWiFi and VoLTE
  cannot both be persistently enabled unchanged; per-line priming remains an
  internal, transient action on each line and is never itself reported as
  "VoWiFi is enabled."

### Key Entities

- **Per-line P-CSCF cache**: The on-disk record of a captured P-CSCF address
  scoped to one specific line, keyed by that line's stable per-modem/card
  identity rather than its discovery-order position. Replaces today's
  single cache location shared by every line.
- **Line-to-carrier binding**: The association between a discovered line and
  the P-CSCF address (pinned or captured) that belongs to that line's own
  carrier network.
- **Capture attempt (per line)**: A single automatic priming run scoped to
  one line, producing either a usable address for that line or a reported,
  line-attributed failure.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An operator running a deployment with several different-carrier
  SIMs can take every line from "VoLTE enabled, nothing configured" to a
  successfully registered state without performing any manual, per-carrier
  priming step.
- **SC-002**: After a redeploy wipes every line's captured address in a
  mixed-carrier deployment, all lines recover to a registered state on their
  own, without an operator having to identify which line belongs to which
  carrier.
- **SC-003**: A deployment where some lines are already pinned or cached
  shows no observable change in startup behavior or startup time for those
  specific lines, regardless of what happens on other lines in the same
  deployment.
- **SC-004**: When priming fails for one line in a multi-line deployment, an
  operator can identify which specific line/carrier failed directly from the
  bridge's own output, within the same startup attempt, while every
  unaffected line continues operating normally.
- **SC-005**: Across any multi-carrier deployment, zero lines ever end up
  registered using an address that belongs to a different line's carrier.

## Assumptions

- Each line already carries a stable per-modem/card identity across restarts
  (the existing per-line network-namespace isolation from
  specs/020-volte-line-netns already assigns one) — this feature reuses that
  existing identity as the per-line cache key (FR-003) rather than
  introducing new logic to detect or track a modem's position changing in
  the discovery order.
- The underlying capture mechanism (a transient VoWiFi/ePDG tunnel reading
  the IKEv2 config payload) is unchanged and continues to support only the
  default tunnel engine, exactly as specs/080-volte-pcscf-auto-prime already
  assumes — this feature changes *which lines* it runs for and *where*
  results are stored, not *how* capture itself works.
- A captured address remains valid for the life of that specific line's
  SIM/carrier pairing, the same assumption specs/080-volte-pcscf-auto-prime
  already makes — now scoped per line instead of globally.
- Operators who have already pinned explicit per-line overrides today are
  unaffected by this feature; their configuration continues to work exactly
  as before.
