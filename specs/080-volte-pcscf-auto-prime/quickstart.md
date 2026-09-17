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

## What this quickstart cannot verify outside real hardware

This session's sandbox has no root/`CAP_NET_ADMIN` (see project memory:
sandbox blocks root network testing), so none of the above can actually be
exercised here — only `cargo test` against `MockCommandRunner` is available
in this environment. Before trusting this feature in production, run the
above quickstart on real hardware (the project's existing privileged Docker
test setup) at least once for:

- **The actual IKE_AUTH/P-CSCF capture** — the persistent `[vowifi]` path
  this reuses is hardware-proven, but priming's *bounded timeout* and
  *explicit teardown* are new code paths that have not themselves run
  against a real charon/modem.
- **The same-process modem hand-off** — priming and VoLTE's own PDN/
  registration bring-up now touch the same modem back-to-back within one
  continuous process lifetime, where the manual dance always did so across
  a full container restart instead. This is the single biggest behavioral
  difference from the already-proven manual procedure and the one thing
  most worth a dedicated hardware pass.
- **Teardown completeness under a real failure** — that
  `shutdown::execute_shutdown_plan` genuinely leaves no XFRM `if_id`, netns,
  or process behind when a priming attempt fails partway through, on a real
  kernel (its logic is reused unchanged from the container-shutdown case,
  but that case tears down at *process exit*, not mid-run).
