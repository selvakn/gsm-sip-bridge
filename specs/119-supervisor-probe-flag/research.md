# Research: Supervisor P-CSCF probe flag

## R1. Where to put the gate

**Decision**: in `tick_steady_state_underlay` (`line_supervisor.rs`), around the block that calls `engine.pcscf_reachable` for the selection and for siblings (today's lines ~1569-1604), keyed on `tracking.probe_disabled`.

**Rationale**: it is the only caller of `pcscf_reachable` in production (the two `TunnelEngine` impls are the callee). Both loops (strongSwan at `orchestrate.rs` ~1882, SWu at ~3350) already go through it, so one `if` covers both engines and FR-003 ("any engine") holds by construction. The route repair and the pool-exhaustion rebuild run *before* the block and return early, so skipping the block cannot affect them (FR-005/006).

**Alternatives**: (a) a `NoProbeEngine` wrapper whose `pcscf_reachable` returns `true`: rejected, "reachable" would then be a lie in logs and any future caller, and the streak/sibling bookkeeping would still run. (b) a new argument on the three `tick_steady_state*` functions: rejected, `tick_steady_state_with_pool` and `_underlay` already take 8-9 arguments with an `#[allow(clippy::too_many_arguments)]`, and about 60 tests call them. (c) a runner that refuses the connect: rejected, still counts as a failure.

## R2. How the flag reaches the tick

**Decision**: a `probe_disabled: bool` field on `SteadyTracking`, set in the two places the loops build it, from `!config.vowifi.supervisor_pcscf_probe`.

**Rationale**: `SteadyTracking` is already the per-line state "owned by the loop" and both construction sites already use struct-update syntax with `..Default::default()`. Stored inverted so the derived `Default` (`false`) means "probe runs", which is the behaviour every existing test asserts; none of them needs editing. The config default is the opposite polarity (`supervisor_pcscf_probe = false`), which is the whole point of the feature, and the inversion is confined to one line per loop.

**Alternatives**: a positive `probe_enabled` field would need a hand-written `Default` of `true` and would make the config default (`false`) and the struct default (`true`) disagree, which is the confusing direction.

## R3. What the skipped block must leave behind

**Decision**: when disabled, set `pcscf_via_sibling = None` and reset the `pcscf_unreachable` streak, then fall through to `SteadyOutcome::StillUp`.

**Rationale**: FR-008 says no result is not a strike and not a success. `pcscf_via_sibling` is read by `report_pcscf_sibling` (`orchestrate.rs` ~2532) to print "pool member answers"; stale `Some` after a flag-on to flag-off change cannot happen at runtime (the flag is fixed for the process) but resetting keeps the field truthful. The reset of the streak is likewise a no-op at runtime and keeps tests that start from a dirty `SteadyTracking` honest.

## R4. The sibling-selection side effect

The probe also decided which pool member the *supervisor* believed was reachable. Nothing downstream needs that with the probe off: the agent owns failover (spec 091), and the pool the supervisor republishes is the advertised set, not the probe's verdict. `pcscf_via_sibling` only feeds a log line. So skipping it loses a log line and nothing else. Verified by reading its three read sites (`orchestrate.rs` ~2010, ~2532, ~3369).

## R5. Config name, section and polarity

**Decision**: `[vowifi].supervisor_pcscf_probe`, boolean, default `false`, global (not per line), takes effect on container recreate. The strongSwan and SWu engines both honour it.

**Rationale**: sits beside `idle_keepalive`, the sibling probe switch from spec 094, and reads the same way. "supervisor_" distinguishes it from the keepalive thread and the health check. Positive polarity matches `idle_keepalive`/`underlay_guard`.

**Export**: `SUPERVISOR_PCSCF_PROBE` in `render_vowifi_shell_env`, as `IDLE_KEEPALIVE` is, after `DPD_DELAY_SEC`. The shell scripts do not read it; the export is how an operator confirms the value (as docs already say for `IDLE_KEEPALIVE`).

## R6. Interactions

- **Spec 098 (DPD)**: the docs for `dpd_delay_sec` say "No effect while the line supervisor's P-CSCF probe runs". With the probe off by default, DPD now actually fires every 120s. That is the tuned design, but it is a visible behaviour change for every existing deployment on upgrade and must go in the release notes. The `dpd_delay_sec` doc sentence needs rewording: it matters *unless* the probe is turned on.
- **Spec 094 (`idle_keepalive`)**: independent. Its doc text says "the line supervisor already probes the P-CSCF every 30s"; reword to "may probe" and point at the new option. Its own startup log line says "P-CSCF reachability is probed by the line supervisor only" when off; that string must become conditional on the new flag or it will lie.
- **Spec 100 (underlay guard)**: the guard gates the `PcscfUnreachable` rebuild. With the probe off that rebuild never fires, and `PcscfPoolExhausted` (also gated) is the one that matters. No code change.
- **Metrics**: `tunnel_recovered{reason="pcscf_unreachable"}` is a fixed enum in `control/protocol.rs`; it stays and simply reads zero (FR-010). Nothing to change.
- **Orchestrate ~3231**: the idle-keepalive's discarded `tcp_connect_ok_in_netns` is the 094 option and is already off by default; untouched.

## R7. Live-verification design

Reuse the recipes already written down: spec 098's `live-verification.md` (the /32 or /128 black-hole to the veth peer, `dpd_delay_sec` handling, charon local-time logs) and spec 096's route deletion. Runs, each on Vodafone (sole P-CSCF) and Jio (pool of four):

1. Probe-off capture: one hour in the tunnel, zero SYNs to the P-CSCF's :5060 (SC-001).
2. Delete the netns default route: restored within a tick (SC-003).
3. Black-hole the current P-CSCF: Jio fails over by the agent (SC-005), Vodafone rebuilds via pool exhaustion (SC-004).
4. Flag on, repeat 3 on Vodafone: `pcscf_unreachable` rebuild as before (SC-006).
5. 4.5h idle soak, flag off: no teardown (SC-002).

The privileged Docker test container (`test/`) is used for anything needing CAP_NET_ADMIN; the Pi runs via the arm64 pipeline.
