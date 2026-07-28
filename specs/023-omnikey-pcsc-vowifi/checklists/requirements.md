# Specification Quality Checklist: PC/SC Card-Reader-Backed VoWiFi Lines

**Purpose**: Validate specification completeness and quality before proceeding to planning
**Created**: 2026-07-27
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

- All items pass on first draft. Domain-specific nouns (SIM, VoWiFi line,
  card reader, IMSI, tunnel connection method) are retained as they are this
  product's business vocabulary, not implementation choices — no
  language/framework/library/file names appear anywhere in spec.md.
- No [NEEDS CLARIFICATION] markers were needed: the driving conversation
  (captured in the approved implementation plan) already resolved the two
  decisions that would otherwise have required clarification — SIM PIN
  status (disabled) and deployment scope (mixed modem + card-reader lines).
- Ready for `/speckit-plan`.

## Post-implementation follow-ups (T027)

Implementation (and live hardware spikes against a real OmniKey AG 3x21)
surfaced no spec gaps requiring a change to spec.md itself, but corrected two
inaccuracies that had crept into design artifacts written before the spikes:

- `quickstart.md`/`docs/omnikey-pcsc-vowifi.md` originally referenced a
  `curl http://localhost:5076/status` JSON endpoint with a `.lines` field —
  this does not exist in this codebase (the real observability surface is
  the `vowifi-status` CLI subcommand plus the `/metrics` Prometheus endpoint,
  both confirmed to key on `card_id`/`module` identically for a pcsc line,
  satisfying FR-010 with no code change). Fixed in both docs.
- `docker/Dockerfile`'s runtime image has no `pcsc-tools` package on Alpine
  (no `pcsc_scan`) — `quickstart.md` fixed to use `opensc-tool
  --list-readers` instead.
- A genuinely new, non-obvious finding worth keeping for future readers:
  hand-decoding `EF_IMSI`'s BCD payload (e.g. without `pySim-read.py`) has a
  parity/oddness nibble at the start that is *not* part of the IMSI — easy
  to get wrong (confirmed by getting it wrong once during the live spike).
  Documented in `docs/omnikey-pcsc-vowifi.md`.

## T026 live run result (partial — see tasks.md)

Ran the real built image against a live mixed deployment: a real modem line
(Airtel, mnc=094) and a real `pcsc_reader` line (Vodafone, mnc=043 via the
OmniKey) simultaneously. Confirmed live and unambiguously:

- `discover` resolved both lines correctly (`LINE_COUNT=2`), pcsc line
  appended at index 1 with card_id `pcsc0`, no modem checks run for it.
- `eap-sim-pcsc`'s reader/card discrimination is provably correct in
  production code, not just inferred: when the Airtel line's IKE_AUTH asked
  for quintuplets matching its own IMSI, strongSwan logged `"tried 1 SIM
  cards, but none has quintuplets for ...mnc094.mcc404..."` and correctly
  sent `AKA_AUTHENTICATION_REJECT` — it found the live OmniKey/Vodafone card
  but correctly refused to misuse it for the wrong line's identity.
- The pcsc line's own ePDG FQDN correctly derived from its `mcc`/`mnc`
  (`epdg.epc.mnc043.mcc404.pub.3gppnetwork.org` → Vodafone's real ePDG,
  `203.88.4.88`) and IKE_SA_INIT was sent there.
- Not achieved: a completed registration for the pcsc line — Vodafone's
  ePDG never responded to repeated IKE_SA_INIT retransmits (silence, not a
  rejection), most likely a carrier-side network-path/entitlement factor
  external to this feature's code (every other stage of the pipeline this
  feature owns was confirmed correct). Needs the operator's own real
  deployment network path to resolve — out of scope for a sandboxed test.
