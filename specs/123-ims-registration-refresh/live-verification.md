# Live verification log: IMS registration refresh (#163)

One row per run. Synthetic identifiers only: no real numbers, IMSI or ICCID.
Times are UTC. "Jio" is VoWiFi on the Raspberry Pi line; "Vodafone" is the local rig's modem line.

Baseline: `make format && make lint && make test` green on `2ad3241` (2026-10-27).

| Date | Line | Build | Check | Outcome |
|---|---|---|---|---|
| 2026-10-27 | Jio, Vodafone | `5e9527c` | Schedule (SC-001) | `next_refresh` is exactly `registered_at` + 3000 s on both |
| 2026-10-27 | Jio to Vodafone | `5e9527c` | 20 s call (dry run) | both ways, 1000 sent / 999 received, 0 loss |
| 2026-10-27 | Vodafone (IPv6, TCP) | `5e9527c` | First in-place refresh | accepted, 0.3 s, granted 3600 s, `cseq=3` |
| 2026-10-27 | Jio | `5e9527c` | First in-place refresh | **failed**: no answer, then `Broken pipe`; recovered by P-CSCF failover and a full re-registration; a healthy P-CSCF was marked unavailable for ~5 min |
| 2026-10-27 | Jio | `425cbba` | In-place refresh with a new protected client port | **failed**: `TCP send failed: Broken pipe` before anything reached the wire. The client-port change was correct but not the cause. |
| 2026-10-27 | Jio | `425cbba` | Outgoing call at +32 min | **failed**: `carrier_timeout`. The agent log shows `Gm client connection reader stopped: connection closed by peer` 13 minutes after registration. |
| 2026-10-27 | Vodafone | `e517250` | In-place refresh | accepted, 0.27 s |
| 2026-10-27 | Jio | `e517250` | In-place refresh onto a closed connection | `Broken pipe`, then `the connection did not carry the refresh; rebuilding it and sending it again`, accepted 0.33 s later, `refresh_mode: in-place`, 0 rejections, no failover |
| 2026-10-27 | Jio to Vodafone | `cd53789` | 21 min call spanning both refreshes (SC-002, SC-004) | stayed up both ways, 62800 sent / 49496 received, 5 lost (0.0%), jitter 2.1 ms. Vodafone: `refreshing the registration in place in_call=true`, accepted 0.16 s. Jio: connection already closed by the network, rebuilt and resent, accepted 0.3 s later. No failover, no re-registration. |

## Findings

- **The Jio P-CSCF closes the idle Gm client connection within minutes.** Observed: closed 13 minutes after registration, with no traffic. The full renewal this change replaces opened a new connection every time and hid it. Since #164 made the periodic OPTIONS keepalive opt-in, nothing noticed the closed connection, so an outgoing call placed after that point got no answer. This affects any outgoing call on Jio, not only the refresh.
- Build `cd53789` keeps the reader's thread handle, so the connection closing is noticed when the reader ends, and rebuilds the connection before a refresh or an idle probe uses it.
- **Test limitation:** the Pi's registrar has one binding per account. `baresip-amr` on the Pi registers as `1002`, so a test-only account `1003` was added to the Pi's config for siptest (backups: `config.toml.bak-pre-1003`, `.env.bak-pre-1003`). It is removed when testing ends.

## Still to run

- A 12 h idle soak (SC-003).
- Vodafone check of the fallback after a forced rejection, and the two-P-CSCF failover (SC-006).
- UDP transport and background re-authentication: Slice C, not started.
