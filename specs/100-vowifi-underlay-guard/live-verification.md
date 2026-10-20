# Live verification: VoWiFi underlay guard (#141)

Run 2026-10-20 (UTC), from the quickstart's recipes. Addresses and identities are redacted to roles.

## Rigs and builds

- **Vodafone:** the local `test/` rig, container `gsm-sip-bridge-test`, amd64, host networking.
- **Jio:** the Raspberry Pi, `gsm_gsm-sip-bridge_1`, arm64, docker-compose v1.

Every image was built from a `git archive` of a commit on this branch, with one throwaway change in the scratch copy only (never committed): `StrongswanEngine::pcscf_reachable` returns `true`. That is the "P-CSCF probe forced off" setup of the 098 baseline (research R1), so the probe's own traffic does not mask charon's DPD and the reachability inference rests on the agent's verdict.

| Build tag | Branch commit | What it lacked |
|---|---|---|
| `forcedprobe` | `76e7936` | the no-route nudge, the 5s hold poll, the activity fingerprint |
| `forcedprobe2` | `2eccaf7` | the 5s hold poll |
| `forcedprobe3` | `f9234d2` | the activity fingerprint, the 30s linger |
| `forcedprobe4` | `09139b3` | nothing: the final code (`09139b3` is HEAD for every code change) |

The Pi ran `forcedprobe3` and `forcedprobe4` and was put back on its own `dpd-098c-arm64` image (compose file restored byte-identical, line registered and healthy). The Vodafone rig was put back on `dpd-098c-local` and its original `config.toml`.

## What the runs found and what was changed because of them

These are not all pass. Each row is a finding from a live run, and the change that followed is on this branch.

| Run | Finding | Change |
|---|---|---|
| Vodafone, no route, `forcedprobe` | Not yet registered 80s after the route returned (SC-003: 30s). charon's IKE_SA_INIT retransmits had gone into the dead route and its backoff left 46s until the next one. | `2eccaf7`: a half-open IKE_SA left from a *no route* hold is re-initiated on the evaluation where the route returns. |
| Vodafone, no route, `forcedprobe2` | Registered 39s after. The hold's end was noticed up to 30s late (one 30s tick), and per-tick "holding X" lines were noisy. | `f9234d2`: a 5s tick while held; what a hold held back is logged once, on the line that ends it. |
| Jio, first run | charon logs in local time (IST on the Pi), so the half-open "stuck" check, which parsed those timestamps as UTC, could never have fired there. | `da7a475`: idleness is judged by a fingerprint of the IKE lines compared between ticks. No time is parsed. |
| Jio, no response, `forcedprobe3` | The IKE_SA was back 20s after the restore, but the new P-CSCF waited for the next 30s tick. | `09139b3`: the 5s tick continues for 30s after a hold ends. |

## Results

### SC-001 and SC-002: the 330s outages

Every guard-on run below had **0** `recovered from`, **0** `tunnel flapping` and **0** supervisor-issued agent restarts between the hold's start and its end. The supervisor issued exactly **one** initiation (FR-010a), only after charon's own DPD had cleared the IKE_SA. The line logged `dead peer verdict: uplink`. For comparison, the baseline (research R1) churned every ~80s: 4 IKE_SA attempts, 3 to 4 agent restarts and a false "rebuilt repeatedly" alert at 4m23s.

| Rig | Build | Injection | Hold started | SC-002 (60s / 120s) | Hold ended |
|---|---|---|---|---|---|
| Vodafone | `forcedprobe` | route to the ePDG black-holed | +43s | pass | 13s after the route returned |
| Vodafone | `forcedprobe2` | route black-holed | +56s | pass | 26s after (30s tick) |
| Vodafone | `forcedprobe` | inbound from the ePDG dropped | +80s | pass | with the first answer, after charon's retry |
| Vodafone | `forcedprobe4` | inbound dropped | +56s | pass | with the first answer |
| Jio | `forcedprobe3` | route black-holed | +49s | pass | 3s after the route returned |
| Jio | `forcedprobe3` | inbound dropped | +112s | pass | 19s after |
| Jio | `forcedprobe4` | inbound dropped | **+148s** | **fail** | 10s after |

The one SC-002 miss is the Jio dropped-inbound run at +148s. A *no response* hold needs two unanswered retransmits of a request, and with nothing else arriving the first request is charon's own DPD, 120s after the last packet received (`dpd_delay_sec`, specs/098), plus 4s and 7s for the retransmits, plus up to one tick. That is 131s to 161s depending on when the last packet came before the injection. The 80s and 112s runs were shorter because charon's DPD fired 60s and 89s after the injection (the last packet had arrived 60s and 31s before it); in the 148s run it fired 120s after. The 120s in SC-002 cannot be met in the worst case with `dpd_delay_sec = 120`.

Mid-outage `vowifi-status` read `uplink: no_route (held 7s, recovery on hold)` on Vodafone, and `uplink: up` after.

The IMS agent exited and restarted itself every 6 to 8s for most of each hold (2 to 80 times a run): with no tunnel it has nothing to register through. That is the agent's own loop, not a supervisor action, and it is unchanged from before.

### SC-003: registered after the path returned (30s)

| Rig | Build | Injection | Registered after restore |
|---|---|---|---|
| Vodafone | `forcedprobe` | no route | over 80s, not timestamped (before the nudge) |
| Vodafone | `forcedprobe2` | no route | 39s (before the 5s poll) |
| Vodafone | `forcedprobe4` | no route, 16.5 min (T043) | **11s** |
| Jio | `forcedprobe3` | no route | **12s** |
| Jio | `forcedprobe4` | inbound dropped | **17s** |
| Jio | `forcedprobe3` | inbound dropped | 82s (30s tick, then three `AT+CSIM failure` agent restarts) |
| Vodafone | `forcedprobe` | inbound dropped | 89s (before the fixes) |
| Vodafone | `forcedprobe4` | inbound dropped | **46s: not met** |

