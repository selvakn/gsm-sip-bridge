# Implementation Plan: Per-Line Network Isolation for VoLTE

**Branch**: `020-volte-line-netns` | **Date**: 2026-07-24 | **Spec**: [./spec.md](./spec.md)
**Input**: Feature specification from `specs/020-volte-line-netns/spec.md`

## Summary

Give each VoLTE line the same per-line network namespace isolation VoWiFi lines already have
(features 011-013), closing the gap left by the prior multi-modem work: today every VoLTE line's
carrier-facing sockets bind the wildcard address and share the container's one routing table, so
which physical LTE interface a line's traffic actually uses is a kernel routing decision by
destination alone — not guaranteed to match the line that sent it.

The mechanism already exists in the tree for the other subsystem, and one seam is already shared
between the two: `volte::bridge::run_inner` already builds `crate::vowifi::RuntimeLine`s and calls
`crate::vowifi::run_telephony_side` — the exact function that talks to VoWiFi's Agent B over a real
veth pair — passing `127.0.0.1` for both `veth_local_addr` and `veth_peer_addr` only because, today,
every VoLTE line's carrier half is a thread in the same process and namespace as the shared
telephone half. Splitting each line's carrier half out into its own OS process, launched via
`ip netns exec volteN` exactly as `docker/entrypoint.sh` already does for `vowifi-ims-agent`, and
handing `run_telephony_side` that line's real veth addresses instead of loopback, is not new
plumbing — it is turning on a generalization the code already made. The carrier-facing Rust code
(`ims::agent`, `ims::sip_client`, `volte::pdn`, `volte::netcfg`) needs no changes: a process
launched inside a namespace via `ip netns exec` inherits that namespace for every socket and every
`ip`/`sysctl` shell-out it makes, with no per-call-site awareness required — which is exactly why
this satisfies spec FR-002 ("must not depend on every call site remembering to bind correctly").

What is new: a per-line carrier-agent subcommand (replacing the in-process thread body of
`run_line`/`run_line_carrier`), an `entrypoint.sh` loop that moves each line's LTE interface into
its own namespace and creates its veth pair before launching that subcommand under `ip netns exec`
(mirroring `ensure_epdg_interface`/`start_line_tail`), and per-line namespace/veth-name derivation
in `volte::discovery` shaped like `vowifi::discovery`'s but on a namespace prefix (`volte`) that can
never collide with VoWiFi's (`ims`) — closing this feature's own FR-004a.

## Technical Context

**Language/Version**: Rust stable (pinned by `rust-toolchain.toml`), unchanged. `bash` for
`docker/entrypoint.sh`'s VoLTE section, extended with a per-line loop mirroring the existing VoWiFi
one.

**Primary Dependencies**: No new Rust crates. Reuses `crate::vowifi::run_telephony_side` and
`crate::vowifi::RuntimeLine` as-is (already generic over veth vs. loopback addressing — see
Summary), `crate::ims::agent::serve_inbound` as-is (already the shared carrier-facing loop for both
subsystems), `crate::volte::discovery` (extended, not replaced, with namespace/veth derivation
shaped like `crate::vowifi::discovery`'s), `serde`/`serde_json` (already a workspace dependency,
for the extended line manifest).

**Storage**: None new. `volte::discovery::VolteLineManifest` (the existing JSON manifest at
`super::discovery::manifest_path()`, already written by `write_manifest` for cleanup/status) gains
`netns`/veth fields, the same way the manifest already carries `iface`/`restore_cid_path`.

