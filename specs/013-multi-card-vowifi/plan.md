# Implementation Plan: Multi-Card VoWiFi

**Branch**: `013-multi-card-vowifi` | **Date**: 2026-07-15 | **Spec**: [./spec.md](./spec.md)
**Input**: Feature specification from `specs/013-multi-card-vowifi/spec.md`

## Summary

Give the VoWiFi path the same multi-card capability the circuit-switched path has had since
feature 004. Auto-discovery is extended to probe every recognized modem's serial interfaces for a
live AT response (instead of a fixed per-model interface number) and to stop excluding
audio-less/VoWiFi-only models. A single one-shot `discover` subcommand runs the scan and role
assignment exactly once and hands the result to both the circuit-switched daemon (which ports to
exclude) and `docker/entrypoint.sh` (which lines to bring up), removing the two-processes-probe-
the-same-serial-port race auto-discovery would otherwise introduce. Every singleton VoWiFi
resource feature 012 already kept parametrized (netns, XFRM if_id/iface, veth pair, control port,
vpcd slot/port, P-CSCF file, charon instance — with one shared pcscd across lines) becomes a pure function of a line's position in a
deterministically-ordered line table, so N lines replicate the single-line recipe N times rather
than sharing state. Agent A (tunnel/IMS-facing) becomes one process per line, launched into that
line's own netns exactly as today, selected via a new `--line` flag; Agent B (PBX-facing) stays
one process — one SIP registration — gaining one control-channel listener thread per line, with
line attribution coming from which listener accepted the connection rather than a wire-protocol
change. A single-SIM configuration resolves to exactly one line whose derived resources equal
today's unindexed defaults, by construction.

## Technical Context

**Language/Version**: Rust stable (pinned by `rust-toolchain.toml`), unchanged. bash for
`docker/entrypoint.sh` orchestration, extended to loop over a resolved line table instead of using
scalar env vars.

**Primary Dependencies**: No new Rust crates. Reuses `modules::at_commander`/`modules::usim`
(AT probing, SIM reads — already used by discovery and by `vowifi/imsi.rs`/`vowifi/plmn.rs`),
`serde`/`serde_json` (already a workspace dependency — used here for the new `LineResolution`
artifact), the existing `strongswan-epdg` fork and `vsmartcard`/`pcsc-lite` stack from feature 012
(replicated as independent instances, not extended with new plugin behavior).

**Storage**: None new. `LineResolution` is a transient JSON file at
`/tmp/gsm-sip-bridge-lines.json` (or `GSM_SIP_BRIDGE_LINES_FILE`), regenerated every startup —
not a durable store. The existing `sms` sqlite table gains no schema change; VoWiFi SMS rows
already carry a `module_id` column (today hardcoded to `"vowifi"`), which now carries each line's
real `card_id`.

