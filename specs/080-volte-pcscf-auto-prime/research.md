# Phase 0 Research: Automatic VoLTE P-CSCF Priming

## R1: How does priming obtain a candidate modem/line when `[vowifi]` is disabled?

**Decision**: Run the existing `discover` subcommand unconditionally whenever
priming is needed (regardless of `config.vowifi.enabled`), and read its
output the same way `orchestrate.rs::run` already does
(`crate::vowifi::discovery::read_line_resolution`), taking the first entry
(FR-002b).

**Rationale**: `discover` already produces a fully-formed
`vowifi::discovery::LineResolutionEntry` — modem port, netns/veth naming,
vpcd port, strongSwan `if_id`/tun-iface naming, MCC/MNC, and a complete
`VowifiConfig` — derived from `[vowifi]`'s defaults plus any
`[[vowifi.line]]` overrides, all without requiring `[vowifi].enabled = true`.
It also already excludes modems "serving the circuit-switched bridge" (the
existing PROMINENT ERROR message at `orchestrate.rs:873` documents this), so
priming cannot accidentally steal a modem the GSM daemon is actively using.
Reusing it means zero new discovery logic and zero new data shape.

**Alternatives considered**:
- *Write new discovery logic scoped to VoLTE's own line manifest
  (`volte-discover-lines`)*: rejected — that manifest describes modems for
  VoLTE's own AT/PDN access, not for standing up an IKEv2/ePDG tunnel; it
  carries none of the netns/veth/vpcd/strongSwan fields the tunnel bring-up
  needs, so this would mean duplicating most of `LineResolutionEntry`.
- *Require the operator to also configure `[[vowifi.line]]` even for a
  VoLTE-only deployment*: rejected — reintroduces exactly the manual
  configuration step this feature exists to remove.

## R2: How does priming actually establish the tunnel, without duplicating strongSwan/charon orchestration?

**Decision**: Extract the "bring up this line's netns + XFRM tun interface +
USIM bridge + swanctl connection, then wait for `Established`" sequence
already inside `orchestrate.rs::start_vowifi_line_strongswan` (roughly
`epdg_iface::ensure_epdg_interface` → `resolve_imsi`/`resolve_epdg_ip` →
`render::render_swanctl_epdg`/`render_updown_script` → spawn
`vowifi-usim-bridge` → `swanctl --initiate` → poll
`line_supervisor::tick_establishing`) into a function both the existing
persistent path and the new priming path call. The persistent path's control
flow and behavior are unchanged — same steps, same order — it just calls the
extracted function instead of running the code inline.

Priming supplies its own, request-scoped `engines::SharedCharon` (its own
`strongswan.conf`/swanctl top conf/log paths, e.g. under `/tmp/volte-prime-*`
— never the container-wide `SHARED_*` path constants) and its own private
pcscd/vpcd pair via the existing `vpcd::start_pcscd_with_retries`. Because
`[vowifi].enabled` is guaranteed false whenever priming runs (the existing
mutual-exclusion FATAL check in `orchestrate.rs` already enforces this before
`orchestrate_volte::start` is ever reached), there is no real, persistent
charon/pcscd instance for a private one to collide with.

**Rationale**: `SharedCharon::new` and `vpcd::start_pcscd_with_retries`
already take their paths/ports as plain parameters — nothing about them is
hardwired to the container-wide instance — so standing up a private,
one-shot pair costs nothing new. This is materially simpler and lower-risk
than either (a) teaching the *shared* container-wide `SharedCharon` about a
transient extra connection that must later be individually unloaded while
every real line keeps running, or (b) reimplementing IKEv2/charon
orchestration from scratch in a new code path that could silently drift from
the already-hardware-proven persistent one (e.g., missing the
`PCSCF_PLUGIN_CONF` conn-name-must-be-registered-before-charon-starts
requirement documented at `orchestrate.rs:358-368`, which a from-scratch
reimplementation would have no reason to know about).

**Alternatives considered**:
- *Reuse the container-wide `SharedCharon` and add the priming connection to
  it*: rejected — would require the shared instance to exist even in
  VoLTE-only deployments, and a mid-life connection add/remove against a
  daemon other lines might later depend on is a new lifecycle this project's
  existing single-purpose `SharedCharon` was not built for (real
  `[vowifi]` lines are all known and registered before charon ever starts).
