# Live verification: supervisor P-CSCF probe flag (#120)

Run 2026-10-21 on the local Vodafone rig (single P-CSCF, IPv6), image `gsm-sip-bridge:probe119-8e5d11c-amd64` built from `git archive HEAD`, default config (`supervisor_pcscf_probe` unset). Times are UTC.

## Scope

Run: startup line, a 5-minute connect count, default-route deletion, black-holing the sole P-CSCF.
Also run: the flag-on black-hole repeat (T021). Not run: the one-hour capture (SC-001 is shown by a 5-minute count only), the Jio Pi runs (T022), the 4.5h soaks (T023). SC-002 and SC-005 are therefore unverified.

## Results

| Step | Evidence | Result |
|---|---|---|
| Startup line (US1 AS2) | `supervisor P-CSCF probe off ([vowifi].supervisor_pcscf_probe = false); route loss, an exhausted P-CSCF pool and a dead peer are repaired from their own signals`; the idle-keepalive line reads `...the supervisor's own P-CSCF probe is [vowifi].supervisor_pcscf_probe = false` | pass |
| No probe connects (SC-001, short) | TCP `ActiveOpens` in the line's netns (`/proc/net/snmp`): 2 at 17:54:44, 2 at 17:59:47. The old probe adds about 10 in 5 minutes | pass (5 min, not 1 h; counts all TCP opens, not only :5060) |
| Default route deleted (SC-003) | `ip -6 route del default dev tun23-0` at 17:59:52; `recovered from DefaultRouteMissing` at 18:00:16, route back by 18:00:17 (about 25s, within one 30s tick) | pass |
| Sole P-CSCF black-holed (SC-004) | `/128` route to the veth peer's link-local in the netns at 18:00:35. `the ePDG has been silent for 30s; checking it is alive (DPD sent) before PcscfPoolExhausted` at 18:03:16, `recovered from PcscfPoolExhausted` at 18:03:46 (191s), line `Registered` again at 18:03:59 (204s). Bound is 315s | pass |
| Flag on, sole P-CSCF black-holed (SC-006) | `supervisor_pcscf_probe = true`, container recreated; startup line `supervisor P-CSCF probe on: TCP connect to each line's P-CSCF :5060 every supervisor tick`. Black-hole injected at 18:18:04. `PcscfUnreachable` came due and the guard's liveness check started at 18:19:23 (79s later, the ~90s strike window less what had already accrued), then `recovered from PcscfPoolExhausted` at 18:19:53 (109s): the agent's mark landed during the 30s check and the pool-exhaustion branch runs first in the tick. Line registered again within the same minute | pass: the probe path engages and detects the dead P-CSCF about 30s ahead of the agent-only path (191s with the probe off) |

The underlay guard's liveness check added about 30s before the rebuild, as designed (specs/100).

## Jio Pi (4-member P-CSCF pool), default config

Image `gsm-sip-bridge:probe119-8e5d11c-arm64` (arm-build-200, `git archive HEAD` of the same tree), container recreated 2026-10-21 about 19:20Z; previous tag `sms101-7d35bfa-arm64` kept loaded, compose backed up as `docker-compose.yaml.bak-pre-probe119`.

| Step | Evidence | Result |
|---|---|---|
| Startup line | `supervisor P-CSCF probe off` | pass |
| No probe connects (SC-001, short) | TCP `ActiveOpens` in the line's netns: 2 at 19:25:25, 2 at 19:30:36 | pass (5 min, not 1 h) |
| Default route deleted (SC-003) | `ip route del default dev tun23-0` at 19:30:45; `recovered from DefaultRouteMissing` at 19:31:00 (15s) | pass |
| Current P-CSCF black-holed (SC-005) | `10.56.158.176/32` via the veth peer at 19:31:08. Agent: `P-CSCF marked unavailable ... cause=gm-keepalive` and `selected 56.2.179.162 reason=failover` at 19:33:48, `registered` at 19:33:49 (161s; bound 193s). No supervisor recovery followed the route repair | pass |

The route repair at 19:31:00 restarts the IMS agent (known behaviour), which is why the agent log shows a fresh start at 19:31:06 before the black-hole took effect.

## Notes

A first flag-on attempt at 18:09 was invalid and is discarded: the black-hole route's next hop was the veth peer's link-local address from the previous container, which changes when the container is recreated (the neighbour showed FAILED and the kernel fell back to the tunnel route, so the probe kept succeeding). Re-read the peer address after every recreate.

The comparison with the pre-change build (the original T021 wording) was not run; the flag-on timing is compared with the flag-off run above instead.

## Open

T023 (soak) is running; see below once complete.
