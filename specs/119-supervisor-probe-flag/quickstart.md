# Quickstart: verifying the supervisor probe flag

## Local (no hardware)

```bash
make format && make lint && make test
cargo test -p gsm-sip-bridge supervisor_pcscf_probe
```

Expected: the default config loads with the probe off; `supervisor_pcscf_probe = true` loads with it on; the tick tests show zero connects flag-off and the old strike/sibling/rebuild behaviour flag-on.

## Live (per carrier: Vodafone local rig, Jio Pi)

Use the privileged container under `test/` (see the memory notes on local docker tests) and the recipes in `specs/098-dpd-tuning/live-verification.md`. Throwaway image tags only.

1. **Startup line**: default config logs `supervisor P-CSCF probe off`. Set the option, recreate, see `on`.
2. **Capture (SC-001)**: `tcpdump` inside the line's netns, 1 hour, `tcp dst port 5060 and tcp[tcpflags] & tcp-syn != 0` toward the P-CSCF: expect none flag-off, one per ~30s flag-on.
3. **Route (SC-003)**: `ip -n <netns> route del default` (v4 or v6 as the P-CSCF's family); expect the repair log within one tick and no `tunnel_recovered`.
4. **Dead P-CSCF (SC-004/005)**: `nsenter` + a /32 (/128) route to the veth peer. Jio: agent failover, registered within ~193s, no rebuild. Vodafone: `pcscf_pool_exhausted` rebuild, registered within 315s.
5. **Flag on (SC-006)**: repeat 4 on Vodafone with the option on; expect `pcscf_unreachable` after ~90s as before.
6. **Soak (SC-002)**: 4.5h idle flag-off on both rigs; no teardown, registered throughout.

Record results in `specs/119-supervisor-probe-flag/live-verification.md`. No real numbers or identifiers.
