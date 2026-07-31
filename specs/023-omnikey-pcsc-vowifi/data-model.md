# Data Model: PC/SC Card-Reader-Backed VoWiFi Lines

## Entities

### VoWiFi Line Override (config input) — `VowifiLineOverride`

Existing struct (`gsm-sip-bridge/src/config/mod.rs:644-664`), extended with
one field:

| Field | Type | Notes |
|---|---|---|
| `modem_serial` | `Option<String>` | Existing — modem matcher. Mutually exclusive with `pcsc_reader = true`. |
| `modem_port` | `Option<String>` | Existing — modem matcher. Mutually exclusive with `pcsc_reader = true`. |
| `mcc` | `Option<String>` | Existing. Optional — *was* mandatory when `pcsc_reader = true`; now derived from the card's `EF_IMSI` (see supersession note below). |
| `mnc` | `Option<String>` | Existing. Optional — *was* mandatory when `pcsc_reader = true`; now derived from the card's `EF_AD` MNC-length byte. |
| `imsi_override` | `Option<String>` | Existing. **Mandatory** when `pcsc_reader = true` — it is the reader-to-line binding key, needed before any card session exists (not because the IMSI is unreadable). |
| `imei_override` | `Option<String>` | Existing. Not applicable to a pcsc line (no modem/IMEI) — ignored if set alongside `pcsc_reader = true`. |
| `pcsc_reader` | `bool` (**new**) | Default `false`. When `true`, this entry describes a card-reader-backed line rather than a modem matcher. |

**Validation rule** (new): if `pcsc_reader == true`, `imsi_override` must be
`Some` and non-empty, or config load fails with an error naming the specific
line — no silent partial line.

> **Superseded (post-v8.1.0):** this rule originally also required `mcc`/`mnc`,
> on the mistaken grounds that there was "no modem to derive them from". Both
> derive from files on the card (`EF_IMSI` for the digits, `EF_AD` byte 4 for
> the MNC length) and are now read over `ApduTransport` by
> `plmn::derive_plmn_from_card` / `vowifi-plmn --pcsc-imsi`. Only the legacy
> `AT+COPS` fallback is modem-only. See the Unreleased section of
> `RELEASE_NOTES.md`.

**Validation rule** (new): if `pcsc_reader == true` and `[vowifi].tunnel_engine
!= "strongswan"`, `supervise` fails at startup naming the incompatible line
(spec FR-008) rather than starting.

### VoWiFi Line (resolved, runtime) — `ResolvedLine` / `LineResolutionEntry`

Existing structs (`gsm-sip-bridge/src/vowifi/discovery.rs:110-118,269-284`),
each extended with one field:

| Field | Type | Notes |
|---|---|---|
| `index` | `u32` | Existing — shared counter across modem and pcsc lines (spec FR-006). |
| `card_id` | `String` | Existing. For a pcsc line, a synthetic id (e.g. `pcsc0`) rather than a derived USB modem id. |
| `modem_port` | `String` (`ResolvedLine` uses `PathBuf` today; see Design Note below) | **Empty string** for a pcsc line — no modem device. |
| `pcsc_reader` | `bool` (**new**) | `true` for a card-reader-backed line. Drives orchestration branching (skip modem checks, skip `vowifi-usim-bridge`). |
| `mcc` / `mnc` / `imsi_override` (on `ResolvedLine`) | `String` / `Option<String>` | Existing. `imsi_override` is always populated for a pcsc line (mandatory override). `mcc`/`mnc` are populated only when the override sets them; left unset they stay **empty strings** — the same "auto-derive" sentinel a modem line uses, resolved later from the card's `EF_IMSI`/`EF_AD` (see the supersession note above). |
| everything else (netns, veth addrs, control_port, vpcd_port, strongswan_if_id/tun_iface, pcscf_source_path, `config: VowifiConfig`) | unchanged types | Existing per-index derivation (`resolve_one_line`'s pure-function-of-`index` block, `discovery.rs:218-233`) — reused as-is for pcsc lines via a sibling `resolve_one_pcsc_line`. |

**Design note**: `ResolvedLine.modem_port` is typed `PathBuf` today
(`discovery.rs:113`), which doesn't have a natural "empty" representation as
clean as `LineResolutionEntry`'s `String`. Plan: keep `PathBuf` but use
`PathBuf::new()` (empty path) for a pcsc line, and gate every consumer
(the modem-existence check, `modem-ims` reconcile call, `vowifi-usim-bridge`
spawn) behind the new `pcsc_reader` flag rather than behind "is this path
non-empty" — the flag is the single source of truth, the empty path is just
a satisfiable placeholder for the field's existing type.

### Relationships

- One `VowifiLineOverride` with `pcsc_reader = true` → exactly one
  `ResolvedLine`/`LineResolutionEntry` with `pcsc_reader = true` (1:1,
  mirroring how a modem override maps to exactly one modem-derived line).
- A `LineResolution`'s `lines: Vec<LineResolutionEntry>` contains modem- and
  pcsc-backed entries interleaved by `index`, both bounded together by
  `[vowifi].max_lines` (spec FR-006) — `resolve_lines` appends pcsc-derived
  lines after modem-derived ones, continuing the same index counter.
- A `pcsc_reader` line has no `RoleAssignment`/`ProbedModem` origin at all —
  it bypasses USB modem scanning entirely, sourced directly from
  `base.line_overrides`.

### State transitions (per line, unchanged mechanism)

Reuses the existing per-line state machine (`line_supervisor.rs`):
`Establishing → {StillEstablishing loop} → Up → SteadyState → {Recovered on
ProcessDied/ViciBroken, restart in place} → Up`, plus the existing
`FailedLine { card_id, reason }` terminal-at-resolution-time state for a line
that never got created (e.g. `max_lines` overflow, missing mandatory
override fields). No new states are introduced — a pcsc line's card/reader
being unreachable surfaces as a failed EAP-AKA attempt inside the existing
`Establishing`/`SteadyState` machinery (research.md §4), not as a new state.

## Validation Summary

| Rule | Enforced where | Failure behavior |
|---|---|---|
| `pcsc_reader` line has `imsi_override` | Config load/validation | Startup error naming the line index/position |
| `pcsc_reader` line's `mcc`/`mnc` — **not** required (superseded; see note above) | Derived at startup from the card's `EF_IMSI`/`EF_AD` when unset | A card whose `EF_AD` omits the MNC-length byte fails that line at startup, with an error saying to set `mcc`/`mnc` explicitly (no `AT+COPS` fallback exists for a reader) |
| `pcsc_reader` line + `tunnel_engine = "swu"` | `supervise` startup, before any line thread spawns | Startup error naming the incompatible line; process exits non-zero |
| Combined modem + pcsc line count vs `max_lines` | `resolve_lines` (existing overflow logic, extended) | Overflow lines reported in `LineTableResult.failed` with reason `max_lines_exceeded`, same as today |
