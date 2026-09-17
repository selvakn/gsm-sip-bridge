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
