# Contract: `gsm-sip-bridge supervise`

## Invocation

```
gsm-sip-bridge --config <path> supervise
```

Replaces the entire body of `docker/entrypoint.sh` from "discover once, up front" through
`wait` at the bottom. By Phase 4, `entrypoint.sh` is:

```bash
#!/usr/bin/env bash
set -uo pipefail
GSM_SIP_BRIDGE_BIN="${GSM_SIP_BRIDGE_BIN:-/usr/local/bin/gsm-sip-bridge}"
GSM_SIP_BRIDGE_CONFIG="${GSM_SIP_BRIDGE_CONFIG:-/etc/gsm-sip-bridge/config.toml}"
[ -x "$GSM_SIP_BRIDGE_BIN" ] || { echo "FATAL: ..."; exit 1; }
[ -f "$GSM_SIP_BRIDGE_CONFIG" ] || { echo "FATAL: ..."; exit 1; }
exec "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" supervise
```

## Behavior (unchanged from current `entrypoint.sh`, now internal to the binary)

1. Resolve VoWiFi line table (`discover`) and VoLTE line table
   (`volte-discover-lines`/config), in the same order as today, before starting the
   circuit-switched daemon — preventing concurrent USB scans (FR-012).
2. Enforce VoWiFi/VoLTE mutual exclusion (fatal exit, same message).
3. Start the circuit-switched daemon under `daemon_supervisor::run_supervised`.
4. If VoWiFi enabled: for each resolved line, `LineSupervisor` drives establish → up →
   steady-state, per the configured `tunnel_engine`. Start the shared `vowifi-sip-agent`
   once every line's veth exists.
5. If VoLTE enabled (`bridge_inbound`): per-line namespace/veth setup +
   `volte-carrier-agent`, then the shared `volte-bridge`. Else: single-line
   `volte-register`.
6. On `SIGINT`/`SIGTERM`/normal exit: build and execute the `ShutdownPlan` (FR-004).
7. Exit code: 0 on clean shutdown; nonzero fatal preconditions exit immediately (same
   messages as today — vpcd-reader not ready, missing netns capability, etc.), per-line
   failures are logged and skipped, never fatal to the whole process (FR-013/FR-014).

## Non-goals

Does not change config.toml schema, does not change any AT-command sequence, does not
change any CLI subcommand already exposed (`discover`, `config`, `modem-ims`,
`vowifi-*`, `volte-*`) — `supervise` orchestrates calls to them (in-process where the
logic moved, still subprocess where a leaf tool like `charon`/`swanctl`/`pcscd` is
inherently external).
