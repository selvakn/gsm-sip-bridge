# Implementation Plan: strongSwan-Based ePDG Tunnel (Option 2)

**Branch**: `012-strongswan-epdg` | **Date**: 2026-07-13 | **Spec**: [./spec.md](./spec.md)
**Input**: Feature specification from `specs/012-strongswan-epdg/spec.md`

## Summary

Swap the VoWiFi bridge's tunnel layer from the vendored SWu-IKEv2 Python dialer ("Option 1") to
strongSwan ("Option 2" — the osmocom `strongswan-epdg` fork, `jolly/work` branch), selectable at
deploy time via a `TUNNEL_ENGINE` env var with the old dialer kept as fallback. strongSwan
brings the reliability Option 1 structurally lacks: scheduled IKE rekeying, EAP re-auth, DPD,
MOBIKE, and — via a pre-created XFRM interface (`if_id`-pinned) inside netns `ims` — a namespace
that survives reconnects, so the 011 agents' veth link is never torn down again. The one
structural gap, EAP-AKA against the SIM that lives *inside the EC200U modem*, is closed with a
virtual PC/SC reader: pcscd + vsmartcard's vpcd driver, fronted by a new pure-Rust
`vowifi-usim-bridge` subcommand that services APDUs over vpcd's TCP protocol by forwarding them
to `AT+CSIM` through the existing `AtCommander`/`modules/usim.rs` machinery — which also
absorbs the EC200U quirks (GET RESPONSE auto-chaining, SELECT P2, runtime AID discovery) at the
APDU boundary instead of patching strongSwan. The 011 agents require zero changes: they still
read `/tmp/pcscf` and still find the tunnel inside netns `ims`.

## Technical Context

**Language/Version**: Rust stable (pinned by `rust-toolchain.toml`) for the new bridge/helper
subcommands; C only as *vendored upstream builds* (strongswan-epdg fork, vsmartcard vpcd) — no
C code owned by this repo; bash for `docker/entrypoint.sh` orchestration.

**Primary Dependencies**:
- `strongswan-epdg` fork (`jolly/work`) — IKEv2/EAP-AKA/P-CSCF engine, built from source in a
  new Docker stage (research.md items 1, 8).
- `vsmartcard` (vpcd) — virtual PC/SC reader driver for pcscd, built from source (item 2).
- `pcsc-lite` — pcscd daemon, from Alpine packages (libs are already in the image for pyscard).
- Existing in-repo: `modules/at_commander.rs` + `modules/usim.rs` (AT+CSIM, AID discovery,
  AUTHENTICATE) — reused as the bridge's card backend; `serialport` crate already a dependency.
- **No new Rust crates**: the vpcd protocol is `std::net` TCP with 2-byte length framing.

**Storage**: None new. Tunnel state is charon's; the only cross-process artifacts are the
existing `/tmp/pcscf` file and charon's log file (`data-model.md`).

**Testing**: `cargo test --workspace` via nextest, no hardware/daemon requirements in CI. vpcd
framing + bridge loop tested over real in-process TCP sockets with real APDU byte fixtures; the
modem side uses the existing justified scripted-transport pattern (`MockStream` in
`at_commander.rs`). Live carrier verification (tunnel, rekey soak, end-to-end call) is
operator-run per `quickstart.md` — the same boundary as features 003–011 (research.md item 10).

**Target Platform**: Linux, Alpine/musl container (unchanged image), host-kernel XFRM +
network namespaces; privilege model unchanged from 011 (`privileged: true` compose service).

**Project Type**: Extension of the existing `gsm-sip-bridge` binary (new read-only/bridge CLI
subcommands) + deployment surface (`docker/Dockerfile`, `docker/entrypoint.sh`, strongSwan
config templates). No new crate.

