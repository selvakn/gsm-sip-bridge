# Contract: `gsm-sip-bridge render`

## Invocation

```
gsm-sip-bridge render strongswan-conf --line <idx> --vici-socket <path> --charon-log <path>
gsm-sip-bridge render swanctl-epdg --line <idx> --imsi <imsi> --mcc <mcc> --mnc <mnc> \
    --epdg-ip <ip> --if-id <id> --updown-script <path> [--src-addr <addr>]
gsm-sip-bridge render updown-script --line <idx> --netns <ns> --tun-iface <iface>
gsm-sip-bridge render vpcd-reader-conf --port <port>
```

Prints the rendered asset to stdout; byte-for-byte identical to the current
`docker/entrypoint.sh` heredoc/`sed` output for the same inputs (FR-003). Introduced in
Phase 1 as a thin CLI wrapper over the pure functions in `supervise::render`; retired as
a standalone invocation point once Phase 4 folds asset rendering into `supervise`
in-process (the subcommand form exists so Phase 1 can ship and be validated
independently, per the strangler requirement, before `supervise` itself exists).

## Non-goals

Does not write files itself in the CLI form (caller redirects stdout) — `supervise`
calls the underlying `render_*` functions directly and writes via `CommandRunner`.