**Testing**: `cargo test --workspace`. Discovery's AT-probe step is tested with a fake serial
transport mirroring `at_commander.rs`'s existing justified `MockStream`; the sysfs-walk step
reuses `test_discovery.rs`'s `tempfile`-backed fake-device-directory pattern. Role assignment,
line-table resolution, and per-line resource derivation (research.md item 5's formulas) are pure
functions, unit-tested directly with table-driven cases including the `N=1` back-compat identity
(FR-020) and the `max_lines` bound (FR-016). Agent B's per-line listener threading is tested the
same way `vowifi/mod.rs`'s existing `RecentCalls` tests are — in-process, no network hardware.
Multi-tunnel/multi-carrier live behavior (SC-002 through SC-006) is operator-run per
`quickstart.md`, the boundary every VoWiFi feature to date has drawn.

**Target Platform**: Linux, Alpine/musl container (unchanged image), host-kernel XFRM + network
namespaces — now N of them instead of one. No new privilege beyond what 011/012 already require.

**Project Type**: Extension of the existing `gsm-sip-bridge` binary (one new CLI subcommand, two
modified ones) + deployment surface (`docker/entrypoint.sh`). No new crate.

**Performance Goals** (from spec Success Criteria):
- 2 lines reach registered state within the same startup window a single line takes today
  (SC-002).
- Inbound call to either SIM answered/bridged ≤ 5s, including ≥ 12h after startup (SC-003).
- Two calls arriving within 5s of each other both bridged concurrently, no cross-talk (SC-004).
- A forced tunnel failure on one line recovers within 90s without affecting the other (SC-005).
- ≥ 24h uptime per line spanning a carrier rekey, zero agent restarts (SC-006).

**Constraints**:
- **Zero new `unsafe` in `gsm-sip-bridge/src`** (unchanged gate, `tools/count-unsafe.sh`) —
  satisfied by design: new code is discovery/config/threading, no FFI.
- Full pre-commit gate unchanged: `cargo fmt --all`, `make lint`, `cargo test --workspace`.
- FR-021: feature 004's circuit-switched multi-card behavior must be unchanged — the shared
  discovery scan's CS-facing output (`RoleAssignment.circuit_switched`) must be a strict subset of
  what `scan_modules()` finds today for any config where `[vowifi].enabled = false` or no
  audio-less modems are present, i.e. the CS pool for an all-audio-capable fleet is unaffected.
- FR-020: a single-SIM config must resolve identically to today (data-model.md's `i = 0` identity)
  — this is the acceptance bar for every per-line derivation formula in research.md item 5.
- The two-subsystems-race hazard (research.md item 3) means `entrypoint.sh`'s existing "start CS
  daemon supervisor, then handle VoWiFi" ordering must change to "run `discover` once, then start
  both" — a real behavior change to the entrypoint's startup sequence, called out explicitly since
  it touches the one thing 011/012 never had to (two subsystems racing over auto-discovered,
  previously hand-assigned, serial ports).

**Scale/Scope**: Up to `[vowifi].max_lines` (default 8) concurrent lines — small-deployment scale,
not carrier scale (spec's own Assumptions section). One call at a time per line, unchanged from
011/012; concurrency is across lines only.

## Constitution Check

*Gate: must pass before Phase 0. Re-checked after Phase 1 design — still passing.*

### I. Integration-First Testing — PASS
- AT-probing and sysfs-walk discovery tested against real byte-level fixtures/fake filesystems, no
  business-logic mocking (research.md item 8).
- The modem itself is the one mocked boundary (scripted transport) — hardware unavailable in CI,
  same justification already on record at `at_commander.rs`'s existing mock site; this feature
  adds no new *kind* of mock, only reuses the existing one for a new caller (discovery).
- Live multi-tunnel/multi-carrier/soak behavior validated against real hardware per
  `quickstart.md` — the correct integration boundary, matching every VoWiFi feature to date.

### II. Green-on-Commit — PASS (process gate)
- Every task below ends with the full pre-commit gate; CI needs no modem/charon/pcscd, so the
  workspace suite stays green throughout (discovery/resolution logic is pure Rust, unit-testable).

### III. Frequent Atomic Commits — PASS
- Phasing (discovery rewrite → role assignment/line-table resolution → `discover` CLI → per-line
  `VowifiConfig` derivation → Agent A `--line` selector → Agent B multi-listener → entrypoint.sh
  multi-line loop → status/metrics/observability → live proving) is sized for independently
  committable, testable steps, each ending green.

### IV. Makefile-Driven Build — PASS
- No new Makefile targets. New code is CLI surface inside the existing binary; Docker builds stay
  on the existing compose/Dockerfile flow (feature 012's strongswan/vpcd stages are reused, not
  replaced — only invoked N times by the entrypoint).

### V. Simplicity & Refactorability — PASS
- Replicating feature 012's proven single-line recipe N times (research.md item 4) is deliberately
  chosen over inventing shared-daemon multiplexing whose correctness (`eap-sim-pcsc` multi-reader
  behavior) is unresearched — the simpler, verified-safe option, even though "N processes" sounds
  like more moving parts than "one process, N connections." Complexity we own stays the same shape
  it already was, just parametrized by index instead of hardcoded.
- Per-line resource values are *derived*, not *configured* — no new per-line config surface for
  operators to hand-maintain and mistype into collision (data-model.md's validation rules).
- Agent B's threading change is the minimum needed to keep "one SIP identity" true while still
  telling lines apart — no new wire protocol (research.md item 6).

No violations — Complexity Tracking is empty.

## Project Structure

### Documentation (this feature)

```text
specs/013-multi-card-vowifi/
├── plan.md                                  ← this file
├── research.md                               ← Phase 0 output
├── data-model.md                             ← Phase 1 output
├── contracts/
│   ├── discover-cli-contract.md              ← new `discover` subcommand's I/O contract
│   └── agent-topology-contract.md             ← per-line agent process/threading + status/metrics
├── quickstart.md                             ← Phase 1 output (live verification runbook)
├── checklists/                               ← from /speckit-specify (pre-existing)
└── tasks.md                                  ← Phase 2 output (/speckit-tasks — not created here)
```

### Source Code Changes

```text
gsm-sip-bridge/src/
├── modules/
│   ├── discovery.rs        MODIFY — replace KNOWN_DEVICES' at_interface_number lookup with live
│   │                                 AT-probing across a device's ttyUSB* interfaces; stop
│   │                                 skipping audio-less models; add SIM-identity read
│   │                                 (ProbedModem, data-model.md)
│   └── mod.rs               MODIFY (minor) — CardPool::new/scan_modules call site accepts an
│                                              excluded-ports set (FR-007), read from
│                                              LineResolution
├── vowifi/
│   ├── mod.rs                MODIFY — role assignment, LineTable resolution, per-line
│   │                                   VowifiConfig derivation (research.md item 5);
│   │                                   `run_inner`/`handle_connection` reworked for N listener
│   │                                   threads + per-line RecentCalls map; print_status iterates
│   │                                   every line
│   ├── discovery.rs          NEW — RoleAssignment + LineTable + LineResolution
│   │                                (data-model.md), the `discover` subcommand's core logic
│   ├── imsi.rs / plmn.rs     MODIFY (minor) — invoked per-line already (take `--modem`); no
│   │                                          interface change, just called N times
│   └── usim_bridge.rs        UNCHANGED — already takes `--vpcd-port`; each line connects to its
│                                          own vpcd slot on the one shared pcscd (research.md item 4)
├── ims/agent.rs               MODIFY (minor) — no behavior change; confirms it already takes
│                                                 `&VowifiConfig` with no global-singleton
│                                                 assumption baked in
├── config/mod.rs               MODIFY — VowifiConfig gains `max_lines`; parses optional
│                                          `[[vowifi.line]]` override entries (explicit modem
│                                          assignment, FR-009)
├── cli.rs                     MODIFY — new `Discover` subcommand (`--out`, `--shell-env`); new
│                                        `--line <index>` flag on `VowifiImsAgent`
└── main.rs                    MODIFY — dispatch `discover`; `--line` plumbed into
                                          `handle_vowifi_ims_agent_command`; CS daemon startup
                                          reads LineResolution's excluded-ports before
                                          CardPool::new

gsm-sip-bridge/tests/
├── test_discovery.rs           MODIFY — AT-probe fixtures, audio-less-model-not-skipped cases
└── test_vowifi_lines.rs        NEW — role assignment, line-table resolution, per-line resource
                                        derivation (including the N=1 back-compat identity and the
                                        max_lines bound), table-driven

docker/
└── entrypoint.sh               MODIFY — run `discover` once up front (before the CS daemon
                                          supervisor starts); loop the existing per-line block
                                          (netns/XFRM iface, pcscd+vpcd+usim-bridge, charon,
                                          swanctl render+initiate, veth pair, Agent A supervisor)
                                          over `LINE_COUNT`; Agent B started once, outside the
                                          loop, after all lines' veth pairs exist
```

**Structure Decision**: Everything stays inside the existing `gsm-sip-bridge` binary and the
existing `docker/` deployment surface — no new crate, no new top-level directory. The multi-line
capability is entirely a matter of (a) a new discovery/resolution module, (b) parametrizing
existing per-line logic by index instead of assuming one, and (c) an entrypoint loop — the same
"replicate the proven single unit" shape feature 004 already established for the circuit-switched
side.

## Implementation Phases (proposed commit-sized slices)

1. **Discovery rewrite** (`modules/discovery.rs` + `test_discovery.rs`): live AT-probing across a
   device's serial interfaces, replacing the `at_interface_number` table lookup; stop skipping
   audio-less models; add SIM-identity read. Circuit-switched behavior for an all-audio-capable
   fleet unchanged (FR-021) — verified by existing CS discovery tests continuing to pass.
2. **Role assignment + line-table resolution** (`vowifi/discovery.rs` NEW +
   `test_vowifi_lines.rs` NEW): `ProbedModem` → `RoleAssignment` → `LineTable`, pure functions,
   table-driven tests including the N=1 identity (FR-020) and the `max_lines` bound (FR-016).
3. **Per-line `VowifiConfig` derivation**: the research.md item 5 formulas, unit-tested directly;
   `config/mod.rs` gains `max_lines` + optional `[[vowifi.line]]` parsing.
4. **`discover` CLI subcommand** (`cli.rs`, `main.rs`, `vowifi/discovery.rs`): JSON + `--shell-env`
   output per the discover-cli-contract; CS daemon startup reads the excluded-ports list before
   `CardPool::new`.
5. **Agent A `--line` selector**: `vowifi-ims-agent --line N` loads the resolved per-line config;
   no other change to `ims/agent.rs`.
6. **Agent B multi-listener rework**: N accept-loop threads over one shared
   `Endpoint`/`Account`/Discord client/store; `RecentCalls` → per-`card_id` map; SMS forwarding and
   every log line tagged with the owning line's `card_id` (FR-017).
7. **`vowifi-status` + metrics**: iterate every line (FR-018); new `vowifi_*` metric families
   labeled `card_id` (FR-017, agent-topology-contract.md); health check considers every line
   (FR-019).
8. **`docker/entrypoint.sh` multi-line loop**: run `discover` once up front; loop the existing
   per-line block over `LINE_COUNT`; Agent B started once, after all lines' veth pairs exist; zero
   usable lines degrades (logs prominently, skips VoWiFi, CS daemon still starts) rather than
   failing the container (spec's clarification).
9. **Live proving per `quickstart.md`**: two-modem discovery, two independent tunnels, concurrent
   calls, fault isolation, 24h soak, attribution, and the single-SIM back-compat check
   (SC-001 through SC-008).

## Complexity Tracking

*No entries — Constitution Check passed without deviations to justify.*
