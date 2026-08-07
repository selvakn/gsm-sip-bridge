# Implementation Plan: Carrier Signaling Connection Liveness & Automatic Reconnect

**Branch**: `028-gm-tcp-reconnect` | **Date**: 2026-08-07 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/028-gm-tcp-reconnect/spec.md`

## Summary

A registered line's Gm client connection can be reset silently and nothing in
the process notices, because the second connection of the pair keeps the mpsc
channel alive and no code path probes the idle socket. The line then reports
itself `Registered` while being unable to place or receive a call, until the
next scheduled renewal (~55 min) happens to rebuild it — or forever.

The fix adds an OPTIONS keepalive on a 2-minute idle timer inside
`ims::agent::dispatch_loop`, correlated asynchronously against the response
that already arrives on `inbound.rx`. A failed or unanswered ping drives
`RegisteredSession::reconnect_transport` (already used reactively by
`hangup_carrier`), confirmed by a follow-up ping before the line is declared
healthy; repeated failure escalates to a forced re-registration, which rebuilds
the SA and both readers. The Gm server listener gets a cheap `is_alive` flag so
its symmetric, currently-invisible death is detected too. Health is surfaced
through the existing three channels — `ServiceHealth`/status reply, a new
per-line gauge via the observability protocol, and a new `gm_connection_lost`
alert category evaluated by `metrics::ingest`'s existing `AlertPhase` machine.

Because `volte::carrier_agent` runs the same `dispatch_loop`, VoLTE is covered
without transport-specific code.

## Technical Context

**Language/Version**: Rust (workspace edition as pinned in `Cargo.toml`)
**Primary Dependencies**: existing only — `prometheus` (`GaugeVec`), `socket2`,
`serde`, `tracing`, `chrono`. No new crates.
**Storage**: N/A for the mechanism. Connection health is in-memory
(`dispatch_loop` stack + the shared `RegistrationStatus` mutex); the alert
phase lives in `metrics::ingest`'s per-module record, as today.
**Testing**: `cargo test` via `make test`; integration tests in
`gsm-sip-bridge/tests/`, unit tests in `#[cfg(test)]` modules. Real
`TcpListener`/`SipTransport` peers rather than mocked transports (Principle I).
**Target Platform**: Linux container (privileged, netns-per-line), ARM/x86.
**Project Type**: Single Rust workspace — a long-running daemon plus CLI.
**Performance Goals**: Detection within ~130s worst case (120s ping period +
10s response deadline). Added Gm traffic ≤ 30 request/response pairs per line
per hour. No added latency on call setup or on the healthy path.
**Constraints**: Must not read the Gm client socket outside the reader thread
(R1). Must not run mid-call (FR-006). Must not terminate the process on
escalation (FR-010a). Must not regress `hangup_carrier`'s reactive reconnect
(FR-008).
**Scale/Scope**: Up to N configured lines per host (multi-modem VoWiFi + VoLTE),
one `dispatch_loop` per line.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Status | Notes |
|---|---|---|
| I. Integration-First Testing | **PASS** | No new mocks. Detection is tested against a real `TcpListener` standing in for the P-CSCF, and `spawn_gm_server` against a real port (research R12). The one extracted pure function (`PingVerdict`) is state, not a mocked boundary. |
| II. Green-on-Commit | **PASS** | `make format && make lint && make test` before every commit; enforced per task in `tasks.md`. Note `test_config_docs.rs` will fail until `docs/configuration.md` documents the new alert key — that is the suite working as designed, and is sequenced as its own task. |
| III. Frequent Atomic Commits | **PASS** | Tasks are grouped into 7 commit-sized phases, each independently green: listener flag, ping mechanism, reconnect+escalation, health surface, metrics, alerting, docs. |
| IV. Makefile-Driven Build | **PASS** | No new build steps or targets. |
| V. Simplicity & Refactorability | **PASS** | Deliberately reuses rather than adds: `reconnect_transport`, `restart_client_reader`, the renewal branch (bypassing one `if`), `MaintenancePolicy`, `RETRY_INITIAL_BACKOFF`, `AlertPhase`, the observability protocol. New surface is one constant pair, one `AtomicBool`, one enum, one gauge, one alert category. The probe interval is a constant, not config — see Complexity Tracking for the one place this was consciously traded. |

**Post-Phase-1 re-check**: **PASS**, unchanged. Phase 1 design added no
abstraction layers; the largest new type is a 3-variant enum
(`GmConnectionState`) and the largest new function is the ping/verdict step.

## Project Structure

### Documentation (this feature)

```text
specs/028-gm-tcp-reconnect/
├── plan.md              # This file
├── spec.md              # Feature specification (4 clarifications resolved)
├── research.md          # Phase 0 — R1..R12
├── data-model.md        # Phase 1 — entities & state transitions
├── quickstart.md        # Phase 1 — how to exercise this by hand
├── contracts/
│   ├── observability-protocol.md   # AgentState.gm_connection_up
│   ├── status-reply.md             # RegistrationStatusReply additions
│   ├── metrics.md                  # new gauge + labels
│   └── config.md                   # [alerts.gm_connection_lost]
├── checklists/
│   └── requirements.md  # spec quality checklist (all pass)
└── tasks.md             # Phase 2 output (/speckit-tasks)
```

### Source Code (repository root)