SC-003 is met for *no route*, where the supervisor knows the instant the route is back. It is **not reliably met for *no response***. The supervisor learns that the path is back only when the ePDG answers something, and with the hold in place nothing but charon sends to it. charon's IKE_SA_INIT chain has gaps of 4, 7, 13, 23, 42 and 76s before it starts a fresh chain, so the first packet after a restore can be up to 76s away. On the last Vodafone run it was 31s after the restore (IKE_SA up at +35s, registered at +46s). The baseline's 9s came from the supervisor's own churn re-initiating by luck.

Meeting 30s there would need either the supervisor re-initiating a half-open IKE_SA while it is held, which FR-010 and FR-015 forbid, or shortening charon's retransmit schedule (`charon.retransmit_limit`), which also moves the 165s give-up window that specs/098 classifies against. Neither was done. This is left for a decision.

### SC-004: a real carrier fault is still repaired

- **Single P-CSCF dead behind a live ePDG** (Vodafone, `forcedprobe`, `/128` route to the veth): the agent marked it unavailable at +33s, the supervisor's tick found the ePDG silent for 30s and sent one DPD (`checking it is alive (DPD sent)`) at +52s, the ePDG answered, and the tunnel was rebuilt at +83s (`recovered from PcscfPoolExhausted`). No `uplink down`. 097 measured 206s and 098's estimate was ~145s, so this is inside "existing time + 30s".
- **Namespace default route deleted** (`ip -n ims0 -6 route del default`): `DefaultRouteMissing` was repaired 5s later, on the next tick, with no hold.

### SC-005: an outage longer than 15 minutes (Vodafone, `forcedprobe4`)

A 1000s route black-hole. The hold began at +50s, and at 902s the log read `uplink hold (no_route) ended after 902s (max_hold)`. That tick ran unheld once (charon's half-open IKE_SA was still being retried, so there was nothing to recover). A new hold started 5s later. The route returned at +16m40s, the hold ended 2s after, the IKE_SA was re-initiated and up 4s after that, and the line registered **11s** after the route returned.

### SC-006: no extra traffic on an idle line

Outer UDP 4500 packets to and from the ePDG, 30 minutes idle, the same rig:

| `underlay_guard` | Packets | `swanctl --dpd` calls |
|---|---|---|
| `true` | 145 | 0 |
| `false` | 147 | 0 |

I took 30 minutes, not the quickstart's hour. The guard sends nothing on a healthy line by construction (the route check is local, and a DPD is sent only when a recovery is about to be taken), and the two counts agree.

### Metrics (quickstart §8)

Scraped from the live daemon (`forcedprobe4`) after a 150s route black-hole on Vodafone that ended in a restore:

```text
gsm_sip_bridge_vowifi_uplink_holds_total{cause="no_route",module="<card-id>"} 1
gsm_sip_bridge_vowifi_uplink_held_seconds_total{cause="no_route",module="<card-id>"} 95
gsm_sip_bridge_vowifi_uplink_held{module="<card-id>"} 0
```

The log line for the same hold read `uplink hold (no_route) ended after 95s (restored); held back: PcscfPoolExhausted`. I did not scrape during a hold, so the gauge reading 1 is covered by `tests/test_vowifi_tunnel_recovery_metrics.rs` (a real reporter, control socket and registry), not by this run.

### SC-007: unit tests

Covered in `supervise/line_supervisor.rs`: `a_half_open_ike_sa_with_recent_activity_is_left_alone`, `waiting_on_the_same_ike_sa_is_logged_once_and_never_strikes` (five ticks, no action), `a_half_open_ike_sa_silent_for_over_200s_is_terminated_and_reinitiated` (exactly one stuck-negotiation recovery) and `new_ike_activity_resets_the_idle_clock_however_old_the_ike_sa`.

### SC-008: guard off reproduces the baseline

`[vowifi].underlay_guard = false`, recreated, not restarted. Startup logged `underlay guard off ([vowifi].underlay_guard = false)`, `vowifi-status` read `uplink: unknown`, and no `.uplink` file was written. Build `forcedprobe4`.

| Injection | Outcome |
|---|---|
| Route black-holed, 330s | `recovered from PcscfPoolExhausted` at +87s, one agent restart, no hold; registered 25s after the route returned |
| Inbound dropped, 330s | `recovered from PcscfPoolExhausted` at +159s, one agent restart, no hold; registered 37s after |

The baseline is reproduced: a teardown at ~80s on the first and later on the second (the agent's keepalive timing set the mark), with an agent restart, and nothing held. The follow-on churn of the baseline (a rebuild every ~80s) does not appear, which is the half-open fix: the second IKE_SA was left to charon.

## Notes

- Vodafone's ePDG address alternated between two values from one container to the next, so each run reads it from `swanctl --list-sas`. One run's injection hit the wrong address and did nothing; it is not counted above.
- `swanctl --dpd --ike ims0` exited 0 on both rigs and logged `parsed INFORMATIONAL response` within the same second. Against an unknown connection it exited 1 with `dpd failed: no IKE_SA named nosuch`. The first build of the fork patch failed at startup with `unable to register command, please increase MAX_COMMANDS` because the table was already full; the patch now raises it to 27 (`0697546`).
