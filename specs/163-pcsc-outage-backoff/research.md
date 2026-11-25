# Research: The Supervisor Stops Retrying in a Tight Loop While a Card-Reader SIM Is Out of Reach

Paths are relative to `gsm-sip-bridge/src`. Line numbers are as of `c249cb1` (main after #235).

## Facts

- **F1. The agent restart loop is one thread per line.** It is in `supervise/orchestrate.rs::start_line_tail` (about `:3175`–`:3362`). It spawns `vowifi-ims-agent` with stdout captured to `/tmp/ims-agent-N.out`, polls `runner.is_alive`, then classifies the exit from that log (`:3303`):
  - `sim_recovery::has_at_stall`;
  - `sim_recovery::has_csim_failure`;
  - otherwise `Other`.

  It calls `IncidentCounters::observe` and `restart_delay`, and waits with one `runner.sleep(restart_delay)` (`:3361`). That sleep does not look at `shutting_down`; the loop checks it only at the top.
- **F2. Two agent-side precedents for the supervisor to classify.**
  - `watchdog::STALL_MARKER` (`ims/agent/watchdog.rs:73`) is logged with `tracing::error!` before a stall exit, and `sim_recovery::has_at_stall_matches_the_marker_the_watchdog_emits` pins the two sides together.
  - `EXIT_WATCHDOG_STALL = 70` exists for humans; the supervisor classifies on the marker.
- **F3. The agent's start-up failure reaches `ims::agent::run` with its type intact, and is then printed to stderr** (`eprintln!("error: {e}")`, `ims/agent/mod.rs:229`). The supervisor captures stdout only, so the marker must be a tracing line, as `STALL_MARKER` is. Both start-up card paths keep the type:
  - the PLMN read (`PcscTransport::connect(&imsi)?`, `:296`);
  - `register_via_pool` → `register_checked` → `register_session` → `open_card` (`RegisterFailure::from` keeps the error).

  #235 typed every unreachable condition as `BridgeError::CardUnreachable`, which only the PC/SC transport builds, so modem lines can never produce it.
- **F4. The tunnel decisions are a pure function of their inputs.** `line_supervisor::tick_underlay` (`:1314`) takes an `UnderlayView` built by the loop before the tick, plus closures (`call_active`, `agent_pool`). Recoveries that are inferred from reachability first ask `underlay_gate` (`:1735`), which is called at `:1510` (ChildSaStuck), `:1626` (PcscfPoolExhausted), `:1694` (PcscfUnreachable), `:1832` (half-open stuck) and `:1927` (ChildSaRekeyStuck). `IkeSaMissing` has its own held branch (`:1449`), which allows exactly one initiation per hold and sets `tracking.agent_restart_owed`. `TunVanished` (`:1395`) is structural: it is never held, and it terminates and re-initiates.
- **F5. `recovery_renegotiates(reason)` (`line_supervisor.rs:1986`) already names the recoveries that start a new IKE negotiation.** It is true for everything except `ChildSaMissing`, `TunUnavailable` and `DefaultRouteMissing`. A new IKE_SA needs EAP-AKA, which needs the SIM; a CHILD_SA re-initiate inside a live IKE_SA does not.
- **F6. The tick sits between two daemon-wide recoveries.** `ProcessDied` restarts the shared charon daemon, and `ViciBroken` restarts or recovers it. Both affect every line, including modem lines.
- **F7. The underlay hold's surface is the uplink.**
  - `UplinkHoldCause` (`NoRoute`/`NoResponse`) goes into `SteadyOutcome::Held`, `UnderlayVerdict`, `AgentState.uplink` (`control/protocol.rs:506`), the `uplink` status file (`write_uplink_file`, read by `vowifi-status`), the `VOWIFI_UPLINK_HELD` gauge (`metrics/ingest.rs:342`), and the tunnel-failure text (`tunnel_failure_description`, `metrics/ingest.rs:902`).
  - `UnderlayTracker::after` matches on `SteadyOutcome` (`underlay.rs:466`).
- **F8. The tunnel-failure alert text is built in one function.** `metrics/ingest.rs::tunnel_failure_description(subject, unhealthy_sec, flapping, cause, uplink)` is called from `alert_description` (`:1144`), which reads the supervisor's record (`last_uplink`). The supervisor's report sets `signal.uplink` (`orchestrate.rs:2130`, `:2260`), and `AgentState` fields are `#[serde(default, skip_serializing_if = "Option::is_none")]`, so a new optional field is compatible both ways.
- **F9. The supervisor already reads a card by IMSI as a subprocess.** `vowifi-plmn --pcsc-imsi <IMSI>` runs through `runner.run` (`orchestrate.rs:1077`, `read_plmn_once` `:1130`), retried 15 times 5 s apart at start-up. `vowifi::plmn::run` (`vowifi/plmn.rs:117`) connects by IMSI, reads EF_IMSI/EF_AD in one transaction and exits `SUCCESS` or `FAILURE`. Other supervisor commands are bounded with `timeout N` in argv (`epdg_iface.rs:310`, `shutdown.rs:630`). The container runs with `init: true`, so `timeout` children are reaped.
- **F10. The steady-state loop is one per line** (`start_vowifi_line_strongswan`, `orchestrate.rs:1965`), on its own thread. It shares a per-line `Arc<RebuildGate>` with that line's agent thread (`:2058`). It ticks every 30 s, and every 5 s around an underlay hold (`HELD_POLL_INTERVAL`).
- **F11. Learnings Part 1 classes that apply:**
  - process lifecycle: the restart path, and shutdown during a long wait;
  - recovery loops: every state has a way out, and nothing is retried forever without pacing;
  - latching state: the outage must end on every path out;
  - observability: the alert text, and a protocol field round-tripped through every serializer;
  - shared devices: card checks must leave the card (#232), bind to the line's IMSI, and never block the tick unbounded;
  - parsers: no matching on free-form text.

## Decisions

### D1. Two shared, test-pinned signals, both typed and neither free text

- **Agent exit:** `ims::agent::CARD_UNREACHABLE_MARKER` (an exact string). `ims::agent::run` logs it with `tracing::error!` immediately before exiting when the error `is_card_unreachable()`. The supervisor recognises it with `sim_recovery::has_card_unreachable(log)`, and a test pins the two together, exactly as for `STALL_MARKER`.
- **Card check exit code:** `vowifi::plmn::EXIT_CARD_UNREACHABLE` (3). `vowifi-plmn` exits with it when the error `is_card_unreachable()`, and `FAILURE` (1) for every other error, as today. Existing callers (`read_plmn_once`) treat any non-zero status as failure, so they are unaffected.
- **Alternatives:**
  - Classify the agent exit by running a card check after every exit. Rejected: the spec (FR-001) asks for an unambiguous exit cause, and an unrelated crash during an outage would be misread.
  - Read the agent's exit status. Rejected: the loop polls `is_alive` and never learns the status, by design (learnings: poll, don't `wait`).

### D2. The card check is a bounded subprocess, not a thread

`card_check(runner, bin, imsi) -> CardVerdict` runs `timeout 5 <bin> vowifi-plmn --pcsc-imsi <imsi>` through `runner.run`:

- exit 0 → `Present`;
- exit 3 → `Out`;
- anything else (124 from `timeout`, 1, a spawn error) → `Unknown`.

**Why:**
- A card command stuck inside the reader blocks the caller until it returns. A thread stuck that way cannot be killed, but `timeout` kills the process, so the check is bounded on every path.
- `runner.run` is what `MockCommandRunner` scripts in every existing supervisor test (`set_run_output_sequence`).
- It is the exact read the supervisor already does at start-up (F9), in one PC/SC transaction released with `LeaveCard` (#232).
- **Unknown never starts an outage.**

**Alternatives:**
- In-process `PcscTransport::connect` on a thread (#233's `SimWorker`). Rejected: a hung check cannot be stopped, and the supervisor tests do not inject PC/SC.
- A new `pcsc-check` subcommand. Rejected as YAGNI: `vowifi-plmn` already identifies the card, and reading EF_AD as well costs nothing.

### D3. One shared per-line `CardOutage`, pure, with an injected clock

There is a new module `supervise/card_outage.rs`. It holds one `CardOutage` per card-reader line, behind `Arc<Mutex<_>>`, shared by that line's agent thread and steady loop the way `RebuildGate` is shared (F10).

- **Evidence starts an outage:** a card-unreachable agent exit, or the tunnel gate's check returning `Out`.
- **What ends it:** a `Present` check, or the line registering.
- **What it decides:** whether a check is due (at most one per `CARD_CHECK_INTERVAL` per line, whichever thread asks), the agent restart delay, and when the next tunnel initiation may go.
- **Transitions:** it returns `Began` / `Ended { held_for }`, so the caller logs and reports each exactly once (FR-013, SC-005).

**Why shared:** one outage per line means one "began" and one "ended" line, one check cadence, and one value for the alert. Two trackers would each log and each poll.

**Alternative:** fold it into `UnderlayTracker`. Rejected under D6.

### D4. Pacing agent restarts

- `AgentExitOutcome` gains `CardUnreachable`. It is classified after `AtStall` and `CsimFailure`, and only when the line `pcsc_reader` (FR-003). `observe` leaves the CSIM counters alone for it (`Action::None`, no reset); a card reader has no remedy (FR-004).
- The delay comes from `CardOutage`: `NORMAL_RESTART_DELAY` (5 s), doubling through `ims::session::next_backoff` up to `CARD_OUTAGE_MAX_BACKOFF` (120 s). That cap is the agent's own `RETRY_MAX_BACKOFF`, which is made `pub(crate)` the way #233 did for `RETRY_INITIAL_BACKOFF`.
- The wait replaces `runner.sleep(restart_delay)` with `wait_for_card_or(delay)`. It sleeps in `CARD_CHECK_INTERVAL` (5 s) steps, runs a check when one is due, and returns early on `Present` (FR-006) or when shutting down. That also fixes F1's unbounded shutdown delay for the longer waits.
- A run that registers ends the outage (FR-005). The loop already detects that (`registered, listening for inbound calls`, `:3290`).

### D5. Holding tunnel recoveries that need the SIM: a card gate beside the underlay gate

- **Which recoveries:** `needs_the_sim(reason)`, exhaustive with no wildcard, true exactly for `recovery_renegotiates(reason)` minus `ProcessDied` and `ViciBroken`. Those two restart the shared charon for every line (F6); each line's missing IKE_SA is then gated on its own. A test pins the relationship to `recovery_renegotiates`.
- **Card context.** The tick gets a `CardView` built by the loop before the tick, the way `UnderlayView` is (F4). It holds `pcsc_reader`, `outage` (from `CardOutage`) and `attempt_due`, plus a `check: &dyn Fn() -> CardVerdict` closure, the same shape as `call_active`/`agent_pool`. The closure is used only when there is no outage yet and a SIM-needing recovery is about to be issued on a card-reader line. It is an on-demand check, as the underlay guard's on-demand liveness check is.
- **The rule, at each SIM-needing recovery site** (`IkeSaMissing`, ChildSaStuck, PcscfPoolExhausted, PcscfUnreachable, half-open stuck, ChildSaRekeyStuck, TunVanished):
  - **No IKE_SA to lose** (`IkeSaMissing`, half-open stuck): while the card is out, initiate only when `attempt_due`. Record the attempt, which doubles the interval from `CARD_TUNNEL_INITIAL_INTERVAL` (60 s) to the 120 s cap, and set `agent_restart_owed` as the underlay branch does. Otherwise return `CardHeld { skipped, initiated: false }`. This is FR-007's floor: never fewer than one attempt per cap.
  - **An established IKE_SA that the recovery would tear down** (PcscfPoolExhausted, PcscfUnreachable, ChildSaStuck, ChildSaRekeyStuck, TunVanished): while the card is out, hold it (`CardHeld`) and do not rebuild. Re-establishing needs the SIM, so the rebuild could only replace a working or half-working IKE_SA with none (FR-009; this is what happened live on 2026-11-24). The hold ends the moment a check says `Present`, and the recovery then runs on the next tick. It is not "forever on a wrong verdict": `Out` means pcscd itself, which charon uses too, cannot find the card. `Unknown` never holds.
- **Composition (FR-010):** `underlay_gate` first, then the card gate. If the uplink holds, the outcome is the uplink's `Held` (its surface is unchanged) and the card gate is not consulted. That way neither hold extends the other.
- **A new `SteadyOutcome::CardHeld { skipped, initiated }` variant.** The compiler then forces classification everywhere `SteadyOutcome` is matched exhaustively (`outcome_restarts_agent`, `UnderlayTracker::after`, the loop's handling, log hints). `CardHeld` restarts nothing and starts no uplink hold.
- **The owed agent restart.** It reuses `HeldInitiationEstablished`: an initiation made under the card hold that establishes an IKE_SA proves the card answered. The existing `agent_restart_owed && underlay.holding().is_none()` branch restarts the agent once.
- **Alternatives:**
  - Add `UplinkHoldCause::Card`. Rejected on FR-011(a): it changes what `uplink` means in the protocol, the status file, the gauge and the alert. It would also inherit the underlay's "one initiation per hold" and "reachability-inferred only" rules, which do not fit: `TunVanished` is structural but needs the SIM.
  - Hold nothing destructive and only pace initiations. Rejected: it lets a PcscfPoolExhausted rebuild throw away a working IKE_SA it cannot rebuild, as seen live.

### D6. FR-011's decision: a separate card gate

The criteria, in order:
- **(a)** The uplink surface is untouched (D5's alternative fails this).
- **(b)** Composition is testable with an injected clock: `CardView` and `UnderlayView` are both plain inputs to the same pure tick.
- **(c)** It is the smaller change: one gate function and one outcome variant, against changing an enum that six surfaces render.

### D7. Telling the daemon, for the alert text (FR-014)

- `AgentState` gains `card_hold: Option<bool>`, with `#[serde(default, skip_serializing_if = "Option::is_none")]`. The supervisor's report sets it when `CardOutage` reports `Began` or `Ended`, next to `signal.uplink` (F8).
- `metrics/ingest.rs` keeps `last_card_hold` on the supervisor's record, as `last_uplink` is kept. `tunnel_failure_description` gains a `card_hold: bool` parameter: when true, the text gets `; SIM card out of reach, tunnel attempts slowed until it is back`, after the uplink suffix if there is one.
- The recovery text, the thresholds, the raise and recovery logic, metrics and the status file are unchanged. Every `AgentState { … }` literal (29 sites) uses `..Default::default()` or gains the field.
- **Alternative:** a new `AlertCategory`. Rejected by Clarification 2026-11-24 (option B).

### D8. Operator log lines (FR-013), from the supervisor's stdout

- **Began:** `[supervise] line N: SIM card out of reach; agent restarts and tunnel attempts are slowed until it is back`.
- **Ended:** `[supervise] line N: SIM card reachable again after Ns; resuming`.
- **Per retry:** each paced agent restart keeps today's single line, now with the cause: `vowifi-ims-agent exited (SIM card out of reach); restarting in Ns`. Held tunnel ticks log nothing at the default level, and a paced initiation logs one line.
- No line carries an IMSI (`mask_subscriber_ids` already exists for `vowifi-plmn`'s stderr).

### D9. Constants

| Constant | Value | Why |
|---|---|---|
| `CARD_CHECK_INTERVAL` | 5 s | The same as `HELD_POLL_INTERVAL`, `PCSC_PLMN_READ_RETRY_DELAY` and `PCSC_CONTEXT_RETRY_DELAY` |
| `CARD_CHECK_TIMEOUT` | 5 s | A healthy read takes about 100 ms. charon mid-EAP holds the card for about 1 s |
| `CARD_OUTAGE_MAX_BACKOFF` | `RETRY_MAX_BACKOFF` (120 s) | The spec's assumption: the agent's own ladder |
| `CARD_TUNNEL_INITIAL_INTERVAL` | 60 s | About today's cadence (a 30 s tick plus a ~50 s initiate client) |
| Agent restart initial delay | `NORMAL_RESTART_DELAY` (5 s) | Reused |
| Doubling | `ims::session::next_backoff` | Reused |

- **SC-001:** restarts at about 5, 15, 35, 75, 155, 275, 395 and 515 s, so 8 in 10 minutes (limit 15).
- **SC-002:** initiations at about 60, 180, 300, 420 and 540 s, so 5, plus at most one more if the outage began with a failed attempt (limit 6).

### D10. The steady loop polls quickly while a card hold is on

While `CardOutage` is on, the line's steady loop ticks every `HELD_POLL_INTERVAL` (5 s), as it does around an underlay hold. Each tick asks `CardOutage` whether a check is due, which is shared with the agent thread so it happens at most once per 5 s per line. When the card is back, SC-003's 30 s is spent on the next tick (≤5 s), the initiation (a healthy one takes about 2 s live), and the agent's start and registration (about 7 s live).

### D11. Out of scope, confirmed in code

- **SWu lines:** config validation already rejects `pcsc_reader` with the SWu engine (`orchestrate.rs:1551`).
- **The establishing phase at start-up** (`tick_establishing`) is not gated. That is the "card out when the bridge starts" case.
- **Modem lines:** every new branch is behind `pcsc_reader`, and `CardUnreachable` is only ever built by the PC/SC transport.
