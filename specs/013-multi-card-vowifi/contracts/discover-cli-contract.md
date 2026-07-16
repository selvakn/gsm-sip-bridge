# Contract: `gsm-sip-bridge discover`

A new one-shot subcommand, the single point where discovery + role assignment happens
(research.md item 3). Both `docker/entrypoint.sh` and the circuit-switched daemon consume its
output; neither re-runs discovery independently once this has run.

## Invocation

```
gsm-sip-bridge --config <path> discover [--out <path>] [--shell-env]
```

- `--out <path>`: where to write the `LineResolution` JSON (data-model.md). Default:
  `/tmp/gsm-sip-bridge-lines.json`, overridable via `GSM_SIP_BRIDGE_LINES_FILE` env var (same
  precedence pattern as `GSM_SIP_BRIDGE_BIN`/`GSM_SIP_BRIDGE_CONFIG` in `entrypoint.sh`).
- `--shell-env`: instead of (or in addition to — both may be requested) writing JSON, print
  shell-sourceable `eval`-safe output to stdout, one line per scalar plus indexed arrays for
  per-line fields, e.g.:
  ```sh
  LINE_COUNT=2
  LINE_CARD_ID=(ec20-1A2B3C ec20-9F9F9F)
  LINE_MODEM_PORT=(/dev/ttyUSB6 /dev/ttyUSB10)
  LINE_NETNS=(ims0 ims1)
  LINE_CONTROL_PORT=(7050 7050)
  LINE_VETH_LOCAL_ADDR=(10.90.1.2 10.90.5.2)
  LINE_VETH_PEER_ADDR=(10.90.1.1 10.90.5.1)
  LINE_VPCD_PORT=(7100 7101)
  LINE_STRONGSWAN_IF_ID=(23 24)
  LINE_STRONGSWAN_TUN_IFACE=(tun23-0 tun23-1)
  LINE_PCSCF_SOURCE_PATH=(/tmp/pcscf-0 /tmp/pcscf-1)
  CS_EXCLUDED_PORTS=(/dev/ttyUSB6 /dev/ttyUSB10)
  ```
  Values are shell-quoted the same way `print_vowifi_shell_env` already quotes them.

## Behavior

1. Runs the shared inventory scan (`modules::discovery::scan_all`) once.
2. If `[vowifi].enabled = false`: writes/prints `LINE_COUNT=0`, `CS_EXCLUDED_PORTS=()`, and
   exits `0` — the CS daemon proceeds exactly as it does today (no exclusions, no VoWiFi setup).
3. If `[vowifi].enabled = true`: computes `RoleAssignment`, then `LineTable` (bounded by
   `max_lines`), then derives every per-line `VowifiConfig` (research.md item 5). Any failed
   modem (no AT port / no usable SIM) is logged with its reason (FR-006) and included in
   `failed`, not fatal to the command's exit code.
4. If VoWiFi is enabled but zero usable lines resolve: still exits `0` (degrade, per the spec's
   clarification — "Log a prominent error, skip the VoWiFi subsystem... container stays up"),
   emitting `LINE_COUNT=0` so `entrypoint.sh` skips VoWiFi setup and lets the CS daemon run alone.
   The prominent error is logged by `discover` itself, not deferred to a caller.
5. Exit code is non-zero **only** for a genuine command failure (bad `--config`, cannot write
   `--out`), never for "zero lines found" (that is success — see step 4).

## Consumers

- `docker/entrypoint.sh`: replaces its current single-shot `config vowifi-shell-env` call for
  everything line-related; still calls `config vowifi-shell-env` (or a trimmed version of it) for
  the handful of fields that remain global (`APN` is per-line now via research.md's table, so
  check — everything currently in `print_vowifi_shell_env` that varies per line moves here;
  anything that stays global, e.g. `METRICS_PORT`, `TUNNEL_ENGINE`, stays in
  `vowifi-shell-env`). Loops `for i in $(seq 0 $((LINE_COUNT - 1)))` over the arrays to start one
  full per-line stack (research.md item 4).
- Circuit-switched daemon startup (`main.rs`, before `CardPool::new`): reads
  `CS_EXCLUDED_PORTS` from the same resolution (via `--out`'s JSON, since the daemon is a Rust
  process, not a shell) and passes it to `scan_modules` so those ports are never opened by the
  CS side (FR-007).

## Backward compatibility

An existing single-SIM deployment that sets `[vowifi].modem_port` explicitly resolves to exactly
one `ResolvedLine` whose `index = 0` derivations equal today's unindexed defaults (data-model.md,
FR-020) — `discover`'s output for that deployment is behaviorally a formality, not a change.
