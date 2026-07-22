# Implementation Plan: Host-Side IMS Registration over LTE (VoLTE)

**Branch**: `015-volte-host-ims` | **Date**: 2026-07-22 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/015-volte-host-ims/spec.md`

## Summary

Give the bridge its own IMS registration over cellular, so cellular voice stops
depending on the modem's internal IMS stack and its degraded audio hand-off.

The approach is deliberately narrow: **the entire IMS stack already exists and
is reused unchanged.** Registration, IMS-AKA authentication, Gm IPsec, and SIP
message construction are shared with the production VoWiFi path. Only the
network attachment underneath differs — an LTE IMS PDN instead of an ePDG
tunnel. The work is therefore (a) a new transport that establishes that PDN and
binds it to the host, (b) P-CSCF discovery, since this modem will not surface
it, and (c) a small seam in `ims/` that lets either transport feed the same
registration machinery.

Phase 0 research verified the riskiest premise on live hardware: **Vodafone
India grants an IMS PDN to a host-controlled PDP context**, and the host
interface carries it. The remaining open question is P-CSCF discovery, which is
gated below rather than assumed.

## Technical Context

**Language/Version**: Rust, toolchain pinned in `rust-toolchain.toml`; workspace-wide zero-`unsafe` policy enforced by `make lint`
**Primary Dependencies**: existing in-tree `ims/` stack (`sip_client`, `digest`, `gm_ipsec`), `modules::at_commander`, `socket2`, `tracing`; `iproute2` (`ip`) shelled out, following the precedent in `ims/gm_ipsec.rs`
**Storage**: existing SQLite store (`store/`) — reused for registration history only; no new schema in this feature
**Testing**: `cargo test --workspace`; integration-first per Constitution I, using the established wire-level modem simulation (`UnixStream` pair + real `AtCommander::from_stream`, per `tests/test_at_commander.rs`)
**Target Platform**: Linux, inside the existing `privileged: true` + `network_mode: host` container (`docker/docker-compose.yml`)
**Project Type**: Single Rust workspace — CLI plus long-running bridge daemon
**Performance Goals**: Not throughput-bound. Attachment under 60s (SC-001); registration under 60s (SC-003)
**Constraints**: IMS PDN is **IPv6-only** on the target carrier; modem exposes exactly one host-facing data path; no QMI/MBIM on EC200U — AT commands are the only control channel
**Scale/Scope**: One modem, one SIM, one registration. Multi-card explicitly out of scope

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Assessment | Status |
|---|---|---|
| **I. Integration-First Testing** | Modem AT exchanges tested against a real `AtCommander` driven over a `UnixStream` pair — the existing in-repo pattern. This simulates the *hardware peer at the wire level*, not the component under test, so it satisfies "real components through actual interaction". Hardware is unavailable in CI, which is the constitution's stated exemption. Live-hardware validation is additionally required by `quickstart.md`. | ✅ PASS |
| **II. Green-on-Commit** | `make format && make lint && make test` before every commit, per `CLAUDE.md`. No hardware-dependent test may be added that fails without a modem — such tests must skip explicitly, as `test_at_commander.rs` already does. | ✅ PASS |
| **III. Frequent Atomic Commits** | Task breakdown maps to independently committable units, ordered so each leaves the suite green. | ✅ PASS |
| **IV. Makefile-Driven Build** | No new entry points. New capability is exposed as CLI subcommands reached through existing `make` targets. | ✅ PASS |
| **V. Simplicity & Refactorability** | One new abstraction (`ImsTransport`) — justified in Complexity Tracking below. Two simplifying decisions taken: **no netns** (R4) and **no AT-port locking layer** (R5), both because the simpler option is sufficient today. | ⚠️ PASS with justification |

**Post-Phase-1 re-check**: ✅ Still passing. The Phase 1 design added no further
abstractions. `data-model.md` introduces no persisted schema. The contracts
describe the CLI surface and the one trait already justified below.

## Gates

Gates are hard stops. Work downstream of an unmet gate must not begin.

### Gate G1 — P-CSCF discovery spike — ✅ **EXECUTED 2026-07-22, result NEGATIVE**

Run in the privileged container against the live Vi India network. Full
evidence in `research.md` R2.

**Outcome**: all three candidate methods **definitively excluded**. DHCPv6
runs but the carrier returns no RFC 3319 options; the RA carries only prefix
and MTU; DNS is unusable because the only servers offered are `::`. A fourth
avenue found during the spike — an undocumented `AT+QCFG="pdn/pco"` toggle —
was enabled and survived a modem reboot, but never populated.

**The gate did its job.** It cost one session and prevented building a
three-method fallback chain that would have failed on every path.

**Consequences** — see "Post-G1 plan revision" below.

### Gate G3 — Obtain a Vi India P-CSCF address *(NEW; blocks US3)*

G1 removed the assumed route to a P-CSCF address, so acquiring one is now its
own gate. US3 cannot start without it. Ordered by cost:

1. Capture the P-CSCF from a **Vi ePDG tunnel** using the existing VoWiFi
   path (it deposits the address at `pcscf_source_path`), then try that
   address from the LTE PDN. Same IMS core; plausibly the same node.
2. Evaluate an **EC200U firmware build that honours `pdn/pco`**.
3. **Vendor query to Quectel** on PCO/P-CSCF exposure.

**Exit criteria**: a P-CSCF address that answers on the IMS PDN.

**If all three fail**, this feature cannot reach US3 on this hardware. US1 is
still independently valuable and complete, and should ship regardless.

### Gate G2 — Gm IPsec over IPv6 *(blocks US3)*

Research R3 found the SIP layer already IPv6-clean, but `ims/gm_ipsec.rs` has
never had its XFRM states and policies exercised with IPv6 selectors. Verify
ESP transport-mode SA installation over IPv6 **independently of registration**,
so a failure here is not misdiagnosed as a registration failure.

## Project Structure

### Documentation (this feature)

```text
specs/015-volte-host-ims/
├── plan.md              # This file
├── spec.md              # Feature specification
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/
│   ├── volte-cli-contract.md        # CLI surface
│   └── ims-transport-contract.md    # Substitutable transport contract
├── checklists/
│   └── requirements.md  # Spec quality checklist
└── tasks.md             # Phase 2 output (/speckit-tasks — NOT created here)
```

### Source Code (repository root)

```text
gsm-sip-bridge/src/
├── ims/                       # SHARED — reused by both transports
│   ├── mod.rs                 # MODIFY: register over a supplied transport
│   ├── transport.rs           # NEW: ImsTransport trait (the seam)
│   ├── sip_client.rs          # UNCHANGED — already IPv6-clean (R3)
│   ├── digest.rs              # UNCHANGED
│   └── gm_ipsec.rs            # VERIFY over IPv6 (Gate G2); change only if needed
│
├── volte/                     # NEW — mirrors the shape of vowifi/
│   ├── mod.rs                 # Orchestration + ImsTransport impl
│   ├── pdn.rs                 # IMS PDN lifecycle over AT (CGDCONT/CGACT/QNETDEVCTL)
│   ├── pcscf.rs               # Ordered discovery chain + config override
│   └── netcfg.rs              # Host interface configuration (shells out to `ip`)
│
├── vowifi/                    # UNCHANGED behaviour (FR-019)
│   ├── mod.rs                 # Adapts to ImsTransport; no behavioural change
│   ├── imsi.rs                # REUSED as-is by volte
│   └── usim_bridge.rs         # REUSED as-is by volte
│
├── cli.rs                     # MODIFY: volte-pdn / volte-discover / volte-register / volte-status
│                              #   (flat kebab-case, matching ims-register / vowifi-status)
└── config/                    # MODIFY: VolteConfig (modem port, cid, APN, P-CSCF override)

