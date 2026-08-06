# Phase 0 Research: Discovery Retry & Missing-Line Health Reporting

No `NEEDS CLARIFICATION` markers remained in the Technical Context after reading the existing code — every unknown resolved to "reuse the existing pattern," documented below as a decision with rationale and the alternative considered.

## R1: Where does a configured-but-undiscovered line disappear today?

**Decision**: The gap is between `modules::discovery::scan_all_preferring` (raw USB scan) and `vowifi::discovery::resolve_lines` (candidate → line resolution). `RoleAssignment::from_probed` only ever sees modems that `scan_all_preferring` actually found in `/sys/bus/usb/devices` *at the moment it ran*; a `[[vowifi.line]]` `modem_port`/`modem_serial` override with no matching entry in that scan is never even a candidate — `resolve_lines`'s existing `failed: Vec<FailedLine>` only records candidates that *were* probed and then rejected (SIM not usable, `MaxLinesExceeded`). There is currently no code path that compares "what's configured" against "what showed up" and flags the difference.

**Rationale**: Confirmed by reading `gsm-sip-bridge/src/modules/discovery.rs` (`scan_all_inner`, lines ~146-246) and `gsm-sip-bridge/src/vowifi/discovery.rs` (`RoleAssignment::from_probed`, `resolve_lines`) directly, and by reproducing the real incident: `supervise`'s startup log showed `LINE_COUNT=1` with zero mention of the EC20's `card_id` (no "discovered modem", no "SIM not usable", no "no AT-capable interface" — all of which `scan_all_inner` logs for anything it *does* enumerate), while a manual `discover` run minutes later found it cleanly. The device was present but not yet enumerated in `/sys/bus/usb/devices` at scan time.

**Alternatives considered**: Treating this as a `modules::discovery` bug to fix by waiting longer before the *first* scan — rejected because there is no fixed "long enough" (USB/modem boot time varies), and it would delay every startup by that worst case even when nothing is slow. A bounded *retry* after startup, rather than a longer single wait, only costs time on the runs that actually need it (FR-005/SC-005).

## R2: How should "not found at all" be represented as a failure reason?

