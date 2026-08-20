# Specification Quality Checklist: Complete release of per-line kernel resources on stop

**Purpose**: Validate specification completeness and quality before proceeding to planning
**Created**: 2026-08-20
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

- All items pass. The specific things watched for while writing, since the source
  material for this feature is a diagnosis full of mechanism:
  - The investigation that produced this spec named exact commands
    (`swanctl --terminate`, an encryption-state flush, deleting `tun23-N` and
    `veth-sipN`) and the compose key `stop_grace_period`. FR-003 through FR-005 and
    FR-010 state the behaviour each of those achieves instead; the commands belong in
    the plan.
  - SC-002 could easily have been written as "grep for `RTNETLINK answers: File
    exists`". It is stated as the observable condition — no line reports its tunnel
    identifier as already claimed — so it survives a change of wording in the log.
  - Key Entities uses tunnel identifier / namespace / virtual cable pair rather than
    `if_id` / `netns` / `veth`, and spells out the claimed-against-the-container
    property, because that property is the whole reason the feature exists and a
    reader who misses it cannot follow the rest.
- Re-validated 2026-08-20 after a clarification session (4 questions, all answered). It
  added FR-018 (one teardown across both bearers), FR-019 (whole-teardown budget with a
  fallback to the release steps), FR-020 (log-only escalation), SC-000 (the baseline every
  restart criterion is now stated against) and SC-010. Two of those — FR-018 and FR-019 —
  changed the plan's blast radius and complexity justifications, and the downstream
  artifacts were updated in the same pass rather than left to drift.
- Deliberately **not** marked `[NEEDS CLARIFICATION]` at authoring time, decided as
  assumptions instead and recorded in the Assumptions section:
  - Exposing the per-line namespaces at host scope (needed for FR-013) — accepted,
    with FR-016 guarding the multi-instance case.
  - Delete-and-recreate rather than adopt a leftover namespace at start — the smaller
    and safer of the two options that both remove the wait.
- SC-000 through SC-007 and SC-010 are **live-measurement** criteria. They cannot be discharged by
  the test suite; the plan must schedule them against the real hardware, using the
  2026-07-31 numbers in `docs/operations.md` as the before-baseline.
- One open risk carried into planning, not a spec gap: the prior investigation
  concluded no remedy exists. If re-measurement confirms that conclusion even after a
  correct teardown, FR-001 through FR-012 remain worth having for their own sake
  (correctness and diagnosability), but SC-001 would be unmet and the feature's premise
  would need revisiting rather than the requirements being quietly relaxed.
