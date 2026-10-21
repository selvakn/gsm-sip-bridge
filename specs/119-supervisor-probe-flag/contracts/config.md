# Contract: configuration

| Section | Key | Type | Default | Meaning |
|---|---|---|---|---|
| `[vowifi]` | `supervisor_pcscf_probe` | boolean | `false` | Every steady-state tick (30s on strongSwan) the line supervisor opens and closes a TCP connection from the line's namespace to the P-CSCF's unprotected SIP port, and rebuilds the tunnel after ~90s of failures, choosing a reachable pool sibling on the way. Off by default (#120): no handset does this, and the replacements (per-tick route check, rebuild when the agent has marked every P-CSCF, tuned DPD) cover what it caught. Turn it on for a single-P-CSCF carrier where you want the faster ~97s detection of a dead P-CSCF instead of the agent's ~115s + poll, or to get the previous behaviour exactly. Both engines. Per deployment; takes effect when the container is recreated |

The shell export (`gsm-sip-bridge config --shell-env`, vowifi section) gains `SUPERVISOR_PCSCF_PROBE='true'|'false'` directly after `DPD_DELAY_SEC`.

Invalid (non-boolean) values fail config loading through the existing TOML type error, which names `vowifi.supervisor_pcscf_probe`.

`docs/configuration.md` gets one row; `config.toml.example` gets the key commented out at its default; the `dpd_delay_sec` and `idle_keepalive` rows are reworded (research R6).