**Testing**: `cargo test --workspace`. Namespace/veth-name derivation is a pure function
(`resolve_volte_lines` extended), unit-tested table-driven exactly like
`vowifi::discovery::resolve_lines`'s existing tests (`assert_ne!` on two lines' namespaces, an
explicit test that a VoLTE line's namespace never equals a same-index VoWiFi line's — closing
FR-004a directly in a unit test, not only live). The carrier-agent subcommand extraction is a
structural refactor of already-tested code (`run_line_carrier`'s body moves, it does not change) —
existing bridge lifecycle tests continue to cover it. Cross-line traffic isolation (spec SC-001) and
the two-subsystem non-collision scenario (spec User Story 1 scenario 5) need real network
namespaces and are validated live per `quickstart.md`, the boundary every VoLTE/VoWiFi multi-line
feature to date has drawn (spec Assumptions).

**Target Platform**: Linux, the existing Alpine/musl container image, host-kernel network namespaces
— now used by VoLTE lines as well as VoWiFi lines. No new privilege: `docker-compose.yml`'s
`privileged: true` + `network_mode: host` already grants `NET_ADMIN`/`SYS_ADMIN` and netns/veth setup
for VoWiFi; nothing beyond `ip link set <iface> netns <ns>` (moving an *existing* interface, not
creating one) is required for VoLTE.

**Project Type**: Extension of the existing `gsm-sip-bridge` binary (one new CLI subcommand
replacing an in-process code path, one modified one) + deployment surface
(`docker/entrypoint.sh`). No new crate.

**Performance Goals** (from spec Success Criteria):
- Zero cross-line packets observed on either interface over a full registration-and-call cycle with
  two same-carrier lines (SC-001).
- Single-line call-answer latency and audio quality unchanged from before this feature (SC-002).
- All previously-passing multi-line VoLTE acceptance criteria continue to pass unmodified (SC-003).
- A forced one-line network failure leaves every other line's registration and in-progress call
  unaffected (SC-004).
- Unclean shutdown + restart brings every line back up with no manual intervention (SC-005).

**Constraints**:
- **Zero new `unsafe` in `gsm-sip-bridge/src`** (unchanged gate, `tools/count-unsafe.sh`) —
  satisfied by design: isolation is achieved by shelling out to `ip`/launching a subprocess under
  `ip netns exec`, consistent with `netcfg.rs`'s and `gm_ipsec.rs`'s existing convention of shelling
  out rather than calling raw netlink/`setns()`. No FFI, no new `unsafe` block.
- Full pre-commit gate unchanged: `cargo fmt --all`, `make lint`, `cargo test --workspace`.
- FR-004b: isolation MUST be unconditional — one code path, no configuration flag reverting to
  today's shared-namespace/thread arrangement. `volte-bridge`'s current in-process thread body for
  the carrier half is replaced, not made conditional.
- FR-004a: a VoLTE line's namespace/veth identifiers MUST be derived from a prefix that cannot
  collide with VoWiFi's (`ims` → `volte`), and this is asserted in a unit test, not left to
  convention alone.
- Scope boundary (spec Assumptions): this feature isolates the lines the prior multi-modem work
  (`volte-bridge`, US1/US2 of specs/017 and specs/018) already establishes. The single-modem,
  single-invocation CLI verbs `volte-register`/`volte-call`/`volte-listen` (used for manual
  registration/outbound-call testing, specs/015/016) have no multi-line concept today and are out of
  scope — there is nothing for their traffic to collide with in isolation, and giving them a netns
  would be isolation with no line to isolate *from*.

**Scale/Scope**: Up to `[volte].max_lines` (default 8) concurrent lines — matching the existing
VoWiFi bound and the existing VoLTE multi-modem bound, small-deployment scale. One call at a time
per line, unchanged.

## Constitution Check

*Gate: must pass before Phase 0. Re-checked after Phase 1 design.*

| Principle | Assessment | Status |
|---|---|---|
| **I. Integration-First Testing** | Namespace/veth-name derivation is a pure function, unit-tested table-driven (including the FR-004a non-collision assertion). The carrier-agent extraction moves already-integration-tested code (`run_line_carrier`) without changing its logic. The property the feature exists to prove — a line's traffic cannot leave on another line's interface — needs real kernel namespaces and two physical modems, and is validated live per `quickstart.md`, exactly the boundary specs/012/013 already drew for the mechanism this reuses. | ✅ PASS |
| **II. Green-on-Commit** | `make format && make lint && make test` before every commit; no test requires a modem, carrier, or even a real namespace (namespace/veth derivation tests are pure Rust) to pass. | ✅ PASS |
| **III. Frequent Atomic Commits** | Phases (discovery/derivation → carrier-agent subcommand extraction → `entrypoint.sh` per-line loop → manifest/status fields) are independently committable, each leaving the suite green — mirroring how specs/013 phased its own, structurally identical, VoWiFi change. | ✅ PASS |
| **IV. Makefile-Driven Build** | No new entry points beyond one CLI subcommand reached through the existing `gsm-sip-bridge` binary and supervised by the existing entrypoint; `make` targets unchanged. | ✅ PASS |
| **V. Simplicity & Refactorability** | **This is a convergence, not a new abstraction.** `run_telephony_side`/`RuntimeLine` already generalize over veth-vs-loopback addressing because VoWiFi wrote them that way first; this feature is the second, intended caller finally using that generality instead of a same-process shortcut. Net effect: VoLTE's process model becomes *identical* to VoWiFi's (one shared telephone-side process, one carrier-side process per line, `ip netns exec`-launched) rather than a second, divergent multi-line pattern living alongside it — one less shape to maintain, not one more. | ✅ PASS |

**Post-Phase-1 re-check**: ✅ Still passing. Phase 1 design adds no new trait, no new indirection layer,
and no configuration surface (FR-004b forbids one) — it extends existing per-line derivation
(`volte::discovery`, shaped after `vowifi::discovery`) and existing `entrypoint.sh` per-line looping
(shaped after the VoWiFi block already there) with one new subcommand replacing an in-process thread
closure.

## Project Structure

### Documentation (this feature)

```text
specs/020-volte-line-netns/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md         # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/
│   └── volte-carrier-agent-contract.md   # New subcommand's CLI/runtime contract
├── checklists/
│   └── requirements.md
└── tasks.md              # Phase 2 output (/speckit-tasks)
```

### Source Code (repository root)

```text
gsm-sip-bridge/src/
├── volte/
│   ├── bridge.rs           # MODIFY: run_inner spawns only the shared telephony
│   │                       #   thread (in-process, default netns) — the per-line
│   │                       #   carrier thread body (run_line/run_line_carrier)
│   │                       #   is extracted, not deleted, into...
│   ├── carrier_agent.rs    # NEW: single-line carrier-agent entry point — the
│   │                       #   extracted body of run_line/run_line_carrier,
│   │                       #   invoked as its own process via
│   │                       #   `ip netns exec volteN`, mirroring
│   │                       #   vowifi-ims-agent's role for Agent A
│   ├── discovery.rs        # MODIFY: per-line netns/veth-iface/veth-addr
│   │                       #   derivation, shaped after
│   │                       #   vowifi::discovery::resolve_one_line, on a
│   │                       #   "volte"-prefixed namespace/iface base (FR-004a)
│   ├── mod.rs               # MODIFY: VolteConfig gains netns/veth base fields
│   │                       #   (defaults, no behavior change at index 0)
│   └── guard.rs             # REUSED as-is
│
├── config/mod.rs            # MODIFY: VolteConfig netns/veth base fields +
│                            #   defaults; volte-shell-env gains a per-line
│                            #   array output (mirrors vowifi-shell-env's
│                            #   `discover --shell-env`)
├── cli.rs / main.rs         # MODIFY: new `volte-carrier-agent --line N`
│                            #   subcommand; `volte-bridge` keeps building
│                            #   the line table and starting Agent B only
│
docker/entrypoint.sh         # MODIFY: VoLTE section gains a per-line loop
                              #   (move iface into netns, create veth pair,
                              #   launch `ip netns exec volteN ... 
                              #   volte-carrier-agent --line N`, supervised) —
                              #   structurally mirroring the existing VoWiFi
                              #   per-line loop (`ensure_epdg_interface`/
                              #   `start_line_tail`), before starting the one
                              #   shared `volte-bridge` (Agent B only, once
                              #   every line's veth exists)

gsm-sip-bridge/tests/
├── test_volte_discovery.rs  # MODIFY/NEW: netns/veth derivation table tests,
│                            #   including the volte-vs-vowifi non-collision
│                            #   assertion (FR-004a)
└── test_volte_bridge.rs     # MODIFY: carrier-agent extraction covered by
                              #   existing lifecycle tests, unchanged behavior
```

**Structure Decision**: Single Rust workspace, unchanged crate layout. The only structurally new
file is `volte/carrier_agent.rs`, which is an *extraction* (the body of today's `run_line`/
`run_line_carrier` moved to its own entry point, not new logic) — the same shape specs/013 used when
it turned VoWiFi's single-line Agent A into `vowifi-ims-agent --line N`. Everything else is
per-line derivation (`discovery.rs`) and orchestration (`entrypoint.sh`), matching the existing
VoWiFi pattern file-for-file.

## Complexity Tracking

*No unjustified violations.* The Constitution V assessment above explains why this is a convergence
onto an existing pattern (VoWiFi's Agent A/B split, and `run_telephony_side`'s pre-existing
veth-vs-loopback generality) rather than new complexity — there is nothing in this table.
