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
- Initially not achieved: the pcsc line's IKE_SA_INIT drew no response, and
  this was first recorded here as a suspected carrier-side factor. **That
  conclusion was wrong — see the follow-up below.**

## T026 follow-up (2026-07-28): tunnel confirmed UP, earlier diagnosis retracted

Re-ran the pcsc line in isolation with a host-side packet capture. The
Vodafone ePDG tunnel establishes fully:

```
[IKE] EAP method EAP_AKA succeeded, MSK established
[IKE] IKE_SA ims[2] established between 192.168.15.10
      [0404438083996440@nai.epc.mnc043.mcc404.3gppnetwork.org]...203.88.4.88
[IKE] CHILD_SA ims{2} established ... TS 2402:8100:6972:e043:0:18:291c:4201/128
[IKE] installing new virtual IP 2402:8100:6972:e043:0:18:291c:4201
```

`tcpdump` confirms the full IKEv2 exchange on the wire (IKE_SA_INIT →
IKE_AUTH ×3 over NAT-T/4500 → ESP + NAT keepalives). A standalone IKEv2
prober also showed both `epdg.epc.mnc043.mcc404` addresses (203.88.4.88 and
203.88.11.33) answering IKE_SA_INIT immediately. So the ePDG was never
silent and the carrier/entitlement theory is retracted.

The real cause of the original T026 silence was **test-harness contention**:
that run started a second bridge container with `--network host` while the
production container was already running, so both instances shared the host's
UDP 500/4500, vpcd port, metrics port and SIP ports. Symptoms traced to this,
not to the feature:

- `pjsua_transport_create returned 120098` (PJ error base + `EADDRINUSE`) on
  the IMS/SIP agents.
- IKE responses landing in the wrong charon instance.

Two genuine defects *were* found and fixed while chasing this:

- **vpcd was provisioned unconditionally under the strongswan engine**, so an
  all-card-reader deployment aborted at startup with "pcscd's vpcd reader
  never came up" on a virtual reader no line would ever use, and eap-sim-pcsc
  logged repeated `SCardConnect: No smart card inserted` walking empty vpcd
  slots before reaching the real reader. Now gated on
  `needs_vpcd_reader()` — provisioned only when a modem-backed line exists.
- Documentation gap: a card-reader-only deployment was never exercised
  before this run; only mixed modem+pcsc was.

Still not verified end-to-end: IMS registration and a test call on the pcsc
line (quickstart steps 5-8). These need the line to run in the *production*
container rather than a second one, to avoid the port contention above.
