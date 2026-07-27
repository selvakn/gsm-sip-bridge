# Quickstart: Validating the Supervisor Move

## Software-only validation (every phase, no hardware)

```bash
cargo fmt --all
make lint
cargo test --workspace
make test-bash   # Phase 0 only: bats-core over docker/lib/render_helpers.sh
```

All decision logic (rendering, teardown ordering, state-machine transitions, log
parsing) must pass here with no modem, no root, no privileged container.

## Live validation (phase boundaries that touch runtime behavior)

Needs the physical EC20 modem with the Airtel SIM, driven through the existing
privileged `docker-compose.yml` (unchanged capabilities: `privileged: true`,
`network_mode: host`).

```bash
make docker-build
make docker-up
make docker-logs   # watch startup: discover, IMS reconciliation, tunnel establish
```

Checklist per phase:

- **Phase 1 (rendering)**: `gsm-sip-bridge render <asset> ...` output diffed against
  the pre-refactor script's rendered files for the same run; then a full deploy to
  confirm strongSwan/swanctl still accept the Rust-rendered configs.
- **Phase 2 (shutdown)**: `docker compose stop` / `SIGTERM` the container mid-call and
  mid-tunnel-establish; confirm the modem's displaced PDN context is restored
  (`AT+CGACT?`/`AT+CGDCONT?` before/after) and no namespace is left behind
  (`ip netns list` empty after exit).
- **Phase 3 (supervision)**: force each degraded-state transition live — kill `charon`
  mid-session, unplug/replug the modem (CSIM-failure auto-recovery), block the ePDG IP
  briefly (P-CSCF re-initiate cadence) — and confirm the same recovery behavior as the
  pre-refactor script, with the same timing.
- **Phase 4 (shim)**: full cold-start + warm-restart cycle, VoWiFi (both `strongswan`
  and `swu` engines if a second SIM/profile is available) and VoLTE, confirming
  parity with the pre-refactor image end to end.

## Rollback

Every phase is a separate commit range; `docker/entrypoint.sh` remains a working
supervisor at every commit (never mid-refactor-broken), so rollback is `git revert` of
the phase's commits, not a special procedure.