**Decision**: Extend the existing `Rejection`/`FailedLine::reason()` vocabulary (`gsm-sip-bridge/src/line/mod.rs`'s `Rejection` enum, used today for `SIM not usable` classification and for `Rejection::MaxLinesExceeded` in `resolve_lines`) with a new reason for "this override matched no probed modem." `LineTableResult`/`LineResolution`'s existing `failed: Vec<FailedLine>` shape doesn't need a new type, just a new populated reason — `resolve_lines` (or a caller wrapping it) needs to walk `base.line_overrides` and `base.pcsc` overrides, not just `assignment.vowifi`, to notice one with no corresponding entry at all.

**Rationale**: `FailedLine`/`LineResolution` are already read by `vowifi-status` (indirectly, once extended per R4) and already serialized to the on-disk resolution file every other tool reads — reusing the shape means no new file format or IPC surface, just a new variant flowing through machinery that already exists.

**Alternatives considered**: A wholly separate "missing lines" list/file — rejected as needless duplication of a mechanism (`failed`) that already exists for exactly this purpose (a configured/candidate line that didn't make it), and would require every consumer (`vowifi-status`, `healthcheck`) to learn a second source of truth instead of one.

## R3: Where does the bounded retry loop live, and how does it avoid delaying lines that already worked?

**Decision**: In `supervise::orchestrate`, immediately after the existing single `discover` invocation (`orchestrate.rs` line ~194) — but *not* blocking on it. Sequence:

1. The first `discover` pass runs exactly as today, synchronously, and its results start the circuit-switched daemon and every successfully-resolved VoWiFi line's supervision *immediately* (sections 2/3 of `orchestrate.rs`, unchanged timing — this is what makes SC-005 hold).
2. If that pass left any configured override (`modem_port`/`modem_serial`/`pcsc_reader`) unresolved (per R1/R2's `NotFound` detection), a **background thread** is spawned to retry just those overrides on an interval, bounded by the retry window.
3. Each retry re-probes *only* what's needed to re-check the missing overrides — reusing `modules::discovery::scan_all_inner`'s existing `skip_card_ids` exclusion (today only ever populated by `scan_modules`'s ongoing rescans) so the retry loop never reopens or re-probes a serial port an already-resolved line's agent has open, which is exactly the "modem claimed by both subsystems" hazard that parameter exists to prevent (FR-005).
4. If a retry succeeds for an override, that specific line is started the same way an initially-resolved line is — via a per-line startup path extracted out of section 3's existing initial loop so it's callable a second time, later, without restarting anything else (FR-004) — and the resolution file is updated so `vowifi-status`/`healthcheck`/future process restarts see it as resolved.
5. If the window elapses first, a terminal `FailedLine{reason: "not_found", ..}` is written to the resolution file (R2) and the metric/alert (R5) fire; no further retries happen for that override this run (startup-only scope, per the spec's clarification).

**Rationale**: Keeps `orchestrate.rs` as the single place that owns both `discover` and startup sequencing, without making already-successful lines (or the circuit-switched daemon) wait on a line that isn't ready — matching how the daemon-supervisor and per-line tunnel-establishment loops elsewhere in this same file already run as independent background threads/loops rather than serializing startup.

**Alternatives considered**: Retrying inside `discover` itself, or blocking the rest of `orchestrate`'s startup sequence on the retry loop before proceeding to sections 2/3 — both rejected because they'd delay the circuit-switched daemon and every already-successful line for the full retry window whenever *any* configured line is slow, violating FR-005/SC-005. Retrying from each per-line supervisor loop (`orchestrate.rs`'s per-line `waiting for tunnel` loops, ~line 889/1219) — rejected because those loops only start *after* a line exists in the resolution; a line that was never discovered has no per-line loop to retry from in the first place.

## R4: How do `vowifi-status` and `healthcheck` need to change?

**Decision**: Both currently only ever look at successfully `resolve_lines`d entries.
- `vowifi::print_status` (`gsm-sip-bridge/src/vowifi/mod.rs:1826`) iterates `resolve_runtime_lines(config)` — needs to also read the resolution file's `failed` list and print each configured-but-not-running line (FR-006/FR-007).
- `healthcheck::evaluate` (`gsm-sip-bridge/src/commands/healthcheck.rs:166-200`) explicitly treats an empty `resolution.lines` as healthy (`if !vowifi_enabled || resolution.lines.is_empty() { return Health::Healthy/CircuitSwitchedDisabled }`) and never inspects `resolution.failed` at all, even when some lines *did* resolve — this is the exact blind spot that let the incident's container report `healthy` in `docker ps`. Needs a new `Health` variant (or an addition to the existing `LinesUnhealthy` fault list) for "a configured line failed to start," populated from `resolution.failed`'s new `NotFound` entries, filtered to only the *terminal* (retry window elapsed) ones — a line still within its retry window should not yet flip the container unhealthy (that would make ordinary, expected startup churn look like an outage).

**Rationale**: Both are read-only consumers of the same resolution file `discover`/the retry loop already write to (R3); no new IPC.

**Alternatives considered**: A separate "line intent vs. reality" endpoint or command — rejected per Constitution Principle V (simplicity); extending two already-existing, already-read call sites is the smaller change.

## R5: How does alerting/metrics plug in, given no agent process exists for a never-discovered line?

**Decision**: The existing alert/metric pipeline (`metrics::ingest::apply_report`, the `AlertPhase`/`CategoryAlertConfig` machinery in `metrics/ingest.rs` and `alerts/mod.rs`) is driven entirely by `AgentReport`s a *running* `vowifi-ims-agent`/`vowifi-sip-agent` process sends over the observability protocol. A line that was never discovered has no such process and therefore can never produce a report — "reuse the existing alerting mechanism" (FR-009) means reusing the *pattern* (a new `AlertCategory`, its own `CategoryAlertConfig` in `AlertsConfig`, a `Failure`/`Recovered` pair dispatched via the same `alerts::discord::DiscordClient`), not literally routing through `ingest::apply_report`. The trigger instead comes directly from `supervise::orchestrate`'s retry loop (R3): it dispatches the `Failure` event itself when the retry window elapses for a configured line, and the `Recovered` event if/when that line later resolves within the same process lifetime. The new metric (a `GaugeVec` alongside `VOWIFI_REGISTERED`/`VOWIFI_TUNNEL_UP` in `metrics/mod.rs`) is set directly by the retry loop too, for the same reason — there's no agent to report it via `AgentReport`.

**Rationale**: Keeps the failure/recovery *shape* identical to `registration_loss`/`tunnel_failure` (same config surface, same Discord message pairing an operator already recognizes) without forcing a fundamentally report-driven design onto a condition that, by definition, has no reporter.

**Alternatives considered**: Spinning up a placeholder/stub agent process for a not-yet-discovered line just so it can emit `AgentReport`s into the existing pipeline unmodified — rejected as substantially more machinery (a process with nothing real to do) for the sole purpose of avoiding one new `AlertCategory` variant and one new call site in `orchestrate.rs`; violates Constitution Principle V.

## R6: Testing approach for a "slow-enumerating modem" without real, flaky USB hardware

**Decision**: `resolve_lines`/`RoleAssignment::from_probed` already take `&[ProbedModem]` as plain data (not a live scan) — `test_discovery.rs`'s existing tests already construct `ProbedModem` fixtures directly. The retry behavior can be tested the same way: call the resolution/retry logic first with a fixture list missing the configured modem, then again with it present, and assert the line resolves on the second call without disturbing a line resolved on the first — exercising the real production code path end-to-end (per the Integration-First Testing principle) without needing real, timing-dependent USB hardware. `test_ingest_critical_alerts.rs`'s existing pattern (constructing threshold-crossing sequences and asserting `Failure`/`Recovered` dispatch) is the direct analog for the new alert category's tests.

**Rationale**: Matches how `resolve_lines_*` tests already work in `test_discovery.rs`; no mocking of a boundary that's impractical to run for real is introduced — the actual USB scan function is the only piece deliberately out of scope for automated tests (as it already is today), consistent with the Constitution's "mocks only for what's impractical to run locally" carve-out.

**Alternatives considered**: None seriously — this follows the existing test file's established pattern directly.
