# Live verification (#139)

Rig: `rig/` per `quickstart.md`, image `gsm-sip-bridge:sms101-7d35bfa-amd64` (pinned strongSwan fork, charon only), silent UDP sink at 172.29.139.99, 2026-10-21. No modem, no real ePDG.

The rig drives `swanctl` directly; `loop-fixed.sh` reproduces the fixed supervisor's exact initiate shape (`timeout -k 2 25 ... --timeout 20`, byte-for-byte what `initiate_argv` builds, asserted by unit test) and its one-client-per-line skip. The Rust decision logic (skip, half-open deferral, strikes, wedge recovery, cancel) is covered by unit tests, not by this rig.

## Baseline (T002): unmodified behaviour, `loop.sh "" 22`, 5s cadence

| tick | clients | charon_threads | list_sas_rc | log_lines |
| --- | --- | --- | --- | --- |
| 1 | 1 | 17 | 0 | 139 |
| 11 | 11 | 17 | 0 | 245 |
| 12 | 12 | 17 | **143** | 249 |
| 22 | 22 | 17 | 143 | 249 |

The control interface stops answering at the 12th initiate and the log never advances again. Reproduces the issue.

## Fixed shape, 22 ticks (T017 short run): `loop-fixed.sh 22`

`list_sas_rc=0` on all 22 ticks, `log_lines` 141 -> 268 and rising, one logical client at a time (`pgrep -fc` reads 2 because it counts the `timeout` wrapper and its `swanctl` child), a new initiate skipped while the previous client ran.

## Long silence (T017): `loop-fixed.sh 72` (6 minutes of silence)

72/72 probes `list_sas_rc=0`, 0 timeouts, `log_lines` 803 at the end and rising throughout, charon never restarted.

## Recovery readiness (T016/US2): `recover.sh 90`

`RECOVERY-READY`: the daemon answers `--list-sas`, IKE_SA_INIT retransmits went 5 -> 10 over the window (a window shorter than charon's ~76s backoff reads as stalled), and a fresh bounded initiate was accepted. A real ePDG returning is not simulated here: that needs a responder, so the "line registers without a restart" step stays a hardware check.

## Not run here

- T034 (two lines, one silent, 30 min) and T039 (shutdown mid-attempt on a live bridge) need the bridge running against real lines. Their logic is unit-tested (`only_one_of_two_lines_restarts_a_wedged_shared_daemon`, `a_silent_epdg_on_one_line_never_touches_a_healthy_line`, `cancel_initiate_reaps_the_outstanding_client_and_is_idempotent`), but there is no live record.
- No orchestrate-level test drives shutdown through `establish_line_tunnel`'s `cancel_initiate` call (the priming test never reaches the initiate); the call sits after the loop so every exit passes it.
