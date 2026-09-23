# Contract: Multi-carrier VoLTE P-CSCF priming during `supervise` startup

Extends specs/080-volte-pcscf-auto-prime's contract to a `[volte].enabled +
bridge_inbound = true` deployment with **more than one** discovered line.
Externally observable interfaces: `supervise`'s startup output (per FR-006),
the per-line cache files at `<[volte].pcscf_source_path>-<card_id>`, the
existing (legacy) `[volte].pcscf_source_path` file, and whether/when each
line's VoLTE registration proceeds. No new CLI subcommand, no new config
field. Each row is independently verifiable by starting the bridge with the
described multi-line state and observing supervise's own output and the
cache files.

## Startup behavior, multiple discovered VoLTE lines

| Per-line state at startup | Observable outcome |
|---|---|
| Every line has an explicit override or a valid own-line/legacy cache | No priming pass runs at all; every line registers immediately, identical to today for a single-carrier fleet (FR-007, SC-003) |
| Some lines have a usable address, others don't | Only the lines that need it are primed; lines that already have an address register immediately without waiting for the others (FR-007, SC-003, User Story 3) |
| No line has a usable address, all different carriers | One priming pass primes every line concurrently; each line's own cache file (`<base>-<card_id>`) ends up containing that line's own carrier's address, never another line's (FR-002, FR-003, FR-004, User Story 1, SC-001, SC-005) |
| No line has a usable address, two lines share one carrier | Both lines are primed independently (no shared-capture optimization); both end up with a correct, usable address for their shared carrier, which may coincide (Edge Cases) |

## Per-line failure isolation (Clarifications Q2, FR-005)

| Priming-pass outcome | Observable outcome |
|---|---|
| Line A succeeds, Line B fails (no signal, tunnel failure) in the same pass | Line A proceeds to VoLTE registration immediately; supervise's output identifies Line B's failure distinctly, attributed to that specific line (FR-006, SC-004); Line B retries on the existing 15s cadence without delaying Line A or any other already-registered line (FR-009) |
| Every line in the pass fails | Every line retries independently on its own cadence; a line whose retry later succeeds registers immediately without waiting for the others |

## Cache-file identity (FR-003, research.md R1)

| Scenario | Observable outcome |
|---|---|
| A modem is added to the fleet, shifting other lines' discovery-order index | Every existing line's already-captured address remains valid and in use — cache lookup is by `card_id`, unaffected by index reassignment |
| A modem is removed from the fleet | The remaining lines' cache files (keyed by their own `card_id`, untouched) continue to resolve correctly; the removed line's now-orphaned cache file is inert (not read, not cleaned up — out of scope) |

## Non-effects (explicitly unchanged by this feature)

| Component | Behavior |
|---|---|
| Single-line VoLTE deployments | Unchanged — one line still resolves via the same three tiers, and with only one line the new middle tier (own-`card_id` cache) and the legacy shared-file tier converge on the same practical outcome the operator already has today |
| specs/080's single-line priming contract | Fully subsumed — a one-line "pass" behaves identically to specs/080's existing behavior |
| Persistent `[vowifi]` subsystem | Unchanged — a priming pass never runs while `[vowifi].enabled` is persistently true, same mutual-exclusion guarantee as specs/080 |
| Standalone `volte-register` / `volte-listen` / `volte-call` / `volte-bridge` (single-`--modem` diagnostic path) | Unchanged — no priming pass; same explicit-override-or-manual-dance behavior as today |
| Circuit-switched GSM-to-SIP daemon | Unchanged — never delayed or disrupted by any line's priming activity or retry (FR-009, carried from specs/080 FR-010) |
| SIM/carrier-swap detection | Out of scope (FR-008) — a stale per-line cache is trusted until missing or invalid, same limitation as specs/080, now per line |
