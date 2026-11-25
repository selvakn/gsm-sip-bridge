# Specification Quality Checklist: A Slow SIM Answer No Longer Costs a Modem Line Its Registration, or Corrupts the Next Session on the Port

**Purpose**: Validate specification completeness and quality before proceeding to planning
**Created**: 2026-11-25
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

- Following this repo's spec convention (see specs/163), the Background names the code paths and observed log text, so the defect can be traced. The requirements themselves state behaviour: they name AT/APDU concepts (`AT+CSIM`, AUTHENTICATE, GET RESPONSE) only because those are the protocol the requirement is about, and they leave the mechanism to planning (which sync exchange to use, how the budget is threaded through).
- FR-008 names existing classifier wording (`AT+CSIM failed`, timeout text) on purpose: preserving it is the requirement.
- FR-007 (the AKA budget for AUTHENTICATE relayed by `vowifi-usim-bridge`) goes beyond the issue's part B. It is the same defect on the ePDG tunnel's EAP-AKA ("fix the class"), and the maintainer confirmed it in the 2026-11-25 clarification.
- Clarified 2026-11-25: the sync also replaces the drain after a timeout inside a session (FR-001a), and the sync is a read-only exchange that relies on reply order (FR-002), with its uncovered case recorded in Assumptions.
- Values chosen without a clarification round, recorded in Assumptions: AKA budget 20 s; sync bound = the port's own timeout; open overhead ≤ 100 ms (SC-003).
- Planning must fill in the Blast Radius section with real grep output: every `AtCommander::open` / `open_with_timeout` caller (about 25 sites), and every classifier that reads AT error text (`sim_recovery::has_csim_failure`, `session::map_registration_error`, `modules/worker.rs`, `bin/gsm_echo.rs`).
