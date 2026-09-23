# Implementation Plan: Multi-Carrier VoLTE P-CSCF Priming

**Branch**: `081-multi-carrier-pcscf` | **Date**: 2026-09-23 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/081-multi-carrier-pcscf/spec.md`

## Summary

specs/080-volte-pcscf-auto-prime made `supervise` auto-capture a P-CSCF for
VoLTE's line 0 and write it to the single, config-wide
`[volte].pcscf_source_path`. Every VoLTE line then reads that same literal
path (`src/commands/volte.rs`'s `resolve_line_pcscf`) — correct only when
every line shares one carrier. This feature makes each line capture and
read **its own** address.

Three changes, all additive to 080's existing machinery, none of it
rewritten:

1. **Read side** (`resolve_line_pcscf`, `src/commands/volte.rs`): add a
   middle resolution tier between the existing "explicit override" and
   "legacy shared file" tiers — a per-line cache file keyed by the line's
   stable `card_id`, e.g. `/tmp/pcscf-0-ec20-ABCDEF`
   (`volte::pcscf::per_line_cache_path`, new pure function). Keying by
   `card_id` rather than a line's discovery-order `index` is required by
   FR-003: `resolve_volte_lines` sorts candidates by `card_id` before
   assigning indices, so a modem's index shifts whenever the discovered
   modem *set* changes, even though that modem itself didn't move.

2. **Write side** (`src/supervise/orchestrate_prime.rs`,
   `orchestrate_volte.rs`): generalize "prime exactly VoLTE's line 0, into
   the one global path" into "prime every VoLTE line that lacks a usable
   address, each into its own `card_id`-keyed path, concurrently." The
   concurrency piece reuses the *exact* pattern the real, hardware-proven
   persistent multi-line VoWiFi subsystem (`start_vowifi_subsystem`)
   already uses for N simultaneous real lines — one shared charon
   instance serving every line's connection at once — rather than
   inventing new concurrency machinery: a **priming pass** (this feature's
   one new concept) is the transient, capture-only analogue of that
   subsystem, scoped to only the lines that need capturing, torn all the
   way down afterward.

3. **Per-line gating**: each VoLTE line's existing supervision thread
   (`start_multiline`'s per-line `std::thread::spawn` loop, already one
   thread per line) gains a wait-for-my-own-address step before its first
   `volte-carrier-agent` spawn, using the same three-tier resolution as
   (1). No new synchronization primitive: this is the same per-line thread
   that already exists, so per-line independence (FR-005/FR-009) and
   concurrent priming (Clarifications) fall out of structure that's
   already there.

A deployment where every line already has a usable address (override or
valid own-line cache) triggers zero priming-pass activity — same
availability check as 080, evaluated per line before deciding whether a
pass is needed at all (SC-003).

## Technical Context

**Language/Version**: Rust 1.94.0 (workspace `rust-toolchain.toml`, edition 2021)
**Primary Dependencies**: No new external crate. Extends existing in-tree modules: `crate::supervise::{orchestrate, orchestrate_prime, orchestrate_volte, engines, shutdown, vpcd, epdg_iface}`, `crate::volte::{pcscf, discovery}`, `crate::vowifi::discovery`
**Storage**: New per-line cache files alongside the existing single `[volte].pcscf_source_path` file — `<pcscf_source_path>-<card_id>`, plain text, one IP address, same format `probe_epdg_cache` already parses. No new format, no database.
**Testing**: `cargo test` across the workspace. The new priming-pass logic is exercised through `supervise::runner::MockCommandRunner`, the same `CommandRunner` seam `orchestrate.rs`'s existing multi-line (`start_vowifi_subsystem`) and single-line (`orchestrate_prime`) tests already use — no new mocking concept. `discover_priming_line`'s live-modem-scan boundary (same limitation 080 already documented) still cannot be unit tested; the per-line gating/resolution decision logic around it is.
**Target Platform**: Linux (Docker, `network_mode: host`) — unchanged from 080, this only ever runs inside `supervise`'s own orchestrated startup (FR-002a-equivalent scope carried forward unchanged from specs/080).
**Project Type**: Single Cargo workspace member (`gsm-sip-bridge`), changes live in `src/supervise/` and `src/volte/`
**Performance Goals**: N/A — a priming pass runs at most once per boot, before VoLTE registration; concurrency here is about correctness (not leaving other lines' registration waiting on one slow/failed line, FR-005) and reusing proven multi-line bring-up, not throughput.
**Constraints**:
- Must not change the observable behavior of the real, persistent
  `[vowifi]` subsystem (`start_vowifi_subsystem` and friends) — the
  priming pass calls into the same per-line establish primitives
  (`prepare_vowifi_line`, `establish_line_tunnel`) 080 already reuses
  unchanged; only the *pass-scoping* logic (which lines, which shared
  charon instance, which per-line resource indices) is new.
- Must not change observable behavior for a deployment with zero lines
  needing capture (SC-003) — same "already available" gate as 080, now
  checked per line before a pass is even considered.
- `vowifi::discovery::resolve_single_line` currently derives every
  per-line resource (netns, `strongswan_tun_iface`, `strongswan_if_id`,
  veth addresses, `vpcd_port`) from a **hardcoded `index = 0`**
  (`resolve_one_line(0, modem, base)`). That is safe today because only
  one priming attempt ever runs at a time; it is not safe for a
  concurrent multi-line pass — two lines primed together with both
  resolved at index 0 would collide on netns name, tunnel interface,
  XFRM if_id, veth addresses, and vpcd port. This function must gain an
  explicit pass-local index parameter (or a sibling function), assigned
  densely (0..N) across only the lines in *this pass* — deliberately not
  each line's real VoLTE index, and never used for the cache filename
  (that stays `card_id`-keyed per FR-003) — purely to keep the transient
  capture's own internal resources collision-free while it runs.
- Cannot be validated against real multi-carrier hardware in this
  environment (no root/CAP_NET_ADMIN in this sandbox — project memory:
  sandbox blocks root network testing). The concurrent-pass mechanism
  reuses the real persistent subsystem's already-hardware-proven shared-
  charon pattern, but running it *transiently, then tearing it down, then
  immediately letting each line's own VoLTE registration reuse the same
  modem* — same same-process hand-off caveat 080 already flagged for one
  line — now applies per line, N times in the same pass. This remains the
  one thing that most needs a live multi-carrier rig pass before full
  trust; flagged again in quickstart.md.
**Scale/Scope**: One new concept (a "priming pass" covering 1..N lines,
replacing 080's single-line `prime_pcscf`), one new pure function
(`volte::pcscf::per_line_cache_path`), a generalized resource-derivation
entry point in `vowifi::discovery`, a rewritten `resolve_line_pcscf` middle
tier in `commands/volte.rs`, and a restructured (not rewritten)
`orchestrate_prime.rs` / `orchestrate_volte.rs::start_multiline` pre-flight.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

- **Integration-First Testing**: The priming pass is built from the same
  already-integration-tested building blocks 080 used
  (`prepare_vowifi_line`, `establish_line_tunnel`, `shutdown::
  build_shutdown_plan`/`execute_shutdown_plan`), exercised through
  `MockCommandRunner` — the multi-line version of the same seam
  `start_vowifi_subsystem`'s own tests already use for N simultaneous real
  lines. No new mocking concept. The live-hardware gap (same-process,
  multi-line capture-then-handoff) is called out explicitly rather than
  hidden, same as 080's own precedent.
- **Green-on-Commit**: Each structural step (generalize
  `resolve_single_line`'s indexing → add `per_line_cache_path` → extend
  `resolve_line_pcscf`'s tiers → restructure the priming pass →  wire
  per-line gating into `start_multiline`) is its own commit, `cargo test`
  green before the next.
- **Frequent Atomic Commits**: Natural break points listed above under
  Green-on-Commit map directly to tasks.md's task boundaries.
- **Makefile-Driven Build**: No new build steps; `make format`/`make
  lint`/`make test` cover everything this feature touches.
- **Simplicity & Refactorability**: The central decision — reuse
  `start_vowifi_subsystem`'s proven one-shared-charon/N-connections
  pattern for the priming pass, and reuse each VoLTE line's *existing*
  supervision thread as the per-line gate — exists specifically to avoid
  two things this feature could easily have grown: a second, divergent
  multi-line bring-up implementation, and a new synchronization primitive
  for "wait for my own line's address." Neither is introduced.

**Result**: PASS, no violations to justify.

## Project Structure

### Documentation (this feature)

```text
specs/081-multi-carrier-pcscf/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md         # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/
│   └── volte-multi-carrier-prime-contract.md
└── tasks.md              # Phase 2 output (/speckit.tasks — not this command)
```

### Source Code (repository root)

```text
gsm-sip-bridge/
└── src/
    ├── volte/
    │   └── pcscf.rs             # New: `per_line_cache_path(base: &str, card_id: &str) -> PathBuf`
    │                            # — FR-003's key-by-card_id formula, pure and unit-tested
    │                            # alongside the existing `probe_epdg_cache`/`pcscf_is_available`.
    ├── vowifi/
    │   └── discovery.rs          # `resolve_single_line` generalized to take an explicit
    │                            # pass-local index instead of a hardcoded 0 (or a new sibling
    │                            # `resolve_priming_line(modem, base, pass_index)`); no change to
    │                            # `resolve_lines`/`resolve_one_line`'s own behavior for real,
    │                            # persistent lines.
    ├── commands/
    │   └── volte.rs              # `resolve_line_pcscf` gains the new middle tier
    │                            # (`per_line_cache_path`) between the existing explicit-override
    │                            # and legacy-shared-file tiers. `volte_bridge_manifest_lines`
    │                            # callers unchanged — same function signature.
    └── supervise/
        ├── orchestrate_prime.rs  # Restructured: `discover_priming_line` (singular) becomes
        │                        # `discover_priming_lines` (plural) — every VoLTE line lacking a
        │                        # usable address, each paired with its own resolved
        │                        # `LineResolutionEntry` at a distinct pass-local index.
        │                        # `prime_pcscf`/`prime_with_line` become a pass-scoped
        │                        # `prime_pass(lines: &[LineResolutionEntry], ...)`: one shared
        │                        # charon + one shared pcscd for the whole pass (mirroring
        │                        # `start_vowifi_subsystem`), each line's `establish_line_tunnel`
        │                        # running on its own thread so a stuck/failed line never blocks
        │                        # the others (FR-005), each success writing to that line's own
        │                        # `card_id`-keyed path (not the global one), `tear_down`
        │                        # generalized from one line's resources to the whole pass's.
        └── orchestrate_volte.rs  # `start_multiline`'s single pre-flight block (today: one
                                  # `ensure_pcscf_primed` call gated on line 0's override) becomes:
                                  # compute which manifest lines still lack a usable address (their
                                  # own per-line cache, per the new tier), and if any do, run one
                                  # `prime_pass` covering exactly those lines before the existing
                                  # per-line spawn loop. Each per-line spawn loop (already one
                                  # thread per line) re-resolves its own line's address via the
                                  # same three-tier check immediately before its first
                                  # `volte-carrier-agent` spawn, retrying on the existing 15s
                                  # cadence — this is the per-line, non-blocking gate FR-005/FR-009
                                  # require, with no new signaling mechanism.
```

**Structure Decision**: No new files beyond the one new pure function
(`volte::pcscf::per_line_cache_path`); everything else is a restructuring
of 080's existing single-line priming into a pass-scoped, multi-line one,
following the same module boundaries 080 established
(`orchestrate_prime.rs` owns the capture mechanics, `orchestrate_volte.rs`
owns when/how often it's invoked). No alternative structure (e.g. N fully
independent priming attempts each with their own charon instance) was
chosen — see research.md R3 for why that alternative was rejected.

## Complexity Tracking

*No Constitution Check violations — this section is not applicable.*

The one piece of genuine new complexity — a priming pass spanning 1..N
lines with pass-local resource indices distinct from each line's real
VoLTE index — is justified directly by FR-002 (concurrent, not sequential,
capture) and is not a new invention: it mirrors `start_vowifi_subsystem`'s
already-shipped, real-hardware-validated structure for N simultaneous
persistent lines. See research.md R3/R4.
