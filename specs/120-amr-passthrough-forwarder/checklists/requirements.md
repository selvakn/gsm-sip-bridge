# Specification Quality Checklist: AMR passthrough for AMR-capable clients

**Purpose**: Validate specification completeness and quality before proceeding to planning
**Created**: 2026-10-22
**Feature**: [spec.md](../spec.md)

## Content Quality

- [x] No implementation details (languages, frameworks, APIs). The Input line quotes the request verbatim. Background names the SIP stack only to explain why the spike ruled one design out.
- [x] Focused on user value and business needs
- [x] Written for non-technical stakeholders (AMR/RTCP/DTMF are this domain's vocabulary, as in spec 085)
- [x] All mandatory sections completed

## Requirement Completeness

- [x] No [NEEDS CLARIFICATION] markers remain (Q1 → inbound only; Q2 → off by default; see spec Clarifications)
- [x] Requirements are testable and unambiguous ("compatible" is defined concretely in FR-003)
- [x] Success criteria are measurable
- [x] Success criteria are technology-agnostic (no implementation details)
- [x] All acceptance scenarios are defined
- [x] Edge cases are identified
- [x] Scope is clearly bounded (AMR only; GSM path untouched; carrier SDP unchanged)
- [x] Dependencies and assumptions identified

## Feature Readiness

- [x] All functional requirements have clear acceptance criteria
- [x] User scenarios cover primary flows
- [x] Feature meets measurable outcomes defined in Success Criteria
- [x] No implementation details leak into specification (FR-017 reworded from "sockets, worker threads")

## Notes

- Planning must resolve the main design risk, which this spec states only as a requirement (FR-004, FR-005). For an inbound call, the telephone-side agent commits the internal link's media handling when the call arrives, before the client has answered. Planning needs a mechanism that still honours the client's actual answer, such as renegotiating the internal link before media starts. That mechanism is unproven, so plan for a short spike.
- Items marked incomplete require spec updates before `/speckit-clarify` or `/speckit-plan`.
