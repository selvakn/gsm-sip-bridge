# Implementation Plan: Hold VoWiFi recovery while the path to the ePDG is down

**Branch**: `100-vowifi-underlay-guard` | **Date**: 2026-10-20 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `specs/100-vowifi-underlay-guard/spec.md`

## Summary

The change has five parts. The half-open fix stands alone; the guard builds on it.

**1. Half-open IKE_SA (US3).**
- `swanctl --list-sas` headers in state `CONNECTING` become `SteadyStateHealth::IkeSaConnecting { ike_id }` instead of falling into the two-strike CHILD_SA path.
- The tick leaves them alone while charon has logged IKE activity for that connection in the last 200s. With `keyingtries = 0`, charon keeps one half-open `#N` alive for a whole outage (research R6), so age alone means nothing.
- After 200s of silence, and only when not held, it terminates and re-initiates as a new `DegradeReason::IkeSaNegotiationStuck`.

**2. Liveness check on demand (FR-004a).**
- A patch to the strongSwan fork, `docker/patches/0002-strongswan-vici-dpd.patch`, adds a vici `dpd` command and `swanctl --dpd --ike <name>`. Together they queue one ordinary DPD on an established IKE_SA (research R5).
- The supervisor reads the answer from the charon log at the next tick, so nothing blocks under the shutdown guard (R8).

**3. Underlay verdict (US1, US2).**
- At the top of each steady-state tick the loop computes an `UnderlayView` for the line's ePDG and passes it into the tick:
  - *no route* comes from `ip route get <ePDG>` (R3);
  - *no response* comes from retransmits with no evidence of life (R4).
- The tick classifies every recovery as reachability-inferred or structural (data-model.md, exhaustive).
- It then applies the decision table: hold, defer for a liveness check, or proceed. It returns `Held` or `Deferred` instead of issuing a held recovery.
- The loop owns the hold lifecycle:
  - the 15-minute bound;
  - one initiation when there is no IKE_SA (FR-010a, R7);
  - `negotiated_at` reset at hold end (FR-011);
  - initiate-on-restore (FR-013).

**4. Observability (US4).**
- `AgentState.uplink`, events `uplink_hold_started`/`uplink_hold_ended`, three metrics, `DeadPeerVerdict::Uplink`, the `ike_sa_negotiation_stuck` recovery label.
- An alert-text suffix when the existing tunnel-failure alert fires during a hold.
- `/tmp/pcscf-N.uplink`, read by `vowifi-status`.

**5. Config, docs, live.**
- `[vowifi].underlay_guard` (default on), plus docs and changelog.
- The quickstart's outage runs on both rigs, recorded in `live-verification.md`.

## Technical Context

**Language/Version**: Rust (workspace toolchain, `rust-toolchain.toml`). C for the strongSwan fork patch (strongSwan 5.9.3, osmocom `022f3e515`).
**Primary Dependencies**: none new. The fork is already built from source in `docker/Dockerfile`'s `strongswan-builder` stage.
**Storage**: one new per-line tmpfs file, `/tmp/pcscf-N.uplink`, in the existing `.pool`/`.state`/`.calls` family.
**Testing**: `cargo test` via `make test`.
- **Pure units (`line_supervisor.rs`):**
  - the decision table, one test per row (data-model.md);
  - `is_reachability_inferred` exhaustiveness;
  - half-open waiting vs stuck, with injected activity times;
  - hold lifecycle (debounce, 15-minute bound, FR-010a single initiation, FR-011 `negotiated_at`).
- **Parsers (`engines.rs`), over real charon/swanctl text captured on the rig:**
  - header state + `#N`;
  - last IKE activity per connection;
  - evidence of life since a mark;
  - retransmits-unanswered;
  - `ip route get` success/failure/no-`src`.
- **Engine:** `MockCommandRunner` scripts for `swanctl --dpd` (exit 0 / non-zero) and `ip route get`. The mock's justification (charon, netns, root) is already documented at its definition.
- **Wire + ingest:**
  - `AgentState.uplink` and both events round-trip;
  - counters and gauge;
  - alert-text suffix present only while held;
  - `DeadPeerVerdict::Uplink`;
  - extend `tests/test_vowifi_tunnel_recovery_metrics.rs` through a real control socket.
- **Status file:** write/read round-trip and `vowifi-status` formatting (`up`, held with duration, `unknown`).
- **Config:** default, `false`, non-boolean rejected; `tests/test_config_docs.rs`.

**Target Platform**: Linux container (amd64/arm64), host networking.
**Project Type**: single Rust workspace (`gsm-sip-bridge` crate) plus one C patch to a vendored build.
**Performance Goals**:
- No new outer traffic on a healthy line (SC-006).
- Per tick, per strongSwan line, one extra `ip route get` exec and one extra pass over the charon log text already read by the tick.
- `swanctl --dpd` is called only on suspicion, at most once per pending recovery.

**Constraints**:
- Guard off ≡ pre-feature behaviour, apart from the half-open fix (FR-025, SC-008).
- Structural recoveries are never held (FR-009).
- Nothing blocks under the shutdown read guard (R8).
- swu and VoLTE are untouched (FR-024).

**Scale/Scope**: 1–8 lines per host. Rough production size:
- fork patch ~120 lines C;
- `line_supervisor.rs` ~220;
- `engines.rs` ~150;
- `orchestrate.rs` ~160;
- protocol/metrics/ingest ~120;
- `vowifi/mod.rs` + `pcscf_files.rs` ~60;
- config ~30.

