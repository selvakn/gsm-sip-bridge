# Phase 1 Data Model: Multi-Card VoWiFi

## `ProbedModem` (discovery, replaces today's audio-gated `DiscoveredModule` for the shared scan)

Produced by the shared inventory scan (`modules::discovery`), one per USB device matching
`KNOWN_DEVICES`, before any role assignment or SIM read.

| Field | Type | Notes |
|---|---|---|
| `card_id` | `String` | Stable identifier from hardware serial (`derive_module_id`, unchanged scheme, FR-005). |
| `model` | `&'static str` | e.g. `"EC20"`, `"EC200"`. |
| `usb_serial` | `String` | Raw USB `serial` sysfs attribute; empty string is a valid fallback (today's behavior). |
| `at_port` | `Option<PathBuf>` | The `ttyUSB*` device that answered `AT\r` with `OK`, found by probing every interface on the device, not a fixed table lookup (FR-002). `None` means no interface responded. |
| `audio_device` | `Option<String>` | `hw:N,0` if an ALSA card exists alongside this USB device; `None` for VoWiFi-only models (FR-003). |
| `sim_status` | `SimStatus` | See below — populated only when `at_port.is_some()`. |

```rust
enum SimStatus {
    Ready { imsi: String },
    Absent,
    Locked,
    Unreadable(String), // AT command error text, for the discovery report
}
```

A `ProbedModem` fails discovery (FR-006) when `at_port.is_none()` or `sim_status` is not `Ready`;
such modems are reported with a reason and excluded from both pools.

## `RoleAssignment`

The partition of successfully-probed modems into the two subsystems (FR-007/008/009).

| Field | Type | Notes |
|---|---|---|
| `circuit_switched` | `Vec<ProbedModem>` | Audio-capable modems not explicitly overridden to VoWiFi. |
| `vowifi` | `Vec<ProbedModem>` | Audio-less modems, plus any modem explicitly overridden to VoWiFi regardless of audio. |

Default rule: `audio_device.is_some()` → circuit-switched; `audio_device.is_none()` → VoWiFi.
Override rule: if the modem's `usb_serial` (or explicit `modem_port`) appears in
`[[vowifi.line]]`/`[vowifi].modem_port`, it goes to `vowifi` regardless of the default rule. A
modem may not appear in both output vectors (FR-007's exactly-one-role invariant is a property of
this partition function, not a runtime check elsewhere).

## `ResolvedLine`

One entry per VoWiFi line that will actually run, in stable order (by `card_id`, i.e. by hardware
serial — deterministic and independent of USB enumeration order jitter). This is the "Line Table"
key entity from the spec.

| Field | Type | Notes |
|---|---|---|
| `index` | `u32` | Position in the table (0-based). Every derived resource below is `f(base, index)` (research.md item 5). |
| `card_id` | `String` | From the `ProbedModem`; used everywhere in logs/metrics/SMS attribution (FR-017). |
| `modem_port` | `PathBuf` | From `ProbedModem.at_port`, or an explicit config override. |
| `mcc` / `mnc` | `String` | Empty means "derive from this line's own SIM at startup" (FR-012 — never shared across lines). |
| `imsi_override` | `Option<String>` | Rare diagnostic escape hatch, per-line. |
| `config` | `VowifiConfig` | A **fully-derived, line-specific** copy: every field in the "Multi-line derivation" table in research.md item 5 has already been computed for this index. Downstream code (`ims::agent`, `vowifi::run`, `vowifi::usim_bridge`) takes a `&VowifiConfig` exactly as it does today and needs no awareness that it's one of several. |

A `LineTable` is just `Vec<ResolvedLine>`, capped at `[vowifi].max_lines` (FR-016); modems beyond
the cap are reported and skipped, not silently dropped.

## `LineResolution` (the serialized artifact shared across processes)

What the new one-shot `discover` subcommand writes (research.md item 3) so the CS daemon and
`docker/entrypoint.sh` agree on the same partition without re-scanning concurrently.

```jsonc
{
  "circuit_switched_excluded_ports": ["/dev/ttyUSB6"],   // modem_port values claimed by VoWiFi
  "lines": [
    {
      "index": 0, "card_id": "ec20-1A2B3C", "modem_port": "/dev/ttyUSB6",
      "netns": "ims0", "control_port": 7050, "veth_local_addr": "10.90.1.2",
      "veth_peer_addr": "10.90.1.1", "vpcd_port": 7100, "strongswan_if_id": 23,
      "strongswan_tun_iface": "tun23-0", "pcscf_source_path": "/tmp/pcscf-0",
      "mcc": "", "mnc": ""
    }
  ],
  "failed": [
    { "card_id": "ec20-9F9F9F", "reason": "sim_locked" }
  ]
}
```

`circuit_switched_excluded_ports` is the only field the CS daemon reads. `entrypoint.sh` reads
`lines` (via a small `gsm-sip-bridge discover --shell-env` rendering, same style as today's
`config vowifi-shell-env`, but one block per line) to drive its per-line loop. `failed` is
logged, matching FR-006.

## Relationships

```
USB bus
  └─ scan_modules() ─▶ Vec<ProbedModem>
                          │
                          ▼
                    RoleAssignment (FR-007/008/009)
                    ├─ circuit_switched ──▶ CardPool (unchanged, feature 004)
                    └─ vowifi ──▶ LineTable (bounded, FR-016)
                                    │
                                    ▼
                              Vec<ResolvedLine>
                                    │
                     ┌──────────────┼───────────────────┐
                     ▼              ▼                    ▼
              Agent A (×N,      charon/pcscd/vpcd/    Agent B (×1,
              one per netns)    usim-bridge (×N)      N listener threads)
```

## Validation rules

- FR-007: a `usb_serial` may not appear in both `RoleAssignment.circuit_switched` and
  `RoleAssignment.vowifi` — enforced structurally by the partition function (single pass, each
  modem assigned exactly once).
- FR-012: `ResolvedLine.mcc`/`mnc`, when empty, are resolved independently per line at that line's
  own startup (`vowifi-plmn --modem <that line's port>`) — never copied from another line's result.
- FR-016: `LineTable.len() <= max_lines`; the excess, in `card_id` order, are reported and skipped.
- FR-020: with exactly one `ResolvedLine`, every derived field in its `config` equals today's
  unindexed default (research.md item 5's `i = 0` identity).
