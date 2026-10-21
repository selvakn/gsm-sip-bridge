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

## Live bridge, one real VoWiFi line (2026-10-21)

Image built from main at 4ab6c07. The ePDG's /16 was dropped inbound with an nft rule (priority -300) before the container was recreated, so startup ran against a silent ePDG; the rule was then removed.

- **Silence, ~7 minutes (T017 short):** 14 samples, `swanctl --list-sas` rc 0 on every one, charon's log advancing throughout (28 -> 62 lines), 17 threads constant, charon never restarted, no wedge line. Log: `initiate started`, `initiate client exited after 28s`, `IKE_SA #1 still negotiating; leaving it to charon`. The "after 28s" is when the client was collected (at the next 30s probe), not how long it ran; swanctl detaches at 20s. (`pgrep -fc` reads 1 because it matches its own `sh -c`.)
- **Recovery (US2):** path restored after ~10 minutes of silence; `tunnel UP` 10-15s later, no container or charon restart. A first attempt to restore failed because the nft table outlives the container that created it and had to be deleted explicitly.
- **Shutdown mid-attempt (T039):** SIGTERM while the client was running. Teardown killed charon, but the `timeout ... swanctl --initiate` wrapper was still present at t+1s, until the container exited. Nothing reaps it: the persistent line's establish loop is not stopped by shutdown, so `cancel_initiate` is not reached on that path. It is bounded (the wrapper ends by 27s on its own), so it cannot outlive the bridge process by more than that outside a container, but US6's "nothing left behind" is not strictly met.
- The bridge was returned to its previous image afterwards and re-established its tunnel.

## Not run

- T034 (two lines, one silent): the host has one modem. Unit tests cover the logic.
- T036: an orchestrate-level shutdown test is not feasible with the mock; the priming path stops at the vpcd readiness check before it reaches the initiate.