```text
gsm-sip-bridge/src/
├── ims/
│   ├── agent.rs          # dispatch_loop: ping timer, verdict, reconnect,
│   │                     #   escalation flag; PingState; new constants
│   ├── session.rs        # restart_gm_server (mirrors restart_client_reader)
│   ├── sip_client.rs     # build_options(); GmServer::is_alive
│   ├── lifecycle.rs      # ServiceHealth.gm_connection_up + blocked_reason
│   ├── mod.rs            # RegistrationStatus.gm_connection / reconnecting_since
│   └── observability.rs  # set_gm_connection_up
├── control/protocol.rs   # AgentState.gm_connection_up;
│                         #   RegistrationStatusReply.gm_connection
├── metrics/
│   ├── mod.rs            # VOWIFI_GM_CONNECTION_UP gauge
│   └── ingest.rs         # gm phase + threshold + message + match arms
├── alerts/mod.rs         # AlertCategory::GmConnectionLost
├── config/
│   ├── raw.rs            # RawAlerts.gm_connection_lost + KEYS entry
│   ├── build.rs          # category(...) defaulting
│   └── mod.rs            # typed field + disabled() default
├── vowifi/mod.rs         # vowifi-status printer line
└── volte/bridge.rs       # volte-status printer line

gsm-sip-bridge/tests/
├── test_gm_connection_liveness.rs   # NEW — detection, reconnect, escalation
├── test_vowifi_health_metrics.rs    # extend — new gauge
├── test_ingest_critical_alerts.rs   # extend — failure/recovery pairing
└── test_config.rs                   # extend — new alert key

docs/
├── configuration.md      # [alerts.gm_connection_lost]
├── observability.md      # new gauge + status field
└── todo.md               # tick the item this closes
```

**Structure Decision**: No new modules or crates. Every change lands in an
existing file that already owns the concern — the liveness state machine in
`ims::agent` beside the renewal it shares a loop with, the transport repair in
`ims::session` beside `restart_client_reader`, the alert in `metrics::ingest`
beside the two categories it mirrors. The single new file is the integration
test.

## Implementation Phases

Each phase is a commit, green on its own.

**P1 — Listener liveness (smallest independent slice).**
`GmServer` gains `alive: Arc<AtomicBool>`, cleared by both the TCP and UDP
accept loops on their fatal-exit paths; `is_alive()` accessor. No consumer yet.
Test: real port, force the fatal path, assert the flip.

**P2 — OPTIONS builder + ping state.**
`build_options()` in `sip_client.rs` (out-of-dialog: Via/From/To/Call-ID/CSeq/
Max-Forwards/Content-Length, modelled on `build_in_dialog_request`).
`PingState` + the pure `verdict(now)` step in `agent.rs`. Unit tests only —
nothing is wired into the loop yet.

**P3 — Wire detection into `dispatch_loop`.**
Send on the idle timer when no call is active and no renewal is proceeding;
match the response at the existing `SipMessage::Response` arm; clear state on
session replacement (R11). Detection only — logs a warning, takes no action.
Test: real `TcpListener` peer answers once, then closes; assert the verdict.

**P4 — Reconnect, confirmation, escalation.**
On a dead verdict: `reconnect_transport` + `restart_client_reader` (client
half) or `restart_gm_server` (listener half), then an immediate confirming ping
(R7); mark up only on its response. Count consecutive failures; at
`MAX_RECONNECT_ATTEMPTS` set `force_renewal`, which bypasses only the
`renewal_due` early-continue. Reuses the existing backoff.

**P5 — Health surface.**
`ServiceHealth.gm_connection_up` + `can_answer` + `blocked_reason` (R9);
`RegistrationStatus.gm_connection` / `reconnecting_since`; wire into the status
listener and both CLI printers.

**P6 — Metrics.**
`AgentState.gm_connection_up` on the protocol, `set_gm_connection_up` on
`AgentObservability`, `VOWIFI_GM_CONNECTION_UP` gauge, ingest application.

**P7 — Alerting + docs.**
`AlertCategory::GmConnectionLost` and its six config touch points; ingest phase,
threshold, and message arms — including the `unreachable!` at `ingest.rs:313`,
which panics today if a third category reaches it. Then `docs/configuration.md`
(required by `test_config_docs.rs`), `docs/observability.md`, `docs/todo.md`.

## Complexity Tracking

Only one deviation was considered and is recorded for the reviewer; it is a
deliberate *reduction* below what the spec's surface might suggest, not an
addition.

| Decision | Why | Alternative rejected because |
|---|---|---|
| Ping interval is a constant, not a config key | Principle V / YAGNI. No evidence any carrier needs a different rate; the value is one line beside `RENEWAL_HEADROOM`. | A config key costs 6 plumbing touch points, a docs entry, and a validation path for a number nobody has yet needed to change. Revisit if a carrier objects to the probe rate. |
| Escalation bypasses one `if` rather than adding a re-registration routine | The renewal branch already does the whole job correctly, including the SA renegotiation that is the only fix for the R7 false-recovery case. | A parallel path would duplicate the attach hook, modem lock, backoff and status handling — the drift risk `ims::session` was extracted to prevent. |

## Known Gaps Carried Forward

- **Listener reachability**: R4's flag detects "the accept loop died," not
  "the listener is alive but unreachable from the network." No cheap signal
  exists for the latter. Documented, not fixed.
- **SC-010 needs hardware**: the originating incident was never reproduced
  synthetically. Tests bound the logic; a live re-run of the specs/025 T072
  pass-1 scenario is what actually closes this.