**Performance Goals** (from spec Success Criteria):
- ≥ 24 h unattended tunnel uptime spanning ≥ 1 carrier rekey, zero agent restarts (SC-001).
- Back in service ≤ 90 s after a ≤ 60 s WAN outage ends (SC-002, matches 011's SC-003).
- Inbound call answered/bridged ≤ 5 s even ≥ 12 h after start (SC-003).
- EAP-AKA succeeds on both carriers with only the modem-resident SIM (SC-004).

**Constraints**:
- **Zero new `unsafe` in `gsm-sip-bridge/src`** (enforced by `make lint` →
  `tools/count-unsafe.sh`) — satisfied by design: the bridge is sockets + existing serial code.
- Full pre-commit gate unchanged: `cargo fmt --all`, `make lint`, `cargo test --workspace`.
- 011 agents (`ims/agent.rs`, `vowifi/*`) and the CS-GSM daemon unmodified (FR-007, FR-008);
  shared-module changes (e.g. a new helper in `at_commander.rs`) allowed only if behavior for
  existing callers is untouched.
- With `TUNNEL_ENGINE=swu`, `entrypoint.sh` behavior must remain equivalent to today's (SC-006).
- Both engines ship in the one Alpine/musl image (FR-009); Python stays until the fallback is
  retired (follow-up feature).
- Multi-ready, single-line (FR-013, from clarification): a tunnel is bound 1:1 to a SIM
  (EAP-AKA), so multi-card VoWiFi means one tunnel per card later — nothing new may hardcode
  the netns name, `if_id`, vpcd port, or modem port beyond their existing parametrized
  defaults (`NETNS`, `MODEM_PORT`, `--vpcd-port`, etc.).

**Scale/Scope**: Single SIM / single tunnel / one call at a time — unchanged from 011.

## Constitution Check

*Gate: must pass before Phase 0. Re-checked after Phase 1 design — still passing.*

### I. Integration-First Testing — PASS
- vpcd protocol tested over real TCP sockets (no transport mocks); APDU quirk
  normalization tested with real wire-format byte fixtures, mirroring `sip_client.rs` style.
- The modem is the one mocked boundary (scripted `AtCommander` transport) — hardware
  unavailable in CI, justification already written at the existing mock site in
  `at_commander.rs`; this feature adds no new *kind* of mock.
- charon/pcscd/live-ePDG integration is validated against the real network per `quickstart.md`
  (real carrier, real SIM), the correct integration boundary as with 011's live checks.

### II. Green-on-Commit — PASS (process gate)
- Every task ends with the full pre-commit gate; CI needs no charon/pcscd/modem, so the
  workspace test suite stays green throughout.

### III. Frequent Atomic Commits — PASS
- Phasing below (vpcd bridge → IMSI helper → Docker stages/config templates → entrypoint
  engine branch → live proving) is sized for independent, committable, testable steps.

### IV. Makefile-Driven Build — PASS
- No new Makefile targets required; the new code is CLI surface inside the existing binary.
  Docker builds continue through the existing compose/Dockerfile flow.

### V. Simplicity & Refactorability — PASS
- The pcscd+vpcd hop looks like an extra layer but is the *simplest owned surface*: the
  alternative (a custom C plugin inside charon) trades two upstream-maintained processes for
  bespoke C we must maintain in a security daemon (research.md item 2). Complexity we own:
  one Rust subcommand + config templates.
- Entrypoint keeps one shared downstream path (P-CSCF file, veth, agents, keepalive); only
  tunnel establishment branches per engine — no abstraction framework, just an `if`.
- Option 1 removal is deliberately deferred (YAGNI in reverse: keep the fallback until live
  proving passes, then delete in a follow-up).

No violations — Complexity Tracking is empty.

## Project Structure

### Documentation (this feature)

```text
specs/012-strongswan-epdg/
├── plan.md                          ← this file
├── research.md                      ← Phase 0 output
├── data-model.md                    ← Phase 1 output
├── contracts/
│   ├── tunnel-engine-contract.md    ← what any engine must provide to the 011 agents
│   └── vpcd-bridge-protocol.md      ← vpcd wire protocol + APDU normalization rules
├── quickstart.md                    ← Phase 1 output (live verification runbook)
├── checklists/requirements.md       ← spec quality checklist (from /speckit-specify)
└── tasks.md                         ← Phase 2 output (/speckit-tasks — not created here)
```

### Source Code Changes

```text
gsm-sip-bridge/src/
├── cli.rs                     MODIFY — add `vowifi-usim-bridge` and `vowifi-imsi`
│                                        subcommands (same pattern as ims-register/ims-call)
├── main.rs                    MODIFY — dispatch the two new subcommands pre-daemon, like
│                                        the existing Card/ImsRegister/ImsCall handling
├── vowifi/
│   ├── mod.rs                 MODIFY (minor) — module wiring for the new file only
│   └── usim_bridge.rs         NEW — vpcd TCP client loop: framing codec, power/ATR/reset
│                                     handling, APDU forwarding via AtCommander, GET RESPONSE
│                                     emulation + SELECT/AID normalization, busy-port retry
└── modules/usim.rs            MODIFY (only if needed) — expose raw-APDU forward helper if
                                      the existing select/authenticate surface is too coarse;
                                      existing callers untouched

docker/
├── Dockerfile                 MODIFY — new stages: strongswan-builder (fork, jolly/work,
│                                        --enable-eap-aka/-eap-sim/-eap-sim-pcsc/-p-cscf),
│                                        vpcd-builder (vsmartcard); runtime gains pcsc-lite,
│                                        charon/swanctl tree, vpcd driver, config templates
├── entrypoint.sh              MODIFY — TUNNEL_ENGINE branch: strongswan path pre-creates
│                                        netns+xfrm if_id iface, starts pcscd + usim-bridge,
│                                        renders swanctl conf (IMSI via vowifi-imsi), initiates,
│                                        watches charon log for readiness + P-CSCF → /tmp/pcscf;
│                                        swu path byte-equivalent to today; shared tail (veth,
│                                        agents, keepalive) unchanged
├── strongswan/                NEW — charon-logging.conf, p-cscf.conf, osmo-epdg.conf (load=no),
│   │                                 charon.conf drop-in (install_virtual_ip = no),
│   │                                 swanctl epdg.conf.template, ims.updown script
└── epdg/.env                  MODIFY — document TUNNEL_ENGINE (default swu for now)
```

**Structure Decision**: Everything stays inside the existing binary + the existing `docker/`
deployment surface, exactly as 011 did. The strongSwan/vpcd artifacts are vendored builds in
Docker stages, not repo source. No new crate, no new top-level directory beyond
`docker/strongswan/` config templates.

## Implementation Phases (proposed commit-sized slices)

1. **vpcd framing + bridge core** (`usim_bridge.rs` + tests): TCP client, length-prefixed
   codec, power/reset/ATR control handling against a canned ATR; APDU path stubbed to error.
2. **APDU forwarding + quirk normalization**: wire `AtCommander`/`usim.rs` behind the APDU
   path; GET RESPONSE emulation, SELECT P2 tolerance, AID redirect; table-driven fixtures from
   the patch's documented quirks. Busy-port retry/backoff.
3. **`vowifi-imsi` helper** + CLI/main dispatch for both subcommands.
4. **Docker stages + config templates**: fork build, vpcd build, runtime install, templates.
   (First point where "verify at implementation" items from research.md get burned down:
   fork builds on musl, plugin APDU trace, ATR opacity.)
5. **entrypoint `TUNNEL_ENGINE` branch**: strongswan orchestration path; `swu` path equivalence
   review; readiness + P-CSCF extraction from charon log.
6. **Live proving per quickstart.md**: tunnel on both carriers, forced-outage recovery, rekey
   soak (SC-001/002/004), end-to-end call on Airtel (SC-003), engine-switch check (SC-005/006).

## Complexity Tracking

*No entries — Constitution Check passed without deviations to justify.*
