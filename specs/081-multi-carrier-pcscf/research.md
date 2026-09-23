# Phase 0 Research: Multi-Carrier VoLTE P-CSCF Priming

## R1: Per-line cache key — `card_id`, not index

**Decision**: The per-line P-CSCF cache is keyed by the line's `card_id`
(`<[volte].pcscf_source_path>-<card_id>`, e.g. `/tmp/pcscf-0-ec20-ABCDEF`),
via a new pure function `volte::pcscf::per_line_cache_path`.

**Rationale**: `volte::discovery::resolve_volte_lines` orders candidates by
`card_id` before assigning `index` ("stable across USB enumeration
jitter" — its own doc comment) and caps/derives every per-line resource
from that `index`. `card_id` itself is stable per modem
(`modules::discovery::derive_module_id` — `ec20-<last 6 alphanumeric chars
of the USB identifier>`), but a given modem's *index* is only stable while
the discovered modem *set* is unchanged: adding or removing a modem
elsewhere in the fleet re-sorts and re-assigns every index. Keying the
cache by `index` (as VoWiFi's own persistent per-line cache already does,
`<vowifi.pcscf_source_path>-<index>`) would silently rebind an existing,
correctly-captured address to the wrong line the moment the fleet's modem
set changes — exactly the failure the spec's Edge Cases and FR-003 rule
out. `card_id` is already alphanumeric-only (`derive_module_id`), so it is
filename-safe with no escaping.

**Alternatives considered**:
- *Keep index-keying* (matching VoWiFi's existing convention): rejected —
  directly violates FR-003/the Clarifications answer; would work only
  until the first fleet topology change.
- *A new opaque UUID per line, persisted separately*: rejected — adds a
  second piece of state (the UUID-to-card_id mapping) that itself needs to
  survive restarts and redeploys, solving nothing `card_id` doesn't already
  solve for free, violating Simplicity.

## R2: Three-tier resolution, not two

**Decision**: `resolve_line_pcscf` (`commands/volte.rs`) gains a middle
tier: (1) explicit `[[volte.line]].pcscf` override — unchanged, highest
precedence — (2) **new**: this line's own `card_id`-keyed cache
(`per_line_cache_path`) — (3) the existing literal
`[volte].pcscf_source_path` file, unchanged, now the fallback.

**Rationale**: Tier (3) is what every single-line and already-pinned
deployment already reads today. Preserving it unchanged, as the lowest-
priority fallback, is what makes User Story 3 / SC-003 ("do no harm") true
for free — an operator who already has a working `/tmp/pcscf-0` (single
carrier, no multi-line concern) sees tier (2) simply miss (no per-`card_id`
file exists yet) and fall through to the exact behavior they have today.
Only a deployment that actually goes through the new priming pass (multi-
line, mixed-carrier) ever populates tier (2).

**Alternatives considered**:
- *Replace the shared file outright with per-line files, no fallback*:
  rejected — breaks every existing single-line deployment's config on
  upgrade (violates SC-003) for a benefit (multi-carrier correctness) those
  deployments don't need.
- *Two tiers only (override, per-line cache), drop the legacy shared
  file entirely*: rejected for the same reason — an operator who
  previously pointed `[volte].pcscf_source_path` at a hand-picked VoWiFi
  capture file loses that pin silently.

## R3: Concurrent priming reuses `start_vowifi_subsystem`'s shared-charon pattern

**Decision**: A priming pass for N lines needing capture builds **one**
shared charon instance (`SHARED_STRONGSWAN_CONF`/`SHARED_SWANCTL_CONF`/
`SHARED_VICI_SOCKET`/`SHARED_CHARON_LOG`, unchanged constants) listing
every priming line's connection name up front — exactly
`start_vowifi_subsystem`'s existing `conn_names`/`render_pcscf_plugin_conf`
step, just scoped to the priming-line subset instead of every real,
persistent VoWiFi line — then runs each line's
`prepare_vowifi_line`/`establish_line_tunnel` on its own thread, same as
that subsystem's own per-line loop.

**Rationale**: This is not new territory — it is the same mechanism the
real, persistent multi-line VoWiFi subsystem already uses for N
*simultaneous* real lines in production, just torn all the way down
afterward instead of kept running. Reusing it directly means concurrent
priming inherits that subsystem's existing hardware validation for the
"many lines, one shared charon" half of the problem; the only genuinely
new risk is the same-process capture-then-handoff-to-VoLTE that 080
already flagged as unvalidated for one line, now happening for several in
the same pass (still called out in quickstart.md).

**Alternatives considered**:
- *N fully independent `prime_pcscf` calls, each building its own shared
  charon* (today's 080 shape, run N times): rejected — the `SHARED_*`
  constants are single, fixed filesystem paths; two concurrent calls would
  overwrite each other's `strongswan.conf`/`swanctl.conf`/VICI socket.
  Making them per-attempt-unique would mean running N separate charon
  daemons, which the real persistent subsystem deliberately does not do
  and which has no existing hardware validation at all.
- *Sequential priming* (one line fully primed, torn down, before the next
  starts): rejected by the Clarifications answer (Q2) — contradicts the
  per-line independence FR-005/FR-009 already establish, and needlessly
  slows startup for fleets with several lines needing capture.

## R4: `resolve_single_line` needs a pass-local index, not a hardcoded 0

**Decision**: Generalize `vowifi::discovery::resolve_single_line(modem,
base)` (today: `resolve_one_line(0, modem, base)`, hardcoded) to accept an
explicit index, assigned densely (0..N) across only the lines in the
current priming pass — a **pass-local** index, unrelated to any line's
real VoLTE `index` or its `card_id`-keyed cache filename.

**Rationale**: Every per-line resource `resolve_one_line`/
`derive_line_resources` derives — netns, `strongswan_tun_iface`,
`strongswan_if_id`, veth addresses, `vpcd_port` — is a pure function of
that hardcoded index. Priming two lines "at index 0" simultaneously would
collide on all of them (same namespace name, same tunnel interface, same
XFRM if_id, same veth pair, same vpcd port) — safe today only because
exactly one priming attempt ever runs at a time. A pass-local index (reset
to 0 for each new pass, independent of VoLTE's own line numbering) is
sufficient: these resources only need to be collision-free *within one
pass*, and are fully torn down before VoLTE's own per-line resources
(different netns/veth namespace, `020-volte-line-netns`) ever touch the
same modem.

**Alternatives considered**:
- *Reuse each line's real VoLTE `index` as the pass-local index*: rejected
  — simpler at first glance, but VoLTE's own `index` is only assigned to
  lines that survived `resolve_volte_lines`' `max_lines` cap and ordering;
  reusing it for VoWiFi-shaped priming resources risks accidentally
  colliding with a persistent `[vowifi]` line's own resources on a system
  that runs both subsystems at different times but shares a filesystem
  (not a live conflict — mutual exclusion still holds — but an
  unnecessary coupling between two independently-evolving index spaces
  the codebase has deliberately kept separate, per `020-volte-line-netns`
  FR-004a). A fresh, pass-local 0..N is simpler to reason about and
  costs nothing extra.

## R5: Per-line gating reuses the existing per-line supervision thread

**Decision**: `start_multiline`'s existing per-manifest-line
`std::thread::spawn` loop (already one thread per line, today used only
to supervise `volte-carrier-agent` restarts) gains one step at its top,
before its first spawn attempt: resolve this line's own address via the
new three-tier check (R2); if unavailable, wait (same 15s cadence already
used there) rather than spawning.

**Rationale**: This is the per-line, non-blocking independence FR-005/
FR-009 require, and it costs nothing new — the thread-per-line structure
already exists for an unrelated reason (supervising the carrier-agent
child process) and is already exactly what "this line's own progress is
independent of every other line's" means in this codebase's existing
idiom (see the identical pattern in `start_vowifi_subsystem`'s per-line
loop). The priming *pass* itself (R3) still needs to run and complete
enough lines for this per-line check to eventually see a populated cache,
but once a specific line's cache exists, that line's own thread proceeds
immediately — it does not wait for the whole pass, or for any other
line's thread.

**Alternatives considered**:
- *A new per-line channel/future signaled by the priming pass on each
  line's success*: rejected — adds a synchronization primitive for
  something a bounded-retry poll (the existing 15s-cadence pattern, used
  everywhere else in this file for exactly this kind of wait) already
  does with less code and one fewer failure mode (a signal that's never
  sent because the pass itself panics or is skipped).

## R6: `tear_down` generalizes from one line's resources to a pass's

**Decision**: `orchestrate_prime::tear_down`'s scoped `StartedState`
(today: `pcscd`, `vowifi_child_handles`, `started_netns`, `vowifi_lines` —
the four fields one priming attempt ever touches) keeps the same four
fields, now populated by every line in the pass instead of just one. One
shared `pcscd` handle for the whole pass (matching `start_vowifi_subsystem`:
one shared pcscd, N per-line vpcd slots via each line's own `vpcd_port`),
`started_netns`/`vowifi_lines` accumulate via the same `Vec::push` every
per-line loop already uses for real, persistent lines.

**Rationale**: No new data structure — `StartedState`'s existing fields
are already `Vec`-shaped for the real multi-line case; a priming pass is,
structurally, just a smaller and transient version of the same shape.

**Alternatives considered**:
- *One `tear_down` call per line, each with its own scoped state*:
  rejected — reintroduces the one-shared-pcscd-vs-N-pcscd question R3
  already resolved against, and would call `build_shutdown_plan` N times
  against overlapping shared resources (the one charon instance) with no
  clear ownership of who tears the shared piece down.

## R7: Deciding whether a pass is needed at all stays cheap

**Decision**: Before running any priming pass, `start_multiline` checks
every manifest line's own three-tier availability (R2) first. If every
line already has a usable address, no pass runs — zero `discover`/charon/
pcscd activity, matching 080's existing SC-003 guarantee, now evaluated
per line instead of once globally.

**Rationale**: This is the same "check before acting" gate 080 already
established (`pcscf_is_available`), just called once per manifest line
instead of once for the whole deployment — the check itself
(`std::fs::read_to_string` + IP parse) is cheap enough that doing it N
times costs nothing observable.
