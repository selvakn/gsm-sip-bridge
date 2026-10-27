# Implementation Plan: IMS Registration Refresh Without Call Interruption

**Branch**: `123-ims-registration-refresh` | **Date**: 2026-10-27 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `/specs/123-ims-registration-refresh/spec.md`

## Summary

Replace the periodic *initial-registration-style* renewal in the IMS agent with a standards-aligned in-place refresh. Delivered as three independently shippable slices:

- **Slice A (US1)** — refresh timing per TS 24.229 §5.1.1.4.1 (600 s lead for grants over 1200 s, half the grant otherwise, 60 s floor).
- **Slice B (US2, US3, US4, plus FR-018a fallback and FR-027 status)** — a protected REGISTER on the live Gm connection with the cached AKA authorization, as an asynchronous transaction owned by the dispatch loop. `Maintenance::Renewal` stops being deferred by calls; a new `RegistrationRecovery` takes over the deferred, disruptive path. Failures feed the existing P-CSCF pool logic. Registration replies and timers keep running while a PBX reply is held.
- **Slice C (US5)** — network-requested fresh AKA in a background worker with staged SAs, ports kept stable, drain period and rollback. Starts only after Slice B has live evidence.

The reporter's PR #168 (commit `3e914c3`) is a reference. Sound parts are re-created or cherry-picked with credit; the defective parts (failure handling, ring-wait ordering, UDP reply reading) are rewritten. See [research.md](research.md) for the three standards questions the spec left open; all three are now answered.

## Technical Context

**Language/Version**: Rust (workspace crate `gsm-sip-bridge`; edition as in `Cargo.toml`)
**Primary Dependencies**: existing only: `socket2`, `rand`, `tracing`, the in-tree SIP client (`ims/sip_client.rs`), Gm IPsec/XFRM backend (`ims/gm_ipsec.rs`). No new crates.
**Storage**: N/A. Registration state is in-memory in the agent; the status snapshot is already published to the status CLIs.
**Testing**: `cargo test` through `make test`; agent-level tests in `ims/agent/*_tests.rs` and `tests/test_*.rs`; a simulated P-CSCF over loopback sockets (UDP and TCP) plus the privileged Docker container for anything needing `CAP_NET_ADMIN` (XFRM). Live: Jio VoWiFi on the Pi and the local Vodafone VoWiFi rig.
**Target Platform**: Linux container (amd64 and arm64).
**Project Type**: single Rust workspace (library + binaries); the IMS agent is a long-running process.
**Performance Goals**: refresh sent within 2 s of due time even during a call (SC-008); ordinary refresh round trip well under the 128 s transaction bound.
**Constraints**: must not alter active call dialogs, media or the reg-event subscription; no blocking SIM/AT/SA work on the dispatch thread (Slice C); synthetic identifiers only in code, tests, docs.
**Scale/Scope**: one registration per line, up to N lines (VoLTE multi-modem), so per-line state only.

## Constitution Check

*GATE: passed before Phase 0, re-checked after Phase 1 (still passing).*

| Principle | Assessment |
|---|---|
| I. Integration-first testing | PASS. Tests drive the real agent loop against a loopback P-CSCF simulator over real sockets. Mocks only for the SIM (no hardware in CI), justified at the site, as the existing AKA tests already do. |
| II. Green-on-commit | PASS. Slices are ordered so each commit builds and passes `make test`; no commit leaves the old and new maintenance paths half-wired. |
| III. Frequent atomic commits | PASS. Tasks are sized to one logical change each; the two-line schedule fix is its own commit. |
| IV. Makefile-driven | PASS. Uses `make format`, `make lint`, `make test`; no new tooling. |
| V. Simplicity | PASS with one watched point. Slice C is the complex part and is deliberately deferred; Slice B reuses the existing pool, backoff and budget logic instead of adding parallel ones. No config switch is added (Clarification Q1). |

Repo rules from `CLAUDE.md` also apply: `make format && make lint && make test` before every commit; no real phone numbers or subscriber identifiers anywhere.

## Design Decisions (summary; detail in research.md)