gsm-sip-bridge/tests/
├── test_volte_pdn.rs          # NEW: PDN lifecycle vs simulated modem
├── test_volte_pcscf.rs        # NEW: discovery chain ordering + fallback + override
└── test_ims_transport.rs      # NEW: both transports satisfy the same contract
```

**Structure Decision**: Extend the existing `gsm-sip-bridge` crate rather than
adding a workspace member. The new `volte/` module intentionally mirrors
`vowifi/` so the two transports are visibly parallel. The only change inside
`ims/` is the `transport.rs` seam plus the wiring in `mod.rs` to accept a
transport — the SIP, digest, and IPsec logic is untouched, which is what
FR-017 and SC-007 require.

## Complexity Tracking

| Violation | Why Needed | Simpler Alternative Rejected Because |
|---|---|---|
| New abstraction: `ImsTransport` trait (Principle V) | FR-017/FR-018 require one registration implementation serving two transports, and SC-007 forbids duplicating it. **Two concrete implementors exist the day the trait lands** (ePDG and LTE), so this is not speculative generality — YAGNI is satisfied. | *Duplicating `register_session` per transport* — directly violates SC-007 and would fork the production VoWiFi code path, risking the regression FR-019 forbids. *Branching on an enum inside `register_session`* — keeps one copy but pushes transport-specific concerns into shared code, making the VoWiFi path newly capable of breaking from VoLTE changes. The trait keeps that blast radius at zero. |

Two candidate complexities were **rejected** rather than justified, and are
recorded here so they are not re-added without cause:

- **A dedicated network namespace for VoLTE** (mirroring VoWiFi). Rejected per
  R4 — one dedicated interface, no veth bridging, no multi-line collision.
- **A locking layer over the modem AT port.** Rejected per R5 — the
  separate-steps CLI design (FR-021) already serialises access.

## Post-G1 plan revision

G1's negative result changes the feature's shape. Three amendments:

### 1. P-CSCF configuration is promoted from fallback to primary

`--pcscf` / config (FR-010) becomes **the supported way** to supply the
address. The three probes are demoted to **diagnostics**: they still run and
still report per-method results, because that reporting is what makes a
future firmware or carrier change discoverable — but they are no longer
presented as the route to a working address.

**Spec amendments required** (flagged for approval, not yet applied to
`spec.md`):

| Item | Current wording | Required change |
|---|---|---|
| **US2 title** | "Locate the carrier's IMS entry point **automatically**" | "**Determine** the IMS entry point, reporting definitively when the carrier does not provide one" |
| **SC-002** | "located automatically, **with no hand-entered address**" | Not achievable. Replace with: discovery runs, reports per-method results, and the endpoint in use is identified with its source |
| **FR-007** | "MUST attempt to discover … **without requiring the operator to supply it**" | Soften to: MUST attempt discovery and MUST report results; MUST NOT require discovery to succeed |
| **US2 priority** | P1 | **Demote to P3.** It no longer gates US3 — G3 does |

FR-008, FR-009, FR-011 survive unchanged: the ordered chain, the method
attribution, and the per-method breakdown are all still exactly what is
wanted. Only the expectation that one of them *succeeds* was wrong.

### 2. Host interface addressing becomes a first-class requirement

R7 found that the IMS PDN is unusable until the host adopts the
modem-assigned IID (the network unicasts RAs to it). This was not in the spec
at all. `volte/netcfg.rs` must set `addr_gen_mode=none` and install the
`CGPADDR`-derived link-local **before** soliciting an RA.

**Proposed new requirement** — *FR-024: The bridge MUST configure the host
interface with the interface identifier the network assigned, and MUST NOT
rely on kernel-generated addressing.*

### 3. US1 is complete and independently shippable

The spike proved the whole US1 path end to end: PDN granted, bound to the
host, RA accepted, global address and default route installed. With US2
demoted and US3 gated on G3, **US1 is the deliverable that is certain**, and
should ship on its own rather than waiting.

## Implementation Phasing

Revised after G1. Ordered so each phase leaves a green, committable tree, and
so no phase depends on an unmet gate.

| Phase | Delivers | Stories | Gate | Status |
|---|---|---|---|---|
| 1 | `ImsTransport` seam; VoWiFi refactored onto it with **zero behavioural change** | — (FR-017/018) | — | ✅ **Done** |
| 2 | IMS PDN lifecycle + **IID-aware interface config (FR-024)**; `volte-pdn`, `volte-status` | US1 | — | ✅ **Done — verified on live hardware** |
| 3 | Discovery probes as **diagnostics**; `volte-discover` reports per-method results | US2 (P3) | — | Ready |
| 4 | **G3**: acquire a P-CSCF address | — | **G3** | ⚠️ Blocked |
| 5 | **G2 verification**, then registration over the LTE transport | US3 | **G2, G3** | ⚠️ Blocked on G3 |
| 6 | Renewal, registration history | US4 | — | After phase 5 |

Phase 1 must land and prove FR-019 (VoWiFi unchanged) before any VoLTE code is
written. That ordering is what keeps the production path safe.

Phases 1–3 are unblocked and account for most of the implementation. Phase 4
(G3) is a research task, not a coding task, and can run in parallel.

### Phases 1–2 completion notes

Both landed with `make lint` clean and `cargo test --workspace` green (382 unit
tests, up from 353).

**Phase 1** — `ims/transport.rs` holds the trait, `TransportStage`, and
`EpdgTransport`. VoWiFi's inline P-CSCF read became `EpdgTransport::prepare()`
with byte-identical error text; the only observable difference is one added
`tracing::info!` line. Note that `EpdgTransport::teardown` is intentionally a
no-op — the tunnel is supervised by `entrypoint.sh`, shared with the SIP-side
agent, and outlives any registration.

**Phase 2** — `volte/{mod,pdn,netcfg}.rs` plus `volte-pdn` and `volte-status`.
Verified against the live Vodafone India network:

| Scenario | Result |
|---|---|
| `--action up --iface …` | ✅ PDN attached, `ims.mnc043.mcc404.gprs`, bearer 6, IPv6-only, **routable: yes** with `default via fe80::…:2540 proto ra` |
| Repeat `--action up` (FR-004) | ✅ Reports reuse, exit 0, no duplicate attachment |
| `--action down` (FR-005) | ✅ PDN released, host addresses flushed |
| `--action status` after down | ✅ "No IMS PDN attached", exit 0 |
| `--action down` twice | ✅ No-op, exit 0 |

R10 was found *by this hardware run* and would not have been caught by unit
tests: the first build reported success on an unroutable interface. The fix —
waiting on the default route rather than on address presence — is why the
`routed` flag exists.

**Deferred from Phase 2**: `[volte]` config-file support. `config/mod.rs` uses
a hand-rolled parser with a `KNOWN_KEYS` list, and the CLI defaults
(`--modem`, `--cid`, `--apn`, `--iface`) cover every current use. The
`volte-cli-contract.md` wording "from `[volte]` config" is therefore not yet
satisfied; fold it in when the daemon needs to bring the PDN up unattended.
