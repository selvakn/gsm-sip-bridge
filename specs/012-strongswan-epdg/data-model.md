# Data Model: strongSwan-Based ePDG Tunnel (Option 2)

No new persistent storage (no SQLite tables, no config-file schema changes to the Rust
binary's `config.toml`). All state below is in-process or ephemeral-file, consistent with how
011 treats tunnel/operational state.

## Entities

### Tunnel Engine (deploy-time selection)

| Field | Type | Notes |
|---|---|---|
| `TUNNEL_ENGINE` | env var: `swu` \| `strongswan` | Read only by `entrypoint.sh`. Default `swu` during the proving period (flips to `strongswan` after SC-001..004 pass). Invalid values fail startup loudly. |

The Rust binary is deliberately engine-agnostic — no `VowifiConfig` change.

### VoWiFi Line (runtime, engine-produced)

Same entity as 011, produced now by either engine:

| Attribute | Produced by | Consumed by |
|---|---|---|
| Carrier identity (MCC/MNC) | `.env` | entrypoint (FQDN, NAI realm), swanctl template |
| IMSI | SIM via `vowifi-imsi` (AT+CIMI) | swanctl template NAI: `0<IMSI>@nai.epc.mnc<MNC>.mcc<MCC>.3gppnetwork.org` |
| Inner address(es) | ePDG via IKE config payload | `ims.updown` → address on the XFRM interface in netns `ims` |
| P-CSCF address(es) | ePDG via P-CSCF config attribute | entrypoint → `/tmp/pcscf` (IPv4 preferred) → `vowifi-ims-agent` (unchanged reader) |
| Tunnel interface | entrypoint-created XFRM iface, `if_id 23`, netns `ims` | agents (routes), charon (SA binding via `if_id`) |

### Bridge (vpcd virtual card) session state — `vowifi-usim-bridge`

In-process state machine, one vpcd TCP connection:

```
Disconnected → Connected(unpowered) → Powered(serial held) → Connected(unpowered) → …
```

| State | Serial port | Behavior |
|---|---|---|
| `Disconnected` | closed | Reconnect to vpcd with backoff. |
| `Connected` (unpowered) | closed | Answer ATR requests with the canned ATR; Power On → try to open the modem port (retry/backoff on busy — the port is shared, TIOCEXCL-exclusive). |
| `Powered` | open, exclusive | Forward APDUs via AT+CSIM with normalization (see contract); cache last response for GET RESPONSE emulation; Reset → replay USIM select state; Power Off / vpcd disconnect → release port. |

Per-powered-session cached data: discovered USIM AID (from EF_DIR, reusing `usim.rs`), last
response bytes + SW (for `61xx`/GET RESPONSE emulation).

### Cross-process artifacts (files)

| Path | Writer | Reader | Format / lifecycle |
|---|---|---|---|
| `/tmp/pcscf` | entrypoint (both engines) | `vowifi-ims-agent` | Single IP address + newline (unchanged from 011). Rewritten on every (re)establishment. |
| `/tmp/charon.log` | charon filelog | entrypoint (readiness + P-CSCF regex) | Reset on engine start; append during run. |
| `/etc/swanctl/conf.d/epdg.conf` | entrypoint (rendered from template) | swanctl/charon | Rendered at startup from IMSI + MCC/MNC + ePDG address; never edited mid-run. |
| `/tmp/swu.log` | SWu dialer (legacy engine only) | entrypoint | Unchanged from today. |

## State transitions that matter (FR-005)

Reconnect/rekey under strongSwan mutates **only**: IKE/CHILD SAs (charon-internal), the inner
address on the XFRM interface (updown script), and `/tmp/pcscf` contents. The netns `ims`, the
XFRM interface, the veth pair, and both agent processes persist — this is the invariant the
tunnel-engine contract pins and the 24 h soak verifies.

## Validation rules

- IMSI: 6–15 digits from AT+CIMI (existing `query_imsi()` validation); refusal to render the
  swanctl template without one (unless `IMSI` env override set).
- P-CSCF extraction: prefer first IPv4; accept IPv6 only when no IPv4 offered (matches
  current agent preference); never write a partial/empty `/tmp/pcscf`.
- `TUNNEL_ENGINE`: exact-match validation, unknown value → fatal startup error (no silent
  default), so a typo can't silently deploy the wrong engine.
