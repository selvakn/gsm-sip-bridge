# Specification Quality Checklist: RTCP reporting on the media legs

**Purpose**: Validate specification completeness and quality before proceeding to planning
**Created**: 2026-08-27
**Feature**: [spec.md](../spec.md)

## Content Quality

- [x] No implementation details (languages, frameworks, APIs)
- [x] Focused on user value and business needs
- [x] Written for non-technical stakeholders
- [x] All mandatory sections completed

## Requirement Completeness

- [x] No [NEEDS CLARIFICATION] markers remain
- [x] Requirements are testable and unambiguous
- [x] Success criteria are measurable
- [x] Success criteria are technology-agnostic (no implementation details)
- [x] All acceptance scenarios are defined
- [x] Edge cases are identified
- [x] Scope is clearly bounded
- [x] Dependencies and assumptions identified

## Feature Readiness

- [x] All functional requirements have clear acceptance criteria
- [x] User scenarios cover primary flows
- [x] Feature meets measurable outcomes defined in Success Criteria
- [x] No implementation details leak into specification

## Notes

- **Iteration 1 (2026-08-27)**: one open marker — FR-023, which media legs
  the feature applies to. Everything else passes.
- **Iteration 2 (2026-08-27)**: marker resolved by explicit user decision —
  **carrier-facing leg of answered calls only** (FR-023), with the
  internal veth leg's own unbacked RTCP bandwidth declaration deliberately
  left as-is and recorded as a known residue (FR-023a) rather than
  silently implying RTP-01 is closed everywhere. The originated-call path
  and the `ims-call` diagnostic tool are explicitly cut, and *Out of
  Scope* now says so with the reason. Separately confirmed in the same
  decision: User Story 5 (the RTCP BYE at teardown) **stays in scope** at
  P3, severable — it is the only part that needs teardown to become
  synchronous, and FR-019/FR-020/SC-007 bound it so a failure or a slow
  socket can never delay a hangup. All items pass.
- The "What the codebase has today, and does not" table in the spec's
  *Why this exists* section names source files and symbols. This is
  deliberate and does not count as implementation leakage: it is
  established fact about the starting state (the whole reason this finding
  was deferred out of batch 5), not a prescription of how to build the
  feature. The requirements themselves name no mechanism.
- FR-014/015/016 deliberately state the port *guarantee* (declare only
  what you use; honour an explicitly named port; fall back rather than
  fail) without pinning the mechanism — RTP+1 versus an explicitly
  declared port is a plan-phase decision, and the offer shape decides it
  per call.
- **Clarify session (2026-08-27)**: five questions asked and integrated —
  reported source identity (FR-002/002a/002b), reporting cadence
  (FR-004/004a/004b), where figures surface (FR-008/008a/008b/008c),
  SDP on RTCP setup failure (FR-017/017a/017b), and inbound RTCP trust
  (FR-010a/010b). The fourth resolved a genuine contradiction between the
  first draft's FR-017 and FR-022, which could not both have been
  satisfied. Note that the third answer (metrics as well as logs) widened
  scope beyond the original recommendation and pulls
  `tests/test_metric_renames.rs` into the feature's dependencies.
- Items marked incomplete require spec updates before `/speckit-clarify`
  or `/speckit-plan`.
