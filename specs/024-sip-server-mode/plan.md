# Implementation Plan: SIP Server Mode

**Branch**: `024-sip-server-mode` | **Date**: 2026-07-31 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/024-sip-server-mode/spec.md`

## Summary

Today all three inbound call paths (circuit-switched, VoWiFi, VoLTE) are SIP
*clients*: they REGISTER to an external PBX and INVITE it when a call arrives
from the carrier. This adds an opt-in mode where the bridge is the SIP
*server* — IP phones REGISTER to it and it INVITEs the registered phone —
removing the PBX as a hard dependency for small deployments.

**Technical approach**: a pure-Rust UDP registrar (`gsm-sip-bridge/src/sip/server/`)
built on the hand-rolled SIP primitives already in `src/ims/sip_client.rs`,
running on its own port and maintaining a binding table. The pjsua call path is
otherwise untouched: it gains one non-registering account constructor
(`Account::local`) and takes its destination URI from the binding table instead
of `[sip].server:port`. The registrar is hosted by whichever component already
owns the outbound call leg, reusing the existing `register_trunk` arbitration,
so all three call paths are covered with no IPC and no new supervised process.

## Technical Context

**Language/Version**: Rust 1.x, edition 2021 (see `rust-toolchain.toml`)
**Primary Dependencies**: existing only — `md5` (via `src/ims/digest.rs`), `rand`,
`prometheus`, `tracing`, `serde`/`toml`. **No new crate dependencies.**
**Storage**: in-memory only (binding table, nonce table). Nothing persisted;
registrations are re-established by the phones themselves after a restart.
**Testing**: `cargo nextest` via `make test`; integration tests in
`gsm-sip-bridge/tests/`, unit tests inline. 20 s per-test timeout
(`.config/nextest.toml`).
**Target Platform**: Linux (containerised; `docker/Dockerfile`), typically a
single-board host on a small LAN.
**Project Type**: Rust workspace — a daemon plus supporting binaries.
**Performance Goals**: not throughput-bound. A handful of phones, one call at a
time. The registrar must answer a REGISTER well inside a phone's retransmit
timer (RFC 3261 T1 = 500 ms), which a synchronous in-memory lookup satisfies by
orders of magnitude.
**Constraints**:
- `tools/count-unsafe.sh` (run by `make lint`) fails the build on **any**
  `unsafe` in `gsm-sip-bridge/src`. The registrar must be safe Rust.
- The `pjsip-linked` cargo feature is **not** enabled by `make test`, `make lint`,
  or CI — only `docker/Dockerfile` builds it. Anything behind that feature is
  neither compiled nor tested in CI.
- Two SIP endpoints cannot bind the same UDP port; the existing port ladder is
  5070–5073 plus 5074+ strided by 4 per VoLTE line.
**Scale/Scope**: 1–8 phone accounts, one designated ringing account, one active
call at a time. ~900 lines of new Rust plus tests.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-checked after Phase 1 design.*

| Principle | Assessment | Verdict |
|---|---|---|
| **I. Integration-First Testing** (NON-NEGOTIABLE) | The registrar is exercised end-to-end over **real loopback UDP sockets** — a real `UdpSocket` "phone" against the real registrar, asserting real SIP bytes. **Zero mocks are introduced**, so the written-justification requirement has nothing to discharge. Binding expiry is made testable without `sleep` by passing `now: Instant` as a parameter rather than reading the clock internally. | PASS |
| **II. Green-on-Commit** (NON-NEGOTIABLE) | The commit sequence below is ordered so every commit is independently green; commits 5–7 add tested-but-unwired modules before the feature goes live in commit 10. | PASS |
| **III. Frequent Atomic Commits** | 14 commits, each a single concern. Refactors (2, 3, 8) are separated from the features that need them. | PASS |
| **IV. Makefile-Driven Build** | No new build operations; existing `make build/test/run/clean/lint/format` cover the work unchanged. | PASS |
| **V. Simplicity & Refactorability** | Two deliberate simplifications over the RFC — one binding per AOR (not a contact set), and no forking — are recorded with in-code justification comments. One new abstraction (`CallTarget`) is introduced; it is a **net removal** of existing duplication. See Complexity Tracking. | PASS with justification |

**Post-Phase-1 re-check**: no new violations. The design adds exactly one
function to `pjsua-safe` (`Account::local`, whose own `unsafe` block takes that
crate from 28 to 29 — a 1.69% ratio against `count-unsafe.sh`'s 5% ceiling,
with `gsm-sip-bridge/src` staying at the required zero) and no new crate
dependencies. The feature's untestable-in-CI surface is a single stub returning
`Ok`.

## Project Structure

### Documentation (this feature)

```text
specs/024-sip-server-mode/
├── plan.md              # This file
├── spec.md              # Feature specification
├── research.md          # Phase 0 — design decisions and rejected alternatives
├── data-model.md        # Phase 1 — entities, state, validation rules
├── quickstart.md        # Phase 1 — operator walkthrough
├── contracts/
│   ├── config-schema.md # The [sip_server] TOML contract
│   └── sip-registrar.md # The on-the-wire SIP contract toward IP phones
├── checklists/
│   └── requirements.md  # Spec quality checklist
└── tasks.md             # Phase 2 output (/speckit-tasks)
```

### Source Code (repository root)

```text
gsm-sip-bridge/
├── src/
│   ├── sip/
│   │   ├── mod.rs               # MODIFIED: server-mode branch in register();
│   │   │                        #   compute_destination_uri -> Result; teardown
│   │   ├── target.rs            # NEW: CallTarget — one destination rule, all paths
│   │   └── server/
│   │       ├── mod.rs           # NEW: UDP serve loop + SIP method dispatch
│   │       ├── bindings.rs      # NEW: BindingStore, Binding, expiry
│   │       └── auth.rs          # NEW: digest challenge/verify, NonceStore
│   ├── ims/
│   │   ├── mod.rs               # MODIFIED: widen `digest` + `sip_client` to pub(crate)
│   │   └── sip_client.rs        # MODIFIED: build_uas_response_with_headers
│   ├── config/
│   │   ├── raw.rs               # MODIFIED: RawSipServer, RawSipServerAccount, KEYS
│   │   ├── build.rs             # MODIFIED: build_sip_server + cross-section rules
│   │   ├── env.rs               # MODIFIED: secret key path
│   │   └── mod.rs               # MODIFIED: SipServerConfig on AppConfig
│   ├── modules/mod.rs           # MODIFIED: handle fallible destination at Ring
│   ├── vowifi/mod.rs            # MODIFIED: host the registrar on the telephony side
│   └── metrics/mod.rs           # MODIFIED: five new series
└── tests/
    └── test_sip_server_registrar.rs  # NEW: real-socket registrar conformance

