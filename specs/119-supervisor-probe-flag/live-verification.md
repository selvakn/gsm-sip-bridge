# Live verification: supervisor P-CSCF probe flag (#120)

Run 2026-10-21 on the local Vodafone rig (single P-CSCF, IPv6), image `gsm-sip-bridge:probe119-8e5d11c-amd64` built from `git archive HEAD`, default config (`supervisor_pcscf_probe` unset). Times are UTC.

## Scope

Run: startup line, a 5-minute connect count, default-route deletion, black-holing the sole P-CSCF.
Not run: the one-hour capture (SC-001 is shown by a 5-minute count only), the flag-on repeat (T021), the Jio Pi runs (T022), the 4.5h soaks (T023). SC-002, SC-005 and SC-006 are therefore unverified.

## Results

| Step | Evidence | Result |
|---|---|---|
| Startup line (US1 AS2) | `supervisor P-CSCF probe off ([vowifi].supervisor_pcscf_probe = false); route loss, an exhausted P-CSCF pool and a dead peer are repaired from their own signals`; the idle-keepalive line reads `...the supervisor's own P-CSCF probe is [vowifi].supervisor_pcscf_probe = false` | pass |
| No probe connects (SC-001, short) | TCP `ActiveOpens` in the line's netns (`/proc/net/snmp`): 2 at 17:54:44, 2 at 17:59:47. The old probe adds about 10 in 5 minutes | pass (5 min, not 1 h; counts all TCP opens, not only :5060) |
| Default route deleted (SC-003) | `ip -6 route del default dev tun23-0` at 17:59:52; `recovered from DefaultRouteMissing` at 18:00:16, route back by 18:00:17 (about 25s, within one 30s tick) | pass |
| Sole P-CSCF black-holed (SC-004) | `/128` route to the veth peer's link-local in the netns at 18:00:35. `the ePDG has been silent for 30s; checking it is alive (DPD sent) before PcscfPoolExhausted` at 18:03:16, `recovered from PcscfPoolExhausted` at 18:03:46 (191s), line `Registered` again at 18:03:59 (204s). Bound is 315s | pass |

The underlay guard's liveness check added about 30s before the rebuild, as designed (specs/100).

## Open

T021 (flag on, same black-hole), T022 (Jio Pi), T023 (soak) remain.
