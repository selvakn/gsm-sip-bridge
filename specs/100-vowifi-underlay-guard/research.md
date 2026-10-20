# Research: VoWiFi underlay guard and the half-open IKE_SA fix

All findings below were checked against the code on `main` at `f500c88`, the pinned strongSwan fork (`osmocom strongswan-epdg` at `022f3e515`, strongSwan 5.9.3), the live Vodafone rig, or a primary standards source. Nothing was taken from memory.

## R1. What happens today in an uplink outage (baseline)

**Finding.** These are two 330s outages on the Vodafone rig (2026-10-20, image `gsm-sip-bridge:dpd-098c-local`, P-CSCF probe off).

| | Black-holed route to the ePDG | Inbound from the ePDG dropped |
|---|---|---|
| Agent marks the only P-CSCF unavailable | 62s (`gm-keepalive`) | 80s (`transport`) |
| Supervisor tears the tunnel down | 86s, `PcscfPoolExhausted` | 80s, `PcscfPoolExhausted` |
| `ChildSaMissing` → `ChildSaStuck` churn | every ~80s | every ~80s |
| IKE_SA attempts during the outage | 4 | 4 |
| Agent restarts (`pkill`) | 3 | 4 |
| False "rebuilt repeatedly" tunnel alert | 4m23s | 4m25s |
| charon socket errors | 21 × `error writing to socket: Invalid argument` | 0 |
| Tunnel back after restore | 5s (a supervisor re-initiate happened to coincide) | 9s (charon's own IKE_SA_INIT retransmit) |
| IMS registered after restore | 15s | 18s |

**Decision.** These numbers are the baseline for SC-001 to SC-003 and SC-008. The quickstart reproduces both injections exactly.

## R2. charon cannot tell us the uplink is down

**Finding.** Read in the fork:
- `ike_sa.c roam()` marks an IKE_SA stale only when *no* path to the peer is valid, and only on a kernel roam event. Neither outage produced any roam, stale or "no route" log line. Keepalives continued into the black hole.
- `task_manager_v2.c retransmit()` defers a retransmission for lack of a route only when a MOBIKE task is active. With `mobike = no`, which the swanctl template sets for Vodafone (see its comment), DPD and IKE retransmits are sent, fail, are counted, and the IKE_SA is destroyed after 5 retransmits whatever the route.
- `send_dpd` is not suppressed by the stale condition. Only NAT-T keepalives are (`ike_sa.c:677`).

**Decision.** The guard derives the underlay verdict itself: local route state for *no route*, and the ePDG's silence for *no response*. Re-enabling MOBIKE is rejected because of the Vodafone breakage documented in the template.

## R3. Detecting *no route* locally

**Finding.** The supervisor runs in the host network namespace (the container uses `network_mode: host`). `ip route get <ePDG>` was tried on the rig against a documentation address:

| Route type | Output | Exit |
|---|---|---|
| normal | `192.0.2.123 via 198.51.100.1 dev eth0 src 198.51.100.10` | 0 |
| `blackhole` | `RTNETLINK answers: Invalid argument` | 2 |
| `unreachable` | `RTNETLINK answers: Host is unreachable` | 2 |
| `prohibit` | `RTNETLINK answers: Permission denied` | 2 |

With no default route at all (Wi-Fi gone), the same command fails with `Network is unreachable`.

**Decision.** *No route* = `ip route get <ePDG>` exits non-zero, or succeeds without a `src` address. It runs through the existing `CommandRunner`, so it is scriptable in tests, and at most once per line per tick: one `fork`/`exec` of `ip` every 30s, sending nothing on the wire.

**Alternatives considered.**
- A netlink socket subscription: faster, but adds a long-lived async listener to a synchronous per-line loop, for a 30s cadence that doesn't need it.
- Link carrier state: misses the black-hole and upstream cases, and the uplink interface name is deployment-specific.

## R4. Detecting *no response* from the ePDG's silence

**Finding.** At the fork's default log level (`filelog default = 1`), charon logs every IKE message in and out at `[ENC]` DBG1, and every retransmit at `[IKE]` DBG1, each prefixed `<conn|N>`. Seen on the rig:

```
… 10[ENC] <ims0|1> parsed INFORMATIONAL request 13 [ ]          # the ePDG's own DPD to us
… 10[ENC] <ims0|1> generating INFORMATIONAL response 13 [ ]
… 08[ENC] <ims0|1> generating INFORMATIONAL request 6 [ D ]
… 08[IKE] <ims0|1> retransmit 1 of request with message ID 6
… 07[ENC] <ims0|9> parsed INFORMATIONAL response 5 [ D ]
… 01[ENC] <ims0|5> parsed IKE_SA_INIT response 0 [ SA KE No … ]
```

The 098 parser already reads each CHILD_SA's inbound `… Ns ago` from `swanctl --list-sas`, and the tunnel interface's RX counter.

**Decision.** For one connection, *evidence of life* is any of:
- a `parsed … (request|response)` line after a given log mark;
- a CHILD_SA inbound age younger than the time since the mark;
- a tunnel RX counter that has grown since the mark.

*No response* is established when, since the mark, charon has sent at least 2 retransmits of an outstanding request (any exchange, IKE_SA_INIT included) or the on-demand DPD (R5) has gone 30s unanswered, and there is no evidence of life. This is a new pure function over log text plus counters, alongside the existing `ike_sa_loss_cause` and `inbound_esp` parsers, using the same `lines_for_conn` scoping.

**Alternatives considered.** Counting outer UDP 4500 packets with a socket filter: needs privileges and a capture, and duplicates what charon already logs.

## R5. Sending one liveness check on demand (FR-004a)

**Finding.**
- strongSwan 5.9.3's vici offers `initiate`, `terminate`, `rekey`, `redirect`, `install`, `uninstall` and `reload-settings`. None of them sends a DPD.
- Inside charon, `ike_dpd_create(TRUE)` (`sa/ikev2/tasks/ike_dpd.c`) is the task an ordinary DPD runs: an empty INFORMATIONAL.
- `ike_sa->queue_task()` followed by `ike_sa->initiate(ike_sa, NULL, 0, NULL, NULL)` is how `processing/jobs/initiate_tasks_job.c` starts queued tasks.
- `charon->ike_sa_manager->checkout_by_name(…, name, FALSE)` returns the connection's IKE_SA.
- swanctl registers commands through a constructor in `src/swanctl/commands/*.c`, listed in `src/swanctl/Makefile.am`. `MAX_COMMANDS` is 26 and the table is already full (found when the build first registered the command), so the patch raises it to 27. Short option `D` is unused.

**Decision.** One patch, `docker/patches/0002-strongswan-vici-dpd.patch`, applied in the `strongswan-builder` stage before `autoreconf`. It adds:
1. A vici command `dpd` (`vici_control.c`). It takes `ike` (connection name), checks out that IKE_SA, refuses unless the IKE_SA is `ESTABLISHED`, queues `ike_dpd_create(TRUE)`, starts it, and checks the IKE_SA back in. It replies `success`, plus `ike-id` and an `errmsg` on refusal. It does not wait for the answer.
2. A swanctl command `--dpd` / `-D --ike <name>` (`commands/dpd.c`, modelled on `redirect.c`).

The exchange is an ordinary charon DPD, so it retransmits on charon's normal schedule. An unanswered one deletes the IKE_SA no sooner than an ordinary DPD would (FR-004a). The supervisor reads the answer from the log (R4): a `parsed INFORMATIONAL response <mid> [ ]` for that connection after the mark taken just before the trigger.

**Alternatives considered.**
- A CHILD_SA rekey as the liveness check: rejected in clarification Q1, since the ePDG refused rekeys in #107.
- `charon.check_current_path = yes`: only fires on a roam event, which neither outage produced (R2).
- A Rust vici client: more code than a 60-line C command, and swanctl is already how the supervisor drives charon.
- Waiting for charon's own DPD: up to ~285s, which breaks SC-004 (clarification Q1).

## R6. The half-open IKE_SA: what "stuck" means

**Finding.**
- `list_sas_has_ike_sa` deliberately counts a `CONNECTING` header as present (087 R1), so an in-flight initiate goes down the two-strike CHILD_SA path rather than being re-fired every tick.
- In `ike_sa.c retransmit()`, with `keyingtries = 0`, a failed IKE_SA_INIT logs `peer not responding, trying again (N/0)` and **resets the same IKE_SA** (same unique id `#N`). So during an outage one half-open `#N` can last indefinitely while charon is working correctly.
- charon's retransmit gaps are 4, 7.2, 13, 23.3, 42 and 75.6s, so its longest silence while retrying is ~76s.

**Decision.**
- A new `SteadyStateHealth::IkeSaConnecting { ike_id }`, parsed from the header's state column.
- "Still working" = an `<conn|N>` IKE log line (sent, retransmitted or parsed) within the last 200s.
- "Stuck" = half-open with no such line for over 200s: more than 2.5 × the longest retransmit gap, and longer than one tick plus the gap.
- Stuck and not held → terminate + re-initiate as a new `DegradeReason::IkeSaNegotiationStuck`. It counts as a renegotiation for the P-CSCF sync and the flap window.
- Not stuck → no action, logged once per `#N`.

The spec's FR-015/FR-016 were amended from "attempt age" to "IKE inactivity" because of this finding.

**Alternatives considered.**
- Age of `#N`: wrong, because charon legitimately keeps one `#N` for the whole outage.
- Treating `CONNECTING` as `IkeSaMissing`: re-fires an initiate and restarts the agent every tick, which is the reason 087 avoided it.

## R7. The no-response hold must not starve itself

**Finding.** The swanctl template sets no `dpd_action`, so it defaults to `clear`. A DPD failure during an outage deletes the IKE_SA, and nothing restarts it, because `keyingtries` only governs an initiation in progress. If the guard then also skipped the `IkeSaMissing` re-initiate, nothing would be sent to the ePDG, and a *no response* hold could never see the answer that ends it.

**Decision.** FR-010a was added to the spec. While held with no IKE_SA listed at all, the supervisor issues one `--initiate` with no agent restart, and charon's unlimited retries carry it from there. While any IKE_SA is listed, half-open included, it initiates nothing.

## R8. Where the hold is evaluated, and the 30s liveness wait

**Finding.**
- The strongSwan steady-state loop ticks every 30s (`STEADY_STATE_POLL_INTERVAL`) and holds the shutdown read guard across `tick_steady_state_with_pool`.
- `tick_steady_state_with_pool` is a pure decision over the engine trait, with `call_active`/`agent_pool` closures for agent facts.
- The reachability-inferred recoveries are decided inside it:
  - the `IkeSaMissing` re-initiate;
  - the `ChildSaMissing`/`ChildSaStuck` recoveries;
  - the stuck rekey (`check_rekey_stuck`);
  - `PcscfPoolExhausted`;
  - `PcscfUnreachable`.

**Decision.**
- The underlay verdict is computed by the loop at the top of each tick and passed into the tick as an input, a new `underlay: &UnderlayView` argument, so the tick stays pure and testable. The tick returns a new `SteadyOutcome::Held { cause, skipped: DegradeReason }` instead of issuing a held recovery.
- The FR-004 liveness check is two-phase and never blocks. Tick N sees a reachability-inferred recovery with inconclusive evidence, triggers `swanctl --dpd`, records a pending check (mark + deadline) in `SteadyTracking`, and returns `SteadyOutcome::Deferred { reason }`. A deferral is not a hold: it is not counted, reported or shown as one. Tick N+1, 30s later, reads the outcome: life seen → the deferred recovery runs; no life → the line is held as *no response*.
- One tick equals the spec's 30s wait, so nothing sleeps under the shutdown guard.

**Alternatives considered.** A blocking 30s wait inside the tick: delays shutdown by up to 30s under the read guard (see the comment above the guard in `orchestrate.rs`).

## R9. What a hold suppresses downstream

**Finding.**
- The false alert in R1 is the folded tunnel-failure alert. `metrics/ingest.rs` opens a streak from the supervisor's `tunnel_flapping` state and words it "rebuilt repeatedly" (`tunnel_failure_description`).
- `tunnel_flapping` comes from `RebuildWindow::record`, called only for renegotiating recoveries in the loop.
- The agent restart comes from the loop's `pkill` on renegotiating recoveries.
- The supervisor's state reaches the daemon as `AgentState` fields (`child_sa_up`, `tunnel_flapping`, `ike_sa_loss_cause`) through `sync_tunnel_signal`.

**Decision.**
- A held tick issues no recovery, so it never calls `record` and never `pkill`s. Flapping can't build up during a hold without special-casing the window.
- `AgentState` gains `uplink: Option<UplinkState>` (`up` / `no_route` / `no_response`). `TunnelSignal` carries it, so it is reported on change like the others.
- Ingest keeps it per supervisor record. `tunnel_failure_description` appends `; uplink down (<cause>), recovery on hold` when the record's `uplink` is held at alert time (FR-023). The text is unchanged otherwise.

## R10. Showing the hold in `vowifi-status`

**Finding.** `vowifi-status` (`vowifi::print_status`) queries each IMS agent over its status port and has no channel to the supervisor. The supervisor and agent already share per-line files beside `/tmp/pcscf-N`, written atomically through `ims::pcscf_files` (`.pool`, `.state`, `.calls`).

**Decision.**
- The supervisor writes `/tmp/pcscf-N.uplink` (`pcscf_files::uplink_path`) on every change: a one-line JSON `{"v":1,"state":"no_route","since":<unix>}`, or `{"v":1,"state":"up"}`.
- `print_status` reads it per line and prints `uplink: up`, or `uplink: no_route (held 3m12s, recovery on hold)`.
- A missing or unparseable file prints `uplink: unknown`. This also covers swu lines and the guard being off.

**Alternatives considered.** A new control-socket query to the supervisor: the supervisor serves no socket today.

## R11. Metrics and the 098 classification

**Finding.**
- Supervisor events reach the daemon's registry as `ObservedEvent`s. The daemon owns the counters, so a supervisor restart does not rewind them; 096/097/098 use this pattern.
- `DeadPeerVerdict` is `false|real|unknown`, labelled on `gsm_sip_bridge_vowifi_dead_peer_losses_total`.

**Decision.**
- New events: `UplinkHoldStarted { cause }` and `UplinkHoldEnded { cause, held_secs, ended_by: restored|max_hold|shutdown }`.
- New metrics:
  - `gsm_sip_bridge_vowifi_uplink_holds_total{module,cause}`, incremented on start;
  - `gsm_sip_bridge_vowifi_uplink_held_seconds_total{module,cause}`, added on end;
  - `gsm_sip_bridge_vowifi_uplink_held{module}`, a gauge from `AgentState.uplink`.
- `DeadPeerVerdict` gains `uplink`. A dead-peer IKE_SA loss seen while the line is held is reported with it, never as `real` (FR-022).

## R12. Configuration

**Finding.** 098 added `[vowifi].dpd_delay_sec` through `config/raw.rs` → `config/build.rs` → `config/mod.rs`, documented in `docs/configuration.md` and `config.toml.example`. `tests/test_config_docs.rs` fails on an undocumented key.

**Decision.**
- `[vowifi].underlay_guard: bool`, default `true`, on the same path. serde's bool type rejects non-booleans, and the error names the key.
- Startup logs `[supervise] underlay guard: on` (or `off`) once.
- Off: the loop passes `UnderlayView::disabled()`, which always reads *up* and never triggers a liveness check, so FR-001–FR-013 and FR-018–FR-022 are inert. The half-open handling (R6) runs either way.

## R13. Standards alignment (recorded for review)

- **RFC 7296 §2.4:** never conclude a peer is dead from routing information; routing information may only prompt a liveness check. The guard uses local state only to *defer* its own actions (FR-005), and R5's on-demand check is exactly an RFC 7296 liveness check.
- **RFC 4555 §3.5:** unreliable hints → DPD. Same.
- **3GPP TS 24.302:**
  - §7.2.2A: liveness failure discards the SAs, with no re-establishment timer.
  - §7.2.2.2: backoff timers apply only to explicit IKE_AUTH rejections.
  - §7.2.2.1: the UE must *request* `TIMEOUT_PERIOD_FOR_LIVENESS_CHECK` in CFG_REQUEST. We don't, and neither carrier sends it unasked (Vodafone `CPRP(ADDR6 PCSCF6)`, Jio `CPRP(ADDR ADDR6 DNS DNS6 PCSCF6×2 PCSCF4×2)`). Out of scope.
- **Android IWLAN (`IwlanDataService`, `main`):** on network loss it keeps up tunnels, force-closing only those in bring-down. While the network is down it refuses new IKE with `RESULT_ERROR_TEMPORARILY_UNAVAILABLE`. The guard's hold matches this.