- *A brand-new subcommand (e.g. `volte-prime-pcscf`) spawned as its own
  subprocess*: rejected — considered because it would isolate priming's
  process lifetime cleanly (process exit does most cleanup for free), but it
  would either duplicate the entire netns/veth/swanctl/USIM-bridge bring-up
  a second time or need the same in-process extraction anyway to call into
  it, without the benefit of R3's teardown reuse (a fresh subprocess has no
  access to the live `StartedState`/shutdown-plan machinery). Also would add
  a CLI surface, which FR-002a explicitly excludes.

## R3: How does priming tear the transient line back down?

**Decision**: Reuse `shutdown::build_shutdown_plan` /
`shutdown::execute_shutdown_plan` unchanged, called against a purpose-built
`StartedState` containing only the one priming line (its `vowifi_lines` entry
and the charon/usim-bridge handles in `vowifi_child_handles`) — not new
teardown code.

**Rationale**: `build_shutdown_plan` is already a pure function
(`&StartedState -> Vec<TeardownStep>`) that knows exactly how to tear down
one `StartedVowifiLine` correctly and in the right order (terminate the
IKE_SA, kill its child processes, delete its XFRM tun interface — the only
thing that actually releases a strongSwan `if_id`, per its own doc comment —
delete its veth, delete its netns). It was written to be driven by whatever
`StartedState` a run actually produced, with no assumption that the state
describes the *whole* container. Scoping it to a `StartedState` with exactly
one line is a legitimate, already-supported use, not a workaround — no new
teardown logic needs to be invented or separately verified.

**Rationale for a bounded establish timeout (new behavior vs. the persistent
path)**: The persistent path's establish loop is intentionally unbounded
(`EstablishOutcome::FatalTimedOut => unreachable!()` for strongSwan) because
a real, permanently-configured line should keep trying indefinitely. Priming
is a one-shot action inside a startup sequence an operator is watching (FR-007),
so it needs an actual deadline — addressing the spec's "capture takes an
unusually long time" edge case — after which it reports failure and lets the
existing retry cadence (R4) try again from scratch on the next cycle, rather
than blocking VoLTE startup indefinitely on a single stuck attempt.

**Alternatives considered**:
- *Write a new, priming-specific teardown function*: rejected — this is
  exactly the kind of second, divergent teardown implementation
  Simplicity/Refactorability warns against, and it would need its own
  correctness proof (ordering, XFRM `if_id` release, budget/abandonment
  under a stuck delete) that `build_shutdown_plan`/`execute_shutdown_plan`
  already have.

## R4: Where does the "is priming needed" check live, and how does retry-on-failure work?

**Decision**: A pure helper in `volte::pcscf` —
`pcscf_is_available(cache_path: &Path, override_addr: Option<&str>) -> bool`
— checks the override first, then `probe_epdg_cache(cache_path).found()`
(FR-001/FR-005). `orchestrate_volte::start_legacy_registration`'s existing
per-attempt retry loop (its `std::thread::spawn(move || loop { ... })`,
15s sleep on failure) calls this check, and `orchestrate_prime::prime_pcscf`
when it's false, before spawning `volte-register` — reusing the loop's
existing retry/backoff for free (FR-008). `start_multiline` has no
equivalent enclosing loop at the right scope (its per-line agent loops start
after discovery, and priming only needs to happen once, not per line), so it
gets a small local loop at the same 15s cadence, placed once, before any
per-line spawn.

**Rationale**: This is the smallest change that satisfies FR-008 exactly as
clarified — "the same restart-loop cadence already used elsewhere" — without
introducing a second retry/backoff concept.

**Alternatives considered**:
- *A dedicated priming retry loop with its own cadence/backoff*: rejected by
  the clarification itself (Q1, session 2026-09-17) — the spec explicitly
  asked for reuse of the existing cadence, not a new one.

## R5: Multi-line VoLTE deployments and the shared P-CSCF cache

**Decision**: No change needed beyond R1's "always line 0." Confirmed by
reading `src/commands/volte.rs::volte_bridge_manifest_lines` — every
auto-discovered VoLTE line already resolves its P-CSCF from the *same*
single `[volte].pcscf_source_path` today, regardless of which modem line it
is. Priming populating that one shared file is therefore sufficient for
every VoLTE line, multi-line or not; this was also the substance of the
spec's clarification session (Q2).

**Alternatives considered**: None — this was a factual question about
existing behavior, not a design choice; see spec.md's Clarifications.
