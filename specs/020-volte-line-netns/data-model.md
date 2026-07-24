# Phase 1 Data Model: Per-Line Network Isolation for VoLTE

This feature extends two existing entities (`volte::discovery`'s resolved line, and the on-disk
`VolteLineManifest`) and adds no new persistent storage. It does not touch `ProbedModem` or
`RoleAssignment` (specs/013/018) — those already produce the modem/line list this feature isolates.

## `ResolvedVolteLine` (extended)

The existing per-line entity `volte::discovery::resolve_volte_lines` produces (specs/018). Gains a
`config` sub-struct's worth of isolation fields, derived the same way the rest of the line's
resources already are — as a pure function of `index` — mirroring
`vowifi::discovery::ResolvedLine.config: VowifiConfig`.

| Field | Type | Notes |
|---|---|---|
| `index` | `u32` | Unchanged (existing). Position in the line table; every field below is `f(base, index)`. |
| `card_id` | `String` | Unchanged (existing). |
| `settings` | `VolteSettings` | Unchanged (existing) — modem port, cid, apn, pcscf, `restore_cid_path`. |
| `sip_leg_port` / `control_port` / `status_port` | `u16` | Unchanged (existing, from specs/018). |
| `netns` | `String` | **New.** This line's namespace name. Index 0: unindexed base default (e.g. `"volte"`, back-compat identity — mirrors `vowifi::discovery`'s `index == 0` rule, spec FR-020-equivalent). Index > 0: `format!("{}{}", base.netns, index)` — same derivation shape as `vowifi::discovery::resolve_one_line` (`discovery.rs:227`), on a distinct `volte`-prefixed base so it can never equal a same-index VoWiFi namespace (closes FR-004a; asserted by a unit test, not left to convention). |
| `veth_carrier_iface` / `veth_telephony_iface` | `String` | **New.** The veth pair's two ends — one visible inside `netns` (the carrier-agent side), one in the default namespace (the telephony half's side). Named/derived exactly like `vowifi::discovery`'s `veth_ims_iface`/`veth_sip_iface`. |
| `veth_carrier_addr` / `veth_telephony_addr` | `String` | **New.** The `/30` address pair connecting the two veth ends — same derivation as `vowifi::discovery`'s `shift_ipv4(base, 4*index)` stepping (`discovery.rs:232-238`), on VoLTE's own base block (distinct from VoWiFi's, for the same collision-avoidance reason as `netns`). These become the real `veth_local_addr`/`veth_peer_addr` passed to `crate::vowifi::run_telephony_side` in place of today's `LOOPBACK` (research.md R2/R4). |

Downstream code — `ims::agent::serve_inbound`, `volte::netcfg`, `volte::pdn` — takes this line's
settings exactly as it does today and needs no awareness that it is running inside a namespace: the
awareness lives entirely in *which process it's running as* (R3), not in any field it reads.

## `VolteLineManifest` / `VolteLineManifestEntry` (extended)

The existing JSON artifact `volte::bridge::write_manifest` writes (`super::discovery::manifest_path()`)
so `docker/entrypoint.sh`'s cleanup and `volte-status` agree with the running service on what exists.
Gains the same new fields as `ResolvedVolteLine`, for the same reason `iface`/`restore_cid_path`
are already there: cleanup needs them to run the right teardown for the right line without
re-deriving anything (research.md R6 — cleanup must run `netcfg::teardown` *inside* the line's
namespace, before deleting it, which means the cleanup trap must know that namespace's name without
re-invoking discovery).

| Field | Type | Notes |
|---|---|---|
| *(all existing fields)* | — | Unchanged: `index`, `card_id`, `modem_port`, `cid`, `iface`, `restore_cid_path`, `status_port`, `control_port`, `sip_leg_port`. |
| `netns` | `String` | **New.** Read by `entrypoint.sh`'s cleanup trap to run this line's teardown via `ip netns exec $netns` before deleting the namespace. |

## `VolteConfig` (extended, `config/mod.rs`)

Gains the same shape of new "base" fields `VowifiConfig` already carries for its own netns/veth
derivation, defaulted so an unconfigured, single-line deployment resolves to today's externally
observable behavior with a namespace name and veth pair it never had to think about before (FR-005).

| Field | Type | Default | Notes |
|---|---|---|---|
| `netns` | `String` | `"volte"` | Base namespace name; distinct from `VowifiConfig`'s `"ims"` default (FR-004a). Not expected to be operator-tuned — exposed as a config field only because every other per-line-derived base in this codebase (`vowifi::netns`, `vowifi::veth_sip_iface`, ...) is one, for consistency and testability, not because an operator has a reason to change it. |
| `veth_carrier_iface` | `String` | e.g. `"veth-volte-ims"` | Base name for the netns-side veth end. |
| `veth_telephony_iface` | `String` | e.g. `"veth-volte-sip"` | Base name for the default-namespace-side veth end. |
| `veth_carrier_addr` / `veth_telephony_addr` | `String` | A `/30` block distinct from VoWiFi's default block | Base addresses; per-line values are `shift_ipv4`-stepped from these, identical mechanism to `vowifi::discovery`. |

## Relationships

```text
VolteConfig (base, one per container)
      │  resolve_volte_lines(modems, base) — pure function, unit-tested table-driven
      ▼
ResolvedVolteLine × N (one per discovered SIM-ready modem, index-ordered)
      │  write_manifest()
      ▼
VolteLineManifest (JSON, /run or /tmp — transient, regenerated every startup)
      │  read by: docker/entrypoint.sh (cleanup), `volte-status`, `volte-cleanup`
      ▼
docker/entrypoint.sh per-line loop:
   1. move `iface` into `netns` (idempotent — R5)
   2. create veth pair (`veth_carrier_iface` in netns, `veth_telephony_iface` in default ns)
   3. `ip netns exec $netns volte-carrier-agent --line $idx` (supervised)
   ...once every line's veth exists...
   4. `volte-bridge` (Agent B only — shared telephony half, default namespace)
```

No new database/store schema. No change to the `sms`/`calls` SQLite tables — this feature is
entirely about which physical connection a line's packets use, not about what is recorded once they
arrive.
