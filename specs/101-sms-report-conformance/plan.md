# Implementation Plan: SMS Delivery Report Conformance

**Branch**: `100-vowifi-underlay-guard` (by request) | **Date**: 2026-10-20 | **Spec**: [spec.md](spec.md)
**Input**: `specs/101-sms-report-conformance/spec.md`

## Summary

The SMS delivery report (RP-ACK `MESSAGE`) the bridge sends after an inbound IMS SMS lacks two things TS 24.341 §5.3.2.4 / Annex B.6 call for: an `In-Reply-To` naming the inbound Call-ID, and a P-CSCF first-hop `Route` ahead of the registrar's `Service-Route` entries. Add both, put the `Route` behind one default-on setting (clarification 2026-10-20), and log a warning when the network rejects a report. Nothing else on the wire changes.

Approach (details in [research.md](research.md)): thread the inbound Call-ID and the setting down to the existing report sender; extend the existing `MESSAGE` builder with an optional `In-Reply-To` and a caller-supplied route list that now starts with the P-CSCF; remember the Call-IDs of reports in flight so the existing response handler can warn on a final non-2xx answer.

## Technical Context

**Language/Version**: Rust (workspace crate `gsm-sip-bridge`)
**Primary Dependencies**: none new
**Storage**: N/A (in-memory set of in-flight report Call-IDs, bounded)
**Testing**: `cargo test` via `make test`; unit tests beside the builder and the agent dispatch tests; live checks per [quickstart.md](quickstart.md)
**Target Platform**: Linux host (VoLTE bridge) and the `vowifi-ims-agent` process (VoWiFi); arm64 Pi build for the Jio line
**Project Type**: single Cargo workspace
**Performance Goals**: none; one extra header pair per SMS
**Constraints**: a byte-level diff of the report must show only the two additions (SC-003); the Jio/Vodafone paths must not regress (SC-004)
**Scale/Scope**: a handful of source files plus docs; no new modules

## Constitution Check

| Principle | Status |
|---|---|
| I. Integration-first testing | Pass. Builder and dispatch tests use the real builder and real parsers, no mocks. The live carrier checks are the integration layer. |
| II. Green-on-commit | Pass. Tasks end each phase with `make format && make lint && make test`. |
| III. Atomic commits | Pass. One commit per phase (see tasks.md). |
| IV. Makefile-driven | Pass. No new commands. |
| V. Simplicity | Pass. One bool setting, one optional header field, one bounded set. No new abstraction. |

Re-checked after design: no violations; Complexity Tracking not needed.

## Project Structure

### Documentation

```text
specs/101-sms-report-conformance/
├── spec.md
├── plan.md
├── research.md
├── data-model.md
├── quickstart.md
├── contracts/
│   ├── delivery-report-message.md
│   └── config.md
├── checklists/requirements.md
└── tasks.md
```

### Source Code (existing files touched)

```text
gsm-sip-bridge/src/
├── ims/sip_client.rs      # MessageRequest + build_message: optional In-Reply-To; builder tests
├── ims/session.rs         # send_sms_delivery_report: take inbound Call-ID + route flag, build P-CSCF route, return report Call-ID
├── ims/agent/mod.rs       # acknowledge() and the two RP-ACK/RP-ERROR call sites pass the Call-ID; in-flight set; final-response warning
├── config/mod.rs          # VowifiConfig / VolteConfig: sms_report_pcscf_route
├── config/raw.rs          # raw TOML mirror, VoLTE inherits VoWiFi's value
└── volte/carrier_agent.rs # plumb the VoLTE value into the agent params
docs/configuration.md      # document the setting
RELEASE_NOTES.md           # user-visible note
```

**Structure Decision**: no new files in source; the change fits the existing report-sending path.
