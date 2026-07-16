# Phase 0 Research: Multi-Card VoWiFi

## 1. Discovery: recognizing VoWiFi-only modems and probing for the AT port

**Decision**: Extend `modules::discovery` with a single shared inventory scan that (a) matches
against the existing `KNOWN_DEVICES` vendor/product table (unchanged — still the boundary of "a
modem this project recognizes"), (b) for every match, *probes* each of that USB device's
`ttyUSB*` child interfaces with a live `AT\r` → `OK` round trip instead of trusting a fixed
`bInterfaceNumber`, and (c) no longer skips a match just because it exposes no ALSA device.
`KnownDevice.at_interface_number: Option<&str>` is deleted; there is nothing left to look up.

**Rationale**: FR-002 explicitly requires probing over an assumed fixed interface number ("that
number varies by model and firmware"). FR-003 requires audio-less models to be recognized at all
— today's scanner's whole reason for skipping the EC200 was audio absence, hard-coded via
`at_interface_number: None`. Probing subsumes the old table: a working AT port is now a discovered
fact, not a per-model constant, so audio-less and audio-capable modems go through one code path
that differs only in whether an `hw:N,0` ALSA device also happens to exist alongside the AT port.

**Alternatives considered**:
- *Keep two scanners (today's CS one plus a new VoWiFi-only one)*: rejected — duplicates the USB
  walk and the SIM-identity read, and is exactly the "modem claimed by both subsystems" hazard
  the spec's edge cases warn about if the two scanners ever disagree about the same device.
- *Assume a per-model AT interface table but add EC200's number instead of `None`*: rejected —
  still breaks on firmware/model drift (acceptance scenario 4 requires probing to survive that).

## 2. Reading SIM identity during discovery

**Decision**: After a modem's AT port is found, discovery opens it with the existing
`AtCommander` (already used by `modules/usim.rs` and `vowifi/imsi.rs`/`vowifi/plmn.rs`) and runs
`AT+CIMI` (IMSI) plus a SIM-readiness check (`AT+CPIN?` == `READY`). A modem with no SIM, a
locked SIM, or no IMSI response is reported with a reason and excluded from the line table (FR-006,
edge case "PIN-locked or not yet ready").

**Rationale**: The line table's identity (`Key Entities: SIM`) and MCC/MNC auto-derivation
(feature 012's `vowifi-plmn`) already do this exact probe for the single-line case; reusing the
same `AtCommander` calls means discovery and `vowifi-plmn`/`vowifi-imsi` never disagree about
whether a SIM is usable.

**Alternatives considered**: Deferring the SIM check to each line's own startup (today's
behavior) — rejected because it can't produce SC-001's "reports each modem's AT port and SIM" at
discovery time, and would surface a bad SIM as a mid-startup per-line failure instead of an
upfront discovery report.

## 3. Role assignment and the both-subsystems-claim-one-modem race

**Decision**: Discovery + role assignment runs exactly **once**, in a new one-shot CLI
subcommand (`gsm-sip-bridge discover`), before either the circuit-switched daemon or any VoWiFi
process opens a single serial port. Its output is a small resolution artifact
(`/tmp/gsm-sip-bridge-lines.json` by default, path overridable) consumed by both:
- `docker/entrypoint.sh` reads it to drive the per-line VoWiFi loop (env-exported, same style as
  today's `config vowifi-shell-env`).
- The circuit-switched daemon reads the same file at startup (path via
  `GSM_SIP_BRIDGE_LINES_FILE` env var, defaulting to the same path) and excludes every modem it
  names from its own `scan_modules()` pool.

Default assignment (FR-008): a matched modem with no ALSA audio device is VoWiFi; one with audio
is circuit-switched. An operator override (FR-009, `[[vowifi.line]]` naming explicit modem serial
numbers or `[vowifi].modem_port`) takes precedence over the default and is honored even against an
audio-capable modem (the override always wins, matching FR-009's plain "override the default").
When VoWiFi is disabled (`[vowifi].enabled = false`), the `discover` step still runs the shared
scan (needed for the CS side's own card table) but performs no VoWiFi role assignment or line
resolution at all, and no resolution file is required — this is unchanged from today's per-process
`scan_modules()` behavior for a fleet with VoWiFi off.

**Rationale**: `entrypoint.sh` already starts the CS daemon supervisor loop in the background and
then immediately moves on to VoWiFi preflight in the same script — i.e., today's two subsystems
already start *concurrently* as separate OS processes. Auto-discovery adds a real hazard that
didn't exist before: two independent processes each running their own USB scan/AT-probe over the
same candidate ttyUSB device at the same moment corrupts both probes (spec's own "modem claimed by
both subsystems" edge case). A single upfront resolution, written once and read twice, removes the
race entirely rather than adding locking around concurrent serial access.

**Alternatives considered**:
- *File lock / mutex around discovery so each process can still scan independently*: rejected —
  more moving parts than a single one-shot resolution step, and a flock around a serial-port AT
  probe is still fragile if a probe partially writes before losing the lock.
- *Put role assignment inside the CS daemon's own startup and have entrypoint.sh shell out to ask
  it what it decided*: rejected — the CS daemon isn't running yet at the point `entrypoint.sh`
  needs the VoWiFi line table (it's started in the background in parallel), so this just
  reintroduces the ordering problem under another name.

## 4. PC/SC layer for N lines: one shared pcscd, N vpcd slots, N charon processes

**Decision**: One `charon` process **per line** (feature 012's single-line recipe replicated N
times), but a **single shared `pcscd`** serving all of them through one `vpcd` reader with **N
slots** — one listening TCP port per slot (35963, 35964, …), one `vowifi-usim-bridge` per line
connecting to its slot's port. Each `charon`'s `eap-sim-pcsc` plugin scans every slot and selects
that line's SIM by IMSI.

**This corrects an earlier, wrong version of this decision** (which called for one `pcscd` *per
line*, isolated via `PCSCLITE_CSOCK_NAME`). Two facts, verified against the actual pinned sources,
overturned it:

1. **`eap-sim-pcsc` already disambiguates readers by IMSI.** In the fork's
   `eap_sim_pcsc_card.c` (`get_triplet`/`get_quintuplet`), the plugin calls `SCardListReaders`,
   loops over **every** reader, reads each card's IMSI, and does `strstr(full_nai, imsi)` —
   logging *"Not the SIM we're looking for"* and `continue`-ing until it finds the card matching
   the identity being authenticated. So a single `pcscd` exposing N SIMs is exactly what the
   plugin is built for; the "no way to pick among several readers" premise was simply false.
2. **`PCSCLITE_CSOCK_NAME` is not a runtime override in modern pcsc-lite.** In the Alpine 3.21
   `pcsc-lite`, the daemon's control-socket path is a compile-time macro with **no `getenv`**
   anywhere in the source — so per-line `pcscd` instances could never coexist (they'd collide on
   `/run/pcscd/pcscd.comm` + the pidfile) regardless. "N pcscd instances" was a dead end, not
   just imperfect.

vpcd's **native multi-slot mode** is the enabler: `--enable-vpcdslots=N` (default 2; set to 8 in
`docker/Dockerfile` to match `[vowifi].max_lines`) makes one vpcd reader open one socket per slot
on incrementing ports, with `ctx[slot]` indexed per slot (no shared global state to collide —
this is vpcd's own intended multi-card design). Slots with no `vowifi-usim-bridge` connected
report no card and are harmlessly skipped by the IMSI scan.

`charon` stays **per line** (not collapsed into one charon with N connections) because that keeps
each line identical to feature 012's proven single-charon unit: its own `strongswan.conf`
(line-indexed `vici` socket + `filelog` path), its own single-connection swanctl config driven via
`swanctl --uri unix:///var/run/charon-N.vici`, and its own clean per-line P-CSCF extraction and
reliability supervision — with per-line failure isolation (FR-013) for the charon/tunnel layer.
The shared `pcscd` is the one component all lines depend on; it is supervised, and its loss breaks
SIM auth for every line until it restarts (an acceptable single point for the PC/SC layer, since
pcsc-lite structurally cannot be run per-line here). All charon instances run in the container's
default netns; each XFRM tunnel interface is created there and moved into that line's own netns,
exactly as today's single `ensure_epdg_interface` does.

**One live-verification point** (flagged in `quickstart.md`): that `pcscd` enumerates the vpcd
reader's N slots as N distinct `SCardListReaders` entries so the per-line IMSI scan sees each SIM.
This is the documented intent of `--enable-vpcdslots` ("vpcd will open one socket for each slot")
and pcsc-lite's standard multi-slot handling, but it is the one piece not confirmable without
running pcscd on hardware.

**Alternatives considered**:
- *One shared charon with N connections* (instead of N charon): also correct — the same IMSI
  match disambiguates per connection — but a larger rewrite of the proven per-charon supervision/
  P-CSCF-extraction, and further from 012's unit. Rejected as unnecessary churn.
- *Per-line mount namespaces* wrapping each `(pcscd + charon)` pair so the compile-time pcsc-lite
  paths isolate naturally: maximally faithful to "replicate the proven unit," but heavy
  orchestration. Held in reserve only if the slot-enumeration verification above fails on hardware.

## 5. Deriving per-line resources from line-table position

**Decision**: Every isolated resource `VowifiConfig` field becomes a **per-line value** derived
by a pure function of `(base_value, line_index)`, computed once when the line table is resolved,
not hand-configured per line:

| Resource | Single-line default | Multi-line derivation (index `i`, 0-based) |
|---|---|---|
| `netns` | `ims` (unchanged) | `ims{i}` |
| `strongswan_tun_iface` | `tun23` (unchanged) | `tun23-{i}` |
| `strongswan_if_id` | base value (unchanged) | `base + i` |
| `veth_sip_iface` / `veth_ims_iface` | unchanged names | `{name}{i}` |
| `veth_local_addr` / `veth_peer_addr` | unchanged `/30` pair | stepped by 4 per line, e.g. `10.90.4i+1`/`10.90.4i+2` |
| `control_port` | unchanged | **unchanged** — distinguished by `veth_peer_addr`, which already differs per line, not by port |
| `vpcd_port` | unchanged | `base + i` (line `i` connects its usim-bridge to the shared pcscd's vpcd **slot** `i`, item 4) |
| `pcscd` control-socket path | shared, not per-line | one shared pcscd on the default socket — pcsc-lite has no runtime socket override (item 4) |
| `charon` vici socket / log paths | unchanged defaults | `/var/run/charon-{i}.vici`, `/tmp/charon-{i}.log` |
| `pcscf_source_path` | `/tmp/pcscf` | `/tmp/pcscf-{i}` |
| `mcc`/`mnc`/`imsi_override`/`modem_port` | from config or auto-derive | **not derived** — read from that line's own SIM/config entry, never shared |

When exactly one line resolves (today's only case, and still the common case per FR-020), `i = 0`
derivations are defined to equal the historical unindexed defaults (`ims`, `tun23`, `/tmp/pcscf`,
etc.) exactly — so an existing single-SIM deployment's on-disk paths, interface names, and ports
don't change at all. This is what makes FR-020 a property of the derivation formula rather than a
separate code path.

**Rationale**: Matches the spec's own "Line Table" key entity description ("each line's resources
are a function of its position in this table, so they are stable across restarts and never
collide") and mirrors how feature 004 derives per-card resources (audio ring buffers, slot ids)
from discovery order rather than hand-typed per-card config.

**Alternatives considered**: A `[[vowifi.line]]` array where the operator hand-types every
resource path per line — rejected as needless config surface for values that are mechanically
derivable and where collision-by-typo is exactly the failure mode FR-011 rules out.

## 6. Agent process topology

**Decision**:
- **Agent A** (`vowifi-ims-agent`, the tunnel/IMS-facing half) stays **one OS process per line**,
  each launched inside that line's own netns (`ip netns exec ims{i} ... vowifi-ims-agent --line
  {i}`), exactly like today's single instance — this needs no new concurrency model, only a
  `--line` selector so it loads the resolved per-line `VowifiConfig` slice instead of the sole
  config today.
- **Agent B** (`vowifi-sip-agent`, the PBX-facing half) stays **one OS process total** — per the
  spec's own assumption ("The bridge presents a single SIP identity to the PBX ... rather than
  registering once per line") — but its control-channel accept loop becomes **N listener threads**
  (one bound to each line's `veth_peer_addr:control_port`), sharing one PJSIP `Endpoint`/`Account`,
  one Discord client, one SMS store handle, and a **per-line** `RecentCalls` map keyed by card id.
  Because each line already has its own veth pair (item 5 above), Agent B learns which line a
  connection came from by *which listener accepted it* — no wire-protocol change to
  `ControlMessage` is needed; the card id is threaded through as a plain function parameter
  captured by that listener thread's closure.

**Rationale**: Agent A's ports (`VETH_SIP_PORT`, `AGENT_A_STATUS_PORT`) are bound inside that
line's own network namespace, so the existing fixed port constants need no per-line change at all
— namespace isolation already gives every line's Agent A its own port space. Reworking Agent B
into N listener threads sharing one PJSIP endpoint is the minimum change that satisfies "one SIP
identity" while still telling lines apart, and avoids inventing a new multiplexed wire protocol
where positional/address attribution already works for free.

**Alternatives considered**: Extending `ControlMessage::IncomingCall` with an explicit `card_id`
field and keeping a single listener — rejected as an unnecessary protocol change when the
per-line veth address already disambiguates for free; kept as a fallback note only if a future
need arises to run Agent A and Agent B N:1 differently than assumed here.

## 7. Line-count bound

**Decision**: `[vowifi].max_lines`, default `8` — same order of magnitude as
`[modules].max_concurrent` (circuit-switched bound, also defaulting to 8). Modems beyond the bound
are reported and skipped (FR-016), in line-table order (stable hardware-serial ordering, item 5).

**Rationale**: Matches the spec's Assumptions ("bounded by the same order of magnitude as the
existing circuit-switched card limit (8)").

## 8. Testing boundary

**Decision**: Discovery (probing, role assignment, line-table resolution, resource derivation) is
fully unit/integration testable without hardware, following `test_discovery.rs`'s existing
`tempfile`-backed fake-sysfs pattern, extended with a fake serial transport for the AT-probe step
(mirroring `at_commander.rs`'s existing justified `MockStream`). Multi-line tunnel establishment,
concurrent call bridging, and 24h soak are operator-run per `quickstart.md`, the same boundary
features 003–012 already draw (spec's own Assumptions section makes this explicit).

**Rationale**: Constitution Principle I (Integration-First Testing) permits mocking only what's
impractical to run in CI — real modem hardware and a real carrier ePDG are exactly that category,
already the precedent set by every prior VoWiFi feature.
