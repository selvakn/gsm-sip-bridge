# Research: Bounded tunnel initiation (#139)

## R1. What actually blocks charon, and what does `--timeout` do?

**Decision**: pass `--timeout 20` on every `swanctl --initiate`.

**Evidence** (pinned fork `022f3e5`, `osmo-epdg-0.1.2-4`; reproduced 2026-10-21):
- `vici_control.c` `initiate()` reads `timeout` (default 0) and calls `controller->initiate(..., timeout, ...)`. With a log callback it blocks in `wait_for_listener(job, timeout)`; `timeout=0` waits forever, a positive value returns `OUT_OF_RES` ("not established after Nms") and releases the worker.
- The listener only completes on `IKE_ESTABLISHED` or `IKE_DESTROYING`. With `keyingtries = 0`, "peer not responding, trying again" calls `reset()` on the same IKE_SA (`ike_sa.c:2481`), so it is never destroyed. Each no-timeout initiate therefore holds a worker for as long as the ePDG is silent.
- `DEFAULT_THREADS` is 16 (`daemon.h:206`); we never set `charon.threads`.
- Rig: no timeout, 5s cadence: log frozen after the 12th initiate, `--list-sas` timing out, clients 22, still frozen after all clients killed. With `--timeout 20`: clients flat at 3, log advancing, `--list-sas` answering for 22 ticks.

**Alternatives**: `--timeout -1` (async, no callback; code-read only, unverified on this build); raising `charon.threads` (delays the freeze, hides the growth, rejected).

## R2. One client per line, and no waiter threads

**Decision**: the engine holds one tracked `ChildHandle`; a new initiate is skipped while it is alive.

**Rationale**: `spawn_detached` returns nothing and runs a waiter thread per call, so "is one pending" is unanswerable and shutdown cannot reach it. A tracked handle answers both, and `wait` on a finished handle reaps it and returns the exit status for logging. The earlier review finding about leaking table entries (see `swanctl_background`'s comment) was about *discarding* the handle; this keeps and consumes it.

**Lifetime arithmetic**: `--timeout 20` ends the client at 20s. The outer `timeout -k 2 25` is only for an unresponsive VICI and ends it by 27s at the latest. The cadence is `STRONGSWAN_REINITIATE_EVERY` (15) x `ESTABLISH_POLL_INTERVAL` (2s) = 30s, so a cadence-driven initiate normally finds no live client. Other callers (steady-state recoveries) can still arrive while one is live and are skipped, which is the intent: the live one is already doing the same job.

**Alternatives**: flag set by the waiter thread (keeps the waiter, adds shared state); pid scan via `pgrep` (races, not line-scoped).

## R3. Recovery for a live but wedged charon does not exist today

**Finding**: `StrongswanEngine::restart_process` calls `SharedCharon::restart_if_dead`, which deliberately returns false while the process is alive (so one line's trouble cannot take down the others). It then calls `load_all` (`swanctl --load-all`, run through `runner.run` with **no deadline**) before `terminate`/`reinitiate`. Against a charon whose workers are all blocked, steady-state `ViciBroken` therefore (a) does not restart the daemon and (b) can block that line's supervisor thread inside `load_all`. The spec's FR-007/FR-008 cannot be met by reusing it.

**Decision**: add `SharedCharon::restart_if_wedged(observed_generation)`:
- Takes the handle lock and compares the log generation the caller observed the failure under; if another line already restarted the daemon since, it does nothing (no killing of the replacement).
- Otherwise reaps the live charon and respawns it exactly as `spawn_locked` does, then every line's `load_all` + initiate follows through its own normal path (they find no IKE_SA and re-initiate).
- Put a `timeout` around `load_all`.
- Used for *control interface unresponsive* in both phases. Other lines pay one tunnel re-establishment; that is the price of a wedged shared daemon and is only reached after two failed probes. With R1 in place it should not be reachable by our own requests.

**Alternatives**: restart only after N minutes (rejected: the freeze is permanent, waiting buys nothing); per-line charon (rejected by earlier work: wildcard-bound UDP 500/4500, see `SharedCharon` docs).

## R4. The startup loop's re-initiate rule

**Decision**: reuse steady state's rule (specs/100 US3) instead of inventing one: on a re-initiate tick, `list_sas`; no IKE_SA -> initiate; `CONNECTING` with activity inside `HALF_OPEN_STUCK_AFTER` (200s) -> leave to charon; `CONNECTING` and idle past it -> terminate + initiate; established with no P-CSCF -> existing terminate-then-initiate.

**Rationale**: with `keyingtries = 0`, charon keeps one half-open IKE_SA for the whole outage and retries on its own backoff (`retry_initiate_interval = 10`, retransmit backoff up to ~76s). Stacking initiates on it adds nothing.

**Note**: the existing 30s cadence stays; only what it does at each tick changes. The probe runs only on cadence ticks (every 30s), not every 2s poll.

## R5. Interaction with the underlay guard (specs/100)

The establish loop does not consult the underlay verdict today; steady state does. That is unchanged here. During a no-route hold the establish-phase initiate is a bounded 20s client against a charon that cannot send, which is harmless. Adding the guard to establishment is a separate change.

## R6. Probe thresholds

- Probe deadline: reuse the existing `STEADY_LIST_SAS_TIMEOUT_SECS` (10s) so one deadline governs both phases.
- Two consecutive failures (about 60s at the 30s cadence) before declaring *unresponsive*; one timeout on a busy daemon is not a wedge.

## R7. Sizing note (T033)

With N silent lines at most N workers are held, each for at most 20s, because each line has at most one live client. charon's pool is 16; reserving 4 for the daemon's own work leaves headroom for the supported maximum of 4 lines per shared charon (`SUPPORTED_MAX_LINES`, asserted at compile time against `16 - 4`). The VICI wedge path (R3) covers anything else that could still exhaust the pool.
