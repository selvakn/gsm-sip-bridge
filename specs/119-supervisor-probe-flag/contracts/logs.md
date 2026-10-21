# Contract: logs

One startup line per run, next to the idle-keepalive and DPD lines (`orchestrate.rs` ~220-245), at `info`:

| State | Message | Fields |
|---|---|---|
| off (default) | `supervisor P-CSCF probe off ([vowifi].supervisor_pcscf_probe = false); route loss, an exhausted P-CSCF pool and a dead peer are repaired from their own signals` | `supervisor_pcscf_probe = false` |
| on | `supervisor P-CSCF probe on: TCP connect to each line's P-CSCF :5060 every supervisor tick ([vowifi].supervisor_pcscf_probe = true)` | `supervisor_pcscf_probe = true` |

The existing idle-keepalive off line currently ends "P-CSCF reachability is probed by the line supervisor only". It becomes: "...; the supervisor's own probe is `[vowifi].supervisor_pcscf_probe`" with the flag's current state named, so it never claims a probe that is not running.

No per-tick log is added. The existing `[supervise] line N: P-CSCF X unreachable; pool member Y answers` line and the `pcscf_unreachable` recovery stay as they are and simply do not occur with the probe off.