1. **Schedule** — `renewal_headroom_for(granted)` returns 600 s if granted > 1200 s else granted / 2; the next-due instant is clamped so consecutive starts are ≥ 60 s apart. The now-unused fixed headroom constant goes away.
2. **Dedicated REGISTER sequence** — a registration-only CSeq counter on the session, separate from the shared request counter used by OPTIONS probes, SUBSCRIBE and calls, because the registration Call-ID is shared with those. Responses are matched on Call-ID + CSeq number + method + top-Via branch, so a late REGISTER answer can never confirm an OPTIONS probe with the same number (and the reverse).
3. **Authorization on refresh** — replay the cached AKA `Authorization` verbatim (username, realm, uri, last nonce, last response, and the qop/nc/cnonce it was computed with). TS 24.229 §5.1.1.4.2(a) says exactly "nonce = last received, response = last calculated". `integrity-protected` is inserted by the P-CSCF (§5.2.2.2), not by the UE, so the UE does not send it.
4. **Maintenance split** — `Maintenance::Renewal` is never deferred; `RegistrationRecovery` (full re-registration, transport replacement) and `Reattachment` are deferred while `busy()`, as today. `force_renewal` and the Gm-liveness escalation route to `RegistrationRecovery`.
5. **Failure routing** — one entry point turns a refresh outcome into the existing `on_renewal_failure` decision. Mapping from TS 24.229 §5.1.1.4.1: 408, 500, 504 and 403 → full re-registration (via `force_renewal`); 503 with Retry-After longer than Timer F (128 s) → mark the P-CSCF unavailable and fail over; 503 shorter than Timer F → retry the same P-CSCF after the Retry-After; 423 → one retry with Min-Expires; timeouts count as failures but not as "rejections" for the fallback. After 2 consecutive rejections the next cycle uses the full re-registration (FR-018a), then in-place is tried again.
6. **Ring-wait ordering** — registration answers and the refresh deadline are evaluated before and independently of any held PBX reply, and a held registration answer is never replayed after its transaction was already abandoned. (Audit task: find the exact hold point in `agent/inbound.rs` on current `main`.)
7. **Expiry mid-call** — the call is left alone, health shows unregistered, refresh retries continue on backoff, and `RegistrationRecovery` runs the moment `busy()` clears.
8. **Observability** — extend the status snapshot with last refresh result/time, next refresh, mode (in-place or fallback) and consecutive rejection count; surface them in `vowifi-status` and `volte-status`.
9. **Slice C** — background worker for fresh AKA, staged SAs, protected server port unchanged, client port new, activation only after success, rollback on failure, drain ≥ 128 s (extended while an origination is pending), and on UDP read replies from both the new socket and the dispatcher channel. SA kernel lifetime: the XFRM states are installed without a lifetime limit, so the "SA lifetime = max(existing, registration + 30 s)" rule in §5.1.1.4.2 needs no kernel-side change; recorded in research.md.

## Delivery Slices and Gates

| Slice | Stories | Ships when |
|---|---|---|
| A | US1 | `make format lint test` green; release note. Low risk, ship first. |
| B | US2, US3, US4, FR-018a, FR-027 | Green, plus live evidence: mid-call refresh on Jio VoWiFi and Vodafone VoWiFi (SC-002, SC-003 run ≥ 12 h, SC-005a). |
| C | US5 | Starts only after B's live evidence is recorded in `live-verification.md`; adds mid-call AKA and UDP evidence (SC-004, SC-005). |

## Project Structure

### Documentation (this feature)

```text
specs/123-ims-registration-refresh/
├── plan.md                    # this file
├── research.md                # standards answers + design decisions
├── data-model.md              # registration/refresh state
├── quickstart.md              # how to build, test and live-verify each slice
├── contracts/
│   └── status-output.md       # added fields in vowifi-status / volte-status
├── checklists/requirements.md
├── live-verification.md       # created during Slice B/C live runs (not by /speckit-plan)
└── tasks.md                   # /speckit-tasks output
```

### Source Code (repository root)

```text
gsm-sip-bridge/src/ims/
├── mod.rs                       # renewal_headroom_for / renewal_due; RegisteredSession gains registration_cseq + cached authorization; RegistrationStatus gains refresh fields
├── lifecycle.rs                 # Maintenance::{Renewal, RegistrationRecovery, Reattachment}; MaintenancePolicy::decide
├── session.rs                   # Inbound reader adoption (Slice C only)
├── sip_client.rs                # REGISTER builder (reused); response matching helpers
├── gm_ipsec.rs                  # staged SA install/activate/rollback (Slice C only)
├── registration_auth.rs         # NEW (Slice C): background AKA worker, staged association
└── agent/
    ├── mod.rs                   # on_idle_tick split into refresh vs recovery; failure routing; status publishing
    ├── registration_refresh.rs  # NEW (Slice B): PendingRegister transaction, response match, lifetime parsing
    ├── inbound.rs               # ring-wait ordering fix
    └── registration_tests.rs    # NEW: agent-level refresh tests

gsm-sip-bridge/src/vowifi/control.rs, vowifi/mod.rs         # vowifi-status wire format + rendering of the new refresh fields
gsm-sip-bridge/src/volte/bridge.rs                           # volte-status rendering of the same fields
gsm-sip-bridge/tests/test_ims_registration_refresh.rs      # NEW: loopback P-CSCF integration tests
docs/vowifi-bridge.md, docs/configuration.md (no new key), RELEASE_NOTES.md
```

**Structure Decision**: keep everything inside the existing `ims` module tree. One new file per slice (`registration_refresh.rs` for B, `registration_auth.rs` for C) so the large `agent/mod.rs` only gains glue. No new crate, no new config key.

## Complexity Tracking

No constitution violations. One note on size: Slice C is the part of PR #168 that is large (about 2,000 of its 4,500 lines, in `registration_auth.rs` and `gm_ipsec.rs`). It is isolated into its own slice so it can be reviewed and live-verified separately.
