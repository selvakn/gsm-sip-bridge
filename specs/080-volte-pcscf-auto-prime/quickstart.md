# Quickstart: Automatic VoLTE P-CSCF Priming

## Trying it (once implemented)

1. Configure only `[volte]`:
   ```toml
   [volte]
   enabled = true
   bridge_inbound = true   # or leave false for the legacy single-line path
   ```
   Deliberately leave `[vowifi]` at its defaults (`enabled = false`), and do
   not set any `pcscf` override, and make sure no stale
   `/tmp/pcscf-0` exists from a previous run.
2. Start the container (or run `supervise` directly against a modem-attached
   host, per this repo's existing `test/` docker setup — see
   [[local-docker-test-folder]] conventions already used for other features).
3. Watch supervise's own stdout for a priming attempt starting, then
   succeeding, then VoLTE registration proceeding — all in one boot, with no
   manual `[vowifi]` toggle and no container restart.
4. Confirm the cache landed exactly as the manual dance describes:
   ```bash
   docker exec <bridge-ctr> cat /tmp/pcscf-0
   ```
5. Confirm nothing was left over from the transient capture:
   ```bash
   docker exec <bridge-ctr> ip netns list      # no leftover priming netns
   docker exec <bridge-ctr> ps aux | grep -E 'charon|vowifi-usim-bridge'
   ```
6. Restart the container without clearing `/tmp/pcscf-0`: priming must NOT
   run again (SC-003) — VoLTE registers immediately.
7. Delete `/tmp/pcscf-0` and restart (simulating a redeploy that wipes it,
   User Story 2): priming must run again automatically and VoLTE must
   recover without any manual step.

## Validated on real hardware (2026-09-17, Vodafone rig)

Steps 1–5 above were run against the real Vodafone SIM/modem
(`test/config.toml`, `[[volte.line]].pcscf` removed to force priming). All
confirmed:

- **The actual IKE_AUTH/P-CSCF capture**: real IKE_SA → EAP-AKA → CHILD_SA →
  `received P-CSCF server IP ...` → written to `/tmp/pcscf-0`, twice, on two
  separate boots (captured two different pool addresses across the two
  runs, both accepted by the IKE_AUTH exchange).
- **The same-process modem hand-off**: VoLTE's own PDN activation and
  carrier-agent startup ran immediately after priming's teardown, in the
  same `supervise` process, using the same modem — no restart in between.
  (This modem is an EC20, which the Jio rig below showed matters: an EC25
  fails here for an unrelated reason — see that section.)
- **Teardown completeness**: `[supervise] teardown: complete, 11 step(s)
  completed, 0 resource(s) not released, 0 abandoned` on every run,
  including after a priming failure (an early pcscd/vpcd-readiness
  failure — see below).

This run also caught and fixed a real bug invisible without hardware:
`discover_priming_line` originally shelled out to the `discover`
subcommand, which unconditionally reports zero VoWiFi lines whenever
`[vowifi].enabled` is false — always true when priming runs. Fixed in
`commands::discover::resolve_vowifi_lines` (called directly, in-process,
bypassing the CLI's `[vowifi].enabled` gate). See that commit for detail;
also see `src/supervise/orchestrate_volte.rs`'s `ensure_pcscf_primed_with`
doc comment for the test-hermeticity issue this same fix surfaced (calling
the real modem scanner from a "unit test" was performing real AT-command
probing whenever hardware happened to be attached to the test machine).

**Not yet resolved from this session**: VoLTE registration itself started
failing with `403 Forbidden - 6037` partway through this validation,
reproduced identically with a freshly-captured P-CSCF, the previously-
known-good P-CSCF, and finally the unmodified stock image — ruling out this
feature's code as the cause. Most likely a carrier-side throttle from the
burst of registration/EAP-AKA attempts during testing (7+ in ~3 minutes).
Left unresolved; re-run steps 6–7 (the skip-priming and redeploy-recovery
scenarios) once VoLTE registration is confirmed healthy again, ideally
spacing attempts out to avoid re-triggering the same throttle.

## Validated on real hardware (2026-09-20, Jio rig, pi@192.168.100.2)

Steps 1–5 run against the real Jio SIM on this Pi's EC25 modem
(`/apps/gsm/config.toml`, switched from a persistent `[vowifi]` deployment to
`[volte].enabled = true` + `bridge_inbound = true`, no `pcscf` override).
Priming itself worked identically to the Vodafone case: full IKE_SA/EAP-AKA/
CHILD_SA, all four P-CSCF candidates Jio always sends (two IPv6, two IPv4 —
matches `engines.rs`'s own doc comment), captured, written to
`/tmp/pcscf-0`, transient tunnel torn down cleanly.

VoLTE's own PDN activation then failed on every attempt: `AT+QNETDEVCTL?`
and `AT+CGACT?` (the pre-existing, priming-unrelated commands `volte::pdn`
uses to activate and rebind the host netdev to the IMS PDP context) both
returned outright `ERROR` — confirmed as a genuine firmware gap, not a race
or a stale-binding symptom: the standalone `volte-pdn --action status`/
`--action down` diagnostics failed identically, and it never once
self-recovered across ~2 minutes of retries. **This EC25's firmware simply
does not implement `AT+QNETDEVCTL`/`AT+CGACT`** — `[volte].bridge_inbound`
cannot work on this modem regardless of how the P-CSCF was obtained; the
classic manual two-restart dance would hit the identical wall. Not a defect
in this feature, and not the same-process-hand-off risk this quickstart
originally worried about — that risk remains open only in the sense that no
hardware run has yet hit it (the Vodafone EC20 run went straight through
with no analogous failure).

Reverting to the original config afterward surfaced a second, separate
finding: the modem briefly reported `SCardConnect: No smart card inserted`
/ `AT+CSIM failed: 0` (the SIM had fallen off the bus — a known failure
mode, unrelated to this feature). Deliberately not intervened on manually;
the existing VoWiFi SIM-recovery logic (`sim_recovery.rs`, three
consecutive CSIM failures → an `AT+CFUN` reset) recovered it within about a
minute, confirmed via `vowifi-status` (`state: Registered`, `can_answer:
true`).
