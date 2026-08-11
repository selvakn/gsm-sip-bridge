# Implementation Plan: Card Phone Number and Instance Identity in Alerts

**Branch**: `034-alert-identity` | **Date**: 2026-08-11 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/034-alert-identity/spec.md`

## Summary

Add two identity attributes to **every** Discord alert — both forwarded-SMS and
critical-event notifications: the affected card/line's **phone number** and an
**instance name** for the host/deployment. Phone number resolves from operator
config per line (reusing VoLTE's existing `msisdn`, extended to VoWiFi), falling
back to the SIM-read `AT+CNUM` value for circuit-switched (CS) cards; when nothing
resolves the field shows the literal `unknown` (clarified 2026-08-11). Instance
name comes from a new optional `[alerts].instance_name`, falling back to the
system hostname via `libc::gethostname`. Both fields are always present.

## Technical Context

**Language/Version**: Rust (workspace pinned by `rust-toolchain.toml`)
**Primary Dependencies**: reqwest + serde_json (Discord embeds), tokio (async
dispatch), `libc = "0.2"` (already a dependency — `gethostname`), prometheus
**Storage**: N/A (config-sourced; no schema change)
**Testing**: `cargo test` via `make test`; `wiremock` (HTTP capture) + `insta`
(payload snapshots) already used by the alerts suite
**Target Platform**: Linux host and Docker container (multi-process: daemon +
supervise-spawned `vowifi-ims-agent` / `volte-carrier-agent`)
**Project Type**: Single Rust workspace (daemon/CLI) — `gsm-sip-bridge/`
**Performance Goals**: Alert dispatch is fire-and-forget; must not add latency to
call/SMS/AT hot paths (FR-009). Hostname read once per process at client build.
**Constraints**: Config is the only cross-process-consistent phone source; the
daemon's `metrics/ingest` alert path holds only `unit_id`, so it resolves numbers
from a config-built `unit_id → msisdn` map.
**Scale/Scope**: A handful of cards/lines per deployment; ~7 alert categories.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

- **I. Integration-First Testing**: PASS. Tests exercise the real `DiscordClient`
  POST against a `wiremock` server and assert on the captured JSON body (existing
  pattern); `instance_label`/hostname fallback and the `unit_id → msisdn` resolver
  are pure functions tested directly. No new mocks of internal boundaries.
- **II. Green-on-Commit**: PASS. `make format && make lint && make test` gate
  every commit (CLAUDE.md pre-commit checklist).
- **III. Frequent Atomic Commits**: PASS. Work splits into config → central
  render (instance) → phone threading → per-call-site population, each committable.
- **IV. Makefile-Driven Build**: PASS. All operations via existing `make` targets.
- **V. Simplicity & Refactorability**: PASS. Reuses the existing `msisdn` field
  and the `Option`-carrying `CriticalEvent`; adds two `Option<String>` fields and
  one string on the client — no new abstraction or layer.

No violations → Complexity Tracking not required.

## Project Structure

### Documentation (this feature)

```text
specs/034-alert-identity/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/
│   └── discord-embed.md # Phase 1 output — the alert embed contract
├── checklists/
│   └── requirements.md  # from /speckit-specify
└── tasks.md             # /speckit-tasks output
```

### Source Code (repository root)

```text
gsm-sip-bridge/src/
├── alerts/
│   ├── mod.rs           # CriticalEvent.phone_number; instance_label + system_hostname helpers
│   └── discord.rs       # DiscordClient.instance + new() param; Phone field + footer in both embeds
├── config/
│   ├── mod.rs           # AlertsConfig.instance_name; VowifiLineOverride.msisdn
│   ├── raw.rs           # [alerts].instance_name; [[vowifi.line]].msisdn parsing
│   └── build.rs         # wire raw → resolved
├── sms/mod.rs           # phone_number param on record_and_forward → forward_sms
├── modules/
│   ├── worker.rs        # cache AT+CNUM; set phone_number on ModuleLifecycle events
│   ├── slot.rs          # (existing) SlotState.phone_number source
│   └── pool/{mod.rs,dispatch.rs}  # populate phone on CS alerts + SMS
├── metrics/ingest.rs    # unit_id → msisdn map at init_alerts; look up in dispatch_transition
├── supervise/orchestrate.rs  # phone lookup for LineDiscoveryFailed from resolved lines
└── vowifi/mod.rs        # pass configured msisdn on VoWiFi SMS
config.toml.example      # document [alerts].instance_name and [[vowifi.line]].msisdn
```

**Structure Decision**: Single existing Rust workspace. Central rendering of both
new fields lives in `alerts/discord.rs`; identity data is threaded through the
existing `CriticalEvent` payload and `record_and_forward`/`forward_sms` signatures,
plus a config-built `unit_id → msisdn` map for the daemon-detected categories.

## Design highlights

- **Instance name (process-global)**: `alerts::instance_label(&AlertsConfig)` =
  configured `instance_name` if non-empty, else `alerts::system_hostname()`
  (`libc::gethostname`, fallback `"unknown"`). Stored as `DiscordClient.instance`
  set at each `DiscordClient::new` site (daemon, pool ×2, supervise, vowifi); both
  `forward_sms` and `send_alert` render it in `footer.text`.
- **Phone number (per unit)**: reuse `[[volte.line]].msisdn`; add
  `[[vowifi.line]].msisdn`. Add `phone_number: Option<String>` to `CriticalEvent`
  and a `phone_number: Option<&str>` arg to `forward_sms` + `record_and_forward`.
  Render a `Phone` embed field in both builders, using the value or the literal
  `unknown` when `None`/empty (FR-005).
- **Population**: CS SMS + CS ModuleLifecycle from `SlotState.phone_number`
  (pool) / cached `AT+CNUM` (worker); VoWiFi SMS from configured line `msisdn`;
  RegistrationLoss/TunnelFailure/GmConnectionLost + LineDiscoveryFailed from the
  `unit_id → msisdn` config map.

## Complexity Tracking

No constitution violations — section intentionally empty.
