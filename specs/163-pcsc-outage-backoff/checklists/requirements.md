# Specification Quality Checklist: The Supervisor Stops Retrying in a Tight Loop While a Card-Reader SIM Is Out of Reach

**Purpose**: Validate specification completeness and quality before proceeding to planning
**Created**: 2026-11-24
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

- **Domain terms.** The audience is the maintainer of a telecom bridge, so domain terms (ePDG tunnel, IKE SA, EAP-AKA, `AT+CFUN`, P-CSCF, card reader) are the user's vocabulary, not implementation details. The Background names existing supervisor behaviour (the modem SIM recovery, the underlay guard) only to state what is unchanged and what the feature mirrors. The requirements state behaviour, not structure.
- **FR-001's "shared marker pinned by a test"** is a behavioural constraint: the classification must not depend on error wording. It follows the learnings class "string classifiers" (docs/greptile-review-learnings.md) and #233's typed error.
- **FR-011 deliberately leaves one design choice to planning**: a new underlay hold cause or a separate card gate. It gives the decision criteria instead of a `[NEEDS CLARIFICATION]`, as the feature request asked.
- **Defaults chosen without asking:**
  - a 2-minute cap, matching the agent's renewal ladder;
  - checks every 5 s, matching the underlay guard's held polling;
  - logs only, with no new alert or metric, matching #233's Clarification Q2.

  Each is listed under Assumptions. The third was revisited by `/speckit-clarify` on 2026-11-24: the existing tunnel-failure alert names the card as the cause (FR-014, option B), still with no new category or metric.
- **Arithmetic checked:**
  - SC-001: about 8 agent restarts in 10 minutes at 5 s doubling to 120 s; the limit is 15.
  - SC-002: about 5 tunnel attempts in 10 minutes from about 50 s doubling to 120 s; the limit is 6.
- **Learnings classes that apply** (Part 1):
  - "Recovery loops that detect but never fix, or never escalate": this feature does the opposite, by slowing futile recovery without ever stopping it (FR-007's floor of one attempt per cap).
  - "Latching state": the outage clears on registration or a reachable check (Key Entities).
  - "Concurrency & shared devices": card checks must leave the card (#232), and lines are matched by IMSI (FR-002, FR-012).
