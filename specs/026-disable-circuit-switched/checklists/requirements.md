# Specification Quality Checklist: Disable Circuit-Switched Handling

**Purpose**: Validate specification completeness and quality before proceeding to planning
**Created**: 2026-08-04
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

All three open questions were resolved with the operator before the spec was written, so no
`[NEEDS CLARIFICATION]` markers were ever introduced:

1. **Scope of the disable** — the daemon process keeps running and keeps hosting the shared
   services (registrar, outbound-dialing channel, metrics, message store); only modem discovery,
   probing, and circuit-switched call handling stop. Recorded as FR-005 through FR-017.
2. **Activation** — an explicit flag defaulting to enabled, *not* auto-derived from the VoWiFi or
   VoLTE flags. Backward compatibility on upgrade was the deciding factor. Recorded as FR-002,
   FR-003, User Story 2, and an explicit Out of Scope entry.
3. **Message forwarding** — follows the circuit-switched path; a startup warning covers the
   orphaned case rather than a hard error. Recorded as FR-016, FR-024, and an Assumption.

### Clarification session 2026-08-04

A `/speckit-clarify` pass asked four further questions and corrected one factual error.

**Correction applied before questioning**: the original draft claimed the circuit-switched host
process hosts the telephone-registration service and the outbound-dialing channel that VoWiFi and
VoLTE use. It does not. When VoWiFi is enabled, that subsystem owns the telephone-facing side and
the circuit-switched host's own SIP side is already inert; the VoWiFi subsystem hosts the registrar
itself, and its outbound dialing never passes through the circuit-switched host. FR-011 through
FR-017 were restated as system-level guarantees, and a "Telephone-facing side" entity was added to
record the ownership rule.

Answers integrated:

4. **Flag location** — a new `[cs]` section holding a single `enabled` key. Recorded as FR-001,
   FR-002a (validation must accept the new section), and FR-004a (`[modules]` keeps its name and
   is cross-referenced from the docs).
5. **Modem assignment** — with the flag off, every probed modem is offered to VoWiFi/VoLTE,
   including voice-capable modems otherwise reserved for the circuit-switched path. Recorded as
   FR-010a/b/c, User Story 4, and three edge cases. This is the one deliberate exception to
   "no change to VoWiFi/VoLTE", now stated explicitly in Out of Scope.
6. **Telephone-facing side** — with the flag off, the circuit-switched host establishes no trunk
   registration and starts no registrar of its own, reusing the existing ownership suppression.
   Recorded as FR-009a/b/c. Consequence: a registrar-only deployment is no longer supported, so
   FR-023 and the corresponding assumption were narrowed to metrics-and-history-only.
7. **Metrics** — circuit-switched series are omitted entirely rather than zeroed, plus one status
   indicator so "disabled" is distinguishable from "process down". Recorded as FR-021a/b/c,
   SC-005a, FR-029, and two new acceptance scenarios.

No open questions remain. Nothing was deferred to planning.
