# Phase 0 Research: Bad-port isolation

All spec clarifications were resolved before planning (see spec.md
Clarifications, Session 2026-08-08). No `NEEDS CLARIFICATION` markers remain.
The research below records the *design* decisions the implementation depends on.

## Decision 1 — Abandon mechanism: worker thread + bounded `recv_timeout`

- **Decision**: Run each individual port open/probe on a `std::thread::spawn`ed
  worker that sends its result on an `mpsc` channel; the scan thread waits with
  `rx.recv_timeout(probe_timeout)`. On `RecvTimeoutError::Timeout`, log and move
  on, deliberately leaking the still-blocked worker.
- **Rationale**: The hang is a kernel `option`-driver block (`tty_wait_until_sent`)
  that is uninterruptible from user space — `serialport`'s read timeout and even
  `SIGTERM` do not break it (per the source triage). The only way to *not block
  the scan* is to wait on the work somewhere abandonable. This exact idiom
  (`recv_timeout` on a channel) is already used across the codebase
  (`ims/agent.rs:2244`, `observability/reporter.rs:149`), so it is the
  simplest, most consistent choice (Constitution V).
- **Alternatives considered**:
  - *`serialport` read-timeout only* — rejected: proven ineffective against this
    class of hang (the whole reason the feature exists).
  - *`async`/tokio with a cancellable task* — rejected: a cancelled async task
    whose blocking syscall never returns still leaks the OS thread from the
    blocking pool; adds a runtime dependency for no real cancellation. More
    moving parts, same leak (YAGNI).
  - *Fork a probe subprocess and kill it* — rejected: heavyweight, and a killed
    process with a wedged fd can still leave the kernel wait pending; large
    complexity for no better guarantee.

## Decision 2 — Leaked thread is accepted and bounded, not reclaimed

- **Decision**: Accept that an abandoned worker thread stays blocked for the
  process lifetime. Bound the count via per-port quarantine after 3 consecutive
  timeouts (FR-013); the persistent blocklist (FR-005) removes it entirely.
- **Rationale**: This cost already exists today (one wedged fd), only today it
  takes the whole scan down. Containing it (scan proceeds) while capping how
  many such threads a single bad port can spawn over a long-running daemon is
  the pragmatic optimum. There is no user-space way to free the wait.
- **Alternatives considered**: *Try to reclaim/kill the thread* — impossible for
  an uninterruptible kernel sleep; rejected.

## Decision 3 — Quarantine state lives in the long-lived caller

- **Decision**: The consecutive-timeout counter / quarantine set is owned by the
  module-manager rescan loop and threaded into the scan via the policy value;
  `scan_all_inner` stays stateless.
- **Rationale**: Quarantine must persist *across* rescans but *not* across
  process restart (FR-013). The rescan loop is the only object with exactly that
  lifetime. Keeping `scan_all_inner` stateless preserves its testability and
  matches how `skip_card_ids` is already passed in from the caller.
- **Alternatives considered**: *Module-level `static`/`OnceCell`* — rejected:
  hidden global state, harder to test deterministically, contradicts the
  existing "caller passes what to skip" pattern.

## Decision 4 — Blocklist matching: exact path OR topology prefix

- **Decision**: An entry matches when it equals a port's `/dev/ttyUSBn` device
  path exactly, OR equals-or-is-a-leading-path-prefix of the port's USB
  interface path (sysfs interface dir name, e.g. `5-1.2.1.2:1.1`). Unanchored
  substring matching is forbidden. (Spec FR-006.)
- **Rationale**: Topology position is stable across replug/reboot where
  `ttyUSBn` renumbers. Prefix matching gives the useful "exclude a whole unit"
  (`5-1.2.1.2`) shortcut without the over-match hazard of arbitrary `contains`.
- **Implementation note**: `candidate_tty_ports` currently returns only the
  `/dev/ttyUSBn` PathBuf. It must be extended to carry the interface path too
  (it already iterates the sysfs interface dir whose name *is* the topology
  fragment) so both matching and the FR-012 timeout log have it.
- **Alternatives considered**: *device-path-only* (renumbers, rejected as sole
  key), *unanchored substring* (over-match risk, rejected).

## Decision 5 — Config surface: new `[discovery]` section

- **Decision**: Add a `[discovery]` section via the existing `section!` macro in
  `config/raw.rs`: `excluded_ports: Vec<String>` (default `[]`) and
  `probe_timeout_ms: u64` (default `5000`, clamped up from below `1000`). Convert to a runtime `DiscoveryConfig`
  via `From<RawDiscovery>`, pre-parsing each `excluded_ports` entry into a typed
  matcher (device-path vs topology-prefix).
- **Rationale**: `section!` gives `deny_unknown_fields` + `default` + a `KEYS`
  list that `tests/test_config_docs.rs` checks against the reference/example docs
  — the established, low-surprise way to add config here. Empty default satisfies
  FR-008 (absent section == today's behavior).
- **Open (deferred to implementation, low-risk)**: whether a live edit to
  `excluded_ports` takes effect on the next rescan or requires restart. Simplest:
  read config once at startup (restart to change), matching how the rest of
  `[…]` config is consumed. Documented in quickstart as "restart to apply".

## Decision 6 — Default timeout value = 5000 ms (raised from 3000), with a floor

- **Decision**: `probe_timeout_ms` default `5000`, clamped up to a floor of
  `1000` (values below that — notably `0` — are raised, with a warning).
- **Rationale**: The clarification chose 3s against the *AT-open* budget
  (`PROBE_TIMEOUT` = 800ms). But the same budget also bounds the SIM-status
  probe, which is open + `AT+CPIN?` + `AT+CIMI` — each read can block up to the
  port read timeout, so a slow-but-healthy modem's worst case approaches 3s and
  could be falsely abandoned. 5s restores comfortable margin. A too-low value is
  clamped (not honored) because `0` makes every `recv_timeout` expire instantly
  → every port abandoned → all modems quarantined for the process lifetime, with
  no diagnosable cause.
- **Related decision**: a SIM-read-phase timeout does **not** feed the
  quarantine counter (only the AT-open probe does), so transient SIM slowness on
  a modem that already answered `AT` can never blackhole it. Implemented by
  giving `probe_sim_status_at` a plain `Duration` and no access to the policy's
  quarantine state.
