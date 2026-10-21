# Data Model: Supervisor P-CSCF probe flag

No stored data. The feature adds one setting and one field of per-line in-memory state.

| Name | Where | Type | Default | Notes |
|---|---|---|---|---|
| `supervisor_pcscf_probe` | `[vowifi]` config, `VowifiConfig`, `RawVowifiConfig` | bool | `false` | Global. Read once at startup. |
| `SteadyTracking.probe_disabled` | per-line loop state | bool | `false` (derived `Default`) | Inverse of the setting. `false` = probe runs = pre-change behaviour. Set where each loop builds its tracking. |

## State behaviour

| `probe_disabled` | Block behaviour | `pcscf_via_sibling` | `pcscf_unreachable` streak | Tick outcome |
|---|---|---|---|---|
| `false` | unchanged | as today | as today | as today |
| `true` | skipped, no connect | `None` | reset | `StillUp` unless an earlier check returned |

Earlier checks that can still return in a tick regardless of the flag: process alive, steady-state health (IKE_SA, CHILD_SA, rekey, half-open), default-route repair, pool-exhaustion rebuild.