pjsua-safe/src/account.rs        # MODIFIED: Account::local (linked + stub)
```

**Structure Decision**: The registrar lives under `src/sip/server/` — beside the
existing SIP client code it complements, not under `src/ims/`, which is
carrier-facing. It is deliberately **process-agnostic**: it owns a socket, a
store and a thread, and is constructed by whichever component hosts it. That is
what lets one module serve both the circuit-switched daemon and the
VoWiFi/VoLTE telephony agent without duplication or IPC.

## Key Design Decisions

Full rationale in [research.md](./research.md). Summary:

1. **Pure-Rust registrar, not a PJSIP module.** A `pjsip_module` would share
   pjsua's socket (nicer on the wire) but would live entirely behind
   `#[cfg(feature = "pjsip-linked")]`, which CI never compiles, lints, or runs —
   putting an authentication subsystem outside both NON-NEGOTIABLE principles.
   It would also require `unsafe` reachable from a C callback, in a crate whose
   build hard-fails on any `unsafe`.
2. **Two ports, made explicit.** The registrar takes 5060; the operator is told
   by a startup error to move `[sip].local_port`. Rejected: a hidden constant
   (collides with a port ladder that grows per VoLTE line) and a SIP proxy
   (correct but strictly more machinery — recorded as the escape hatch).
3. **Registrar hosted by the trunk owner.** Reuses the existing `register_trunk`
   arbitration, so all three call paths work with no IPC and no fourth process.
   Verified: the VoWiFi agent is spawned without an `ip netns exec` wrapper, so
   it is LAN-reachable.
4. **Challenge every REGISTER.** Makes the nonce lifecycle trivial (one nonce,
   one use) and matches what every IP phone expects.
5. **Settings that would do nothing are startup errors**, not warnings —
   consistent with why this project adopted strict config parsing.

## Complexity Tracking

> Constitution Principle V requires written justification for any new
> abstraction, layer, or indirection.

| Addition | Why needed | Simpler alternative rejected because |
|---|---|---|
| `CallTarget` enum (`src/sip/target.rs`) | `SipBridge::compute_destination_uri` and `vowifi::pbx_dest_uri` are already deliberate duplicates of one rule, and **both** must now grow the identical binding-lookup branch. | Leaving them duplicated means writing the new branch twice and keeping two copies of the DID-passthrough rule in sync across two subsystems. The enum is a **net reduction** in duplicated logic — it removes an existing copy rather than adding a layer over it. |
| `BindingStore` / `NonceStore` types | They are the feature's state, not indirection over something else. | No alternative: the data must live somewhere, and both need interior mutability behind an `Arc` because the registrar thread writes while the call path reads. |
| `now: Instant` as a parameter rather than an internal clock read | Makes every expiry and nonce-lifetime rule directly testable under the 20 s per-test timeout without `sleep`. | An internal `Instant::now()` would force either real sleeps (slow, flaky) or a clock-injection trait — a genuine new abstraction, strictly more machinery than one parameter. |

**Not** added, and deliberately so: no trait abstracting "SIP backend", no clock
trait, no async runtime for the registrar (one blocking thread with a read
timeout, matching `ims::agent`'s existing listeners), no new supervised process,
no IPC, no new crate dependency.

## Commit Sequence

Each commit is independently green under `make format && make lint && make test`.

1. `docs(specs): add 024-sip-server-mode spec, plan, research, data model, tasks`
2. `refactor(ims): make sip_client and digest reachable crate-wide`
3. `feat(ims): let a UAS response carry extra headers`
4. `feat(config): add the opt-in [sip_server] section`
5. `feat(sip): registration binding store`
6. `feat(sip): digest challenge and verification for the registrar`
7. `feat(sip): serve REGISTER and OPTIONS on the bridge's own SIP port`
8. `refactor(sip): unify PBX destination-URI logic behind CallTarget`
9. `feat(pjsua-safe): add a non-registering local account`
10. `feat(sip): ring the registered phone instead of a PBX` ← feature goes live
11. `feat(vowifi): host the registrar on the telephony side too`
12. `feat(sip): observe the embedded registrar`
13. `docs: document SIP server mode in the architecture and operations guides`
14. `chore(release): 8.3.0`
