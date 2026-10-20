# Specification Quality Checklist: Hold VoWiFi recovery while the path to the ePDG is down

**Purpose**: Validate specification completeness and quality before proceeding to planning
**Created**: 2026-10-20
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

- Protocol and component vocabulary (IKE_SA, CHILD_SA, ePDG, P-CSCF, charon, DPD, the named recovery reasons) is used as in specs 086–098. It is the domain language of this project's operators, not implementation detail. No code structure, module names or languages appear.
- Fixed values were chosen as informed defaults and are recorded in the requirements, not left open:
  - 15-minute maximum hold (FR-012);
  - 2 × 10s no-route debounce (FR-002);
  - 30s liveness wait (FR-004);
  - 200s of IKE inactivity before a half-open IKE_SA counts as stuck (FR-015).
- One setting added in clarification: `[vowifi].underlay_guard`, default on (FR-025).
- Two items deferred to planning on purpose:
  - the shape of the on-demand liveness trigger in the strongSwan fork (FR-004a; clarification settled that it is a fork change);
  - the exact evidence parser for "no response" (FR-003).
- Explicitly out of scope:
  - charon retransmit tuning;
  - MOBIKE;
  - requesting the carrier liveness timeout;
  - ePDG re-selection;
  - swu;
  - VoLTE;
  - removing the P-CSCF probe (#120).