Plus tests, docs and changelog.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Status |
|---|---|
| I. Integration-first testing | **Pass.** Parsers run on real captured charon/swanctl text. Events go through a real control socket into the real registry. The tick is tested through the existing `FakeEngine` and `MockCommandRunner`, whose justification (charon, a netns, root) is documented at their definitions. The fork patch is exercised by the image build and the quickstart's step 1 on a live rig. |
| II. Green-on-commit | **Pass.** `make format && make lint && make test` before every commit, with `CARGO_TARGET_DIR=$PWD/target-100` (shared target-dir clobbering). |
| III. Atomic commits | **Pass.** Planned commits: (1) spec artifacts; (2) half-open fix; (3) fork patch + Dockerfile; (4) parsers; (5) verdict + decision table in the tick; (6) loop hold lifecycle; (7) wire/metrics/ingest/alert text; (8) status file + `vowifi-status`; (9) config; (10) docs + changelog; (11) live verification. |
| IV. Makefile-driven | **Pass.** No new targets. The patch is applied by the existing image build. |
| V. Simplicity | **Pass.** No new process, socket or alert type. The verdict is an input to the existing pure tick, and the hold lifecycle reuses 097's `negotiated_at` mechanism. The liveness check reuses charon's own DPD task rather than a new probe. The one new external piece, the fork patch, is the smallest way to meet clarification Q1. |

Re-checked after Phase 1: unchanged. The data-model added three `SteadyOutcome` variants and one tracker struct, all inside the existing loop/tick split.

## Project Structure

### Documentation (this feature)

```text
specs/100-vowifi-underlay-guard/
├── spec.md
├── plan.md                 # this file
├── research.md             # R1–R13
├── data-model.md           # verdict, view, tracker, decision table, lifecycle
├── quickstart.md           # live recipe (both rigs)
├── contracts/
│   ├── config.md           # [vowifi].underlay_guard
│   ├── dpd-command.md      # vici `dpd` + swanctl --dpd
│   ├── observability.md    # AgentState.uplink, events, metrics, alert text, logs
│   └── status-file.md      # /tmp/pcscf-N.uplink and vowifi-status output
├── checklists/requirements.md
├── tasks.md                # /speckit-tasks
└── live-verification.md    # written after the rig runs
```

### Source Code (repository root)

```text
docker/
├── Dockerfile                                   # apply 0002 patch in strongswan-builder
└── patches/0002-strongswan-vici-dpd.patch       # new: vici `dpd`, swanctl --dpd
gsm-sip-bridge/src/
├── supervise/
│   ├── line_supervisor.rs   # IkeSaConnecting, IkeSaNegotiationStuck, UnderlayView/Verdict,
│   │                        # is_reachability_inferred, decision table, Held/Deferred/WaitingForNegotiation
│   ├── engines.rs           # header state+#N, last IKE activity, evidence of life, retransmits,
│   │                        # route_ok (ip route get), send_dpd (swanctl --dpd); TunnelEngine methods
│   └── orchestrate.rs       # UnderlayTracker, hold lifecycle, status file, signal/events, startup log
├── control/protocol.rs      # AgentState.uplink, UplinkState, events, DeadPeerVerdict::Uplink,
│                            # TunnelRecoveryReason::IkeSaNegotiationStuck
├── metrics/{mod,ingest}.rs  # 3 metrics, ingest of state/events, alert-text suffix
├── ims/pcscf_files.rs       # uplink_path, write/parse
├── vowifi/mod.rs            # print_status: uplink line
└── config/{raw,build,mod}.rs # underlay_guard
gsm-sip-bridge/tests/
├── test_vowifi_tunnel_recovery_metrics.rs   # extended
└── test_config_docs.rs                       # unchanged; must pass
docs/{configuration,operations}.md, config.toml.example, CHANGELOG.md
```

**Structure Decision**: same single-crate layout as 096–098. The guard lives in the existing supervise modules: the decision in `line_supervisor.rs`, I/O in `engines.rs`, lifecycle in `orchestrate.rs`. No new module.

## Phase 0: Research

Done. See [research.md](research.md). No open NEEDS CLARIFICATION remain. Two findings amended the spec:
- R6: "stuck" is measured by IKE inactivity, not attempt age (FR-015/016, US3, SC-007);
- R7: one initiation while held with no IKE_SA (FR-010a).

## Phase 1: Design & Contracts

Done. See [data-model.md](data-model.md), [contracts/](contracts/) and [quickstart.md](quickstart.md). The agent context (`CLAUDE.md` SPECKIT block) points at this plan.

## Risks

| Risk | Mitigation |
|---|---|
| A wrong *no response* verdict holds a fixable line. | The verdict needs both retransmits/an unanswered DPD **and** no evidence of life. Any life ends it. 15-minute bound. Kill switch. US2 live tests. |
| The fork patch doesn't build on musl, or conflicts at `022f3e515`. | It touches 3 files with no new dependencies. The image build is the first check (task order puts it early). |
| charon log rotation or truncation loses the mark. | The existing `log_mark` handles truncation (096/097 reuse it). A lost mark reads as no evidence, so the pending check stands and the next tick decides. |
| The upstream-dead case looks like "ePDG down while the internet works". | Holding is correct there too: each line's ePDG address is fixed for the container's life (spec Edge Cases). |

## Complexity Tracking

No constitution violations to justify.
