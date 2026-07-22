# Implementation Plan: Voice Calls over the Host-Side LTE Registration

**Branch**: `016-volte-calls` | **Date**: 2026-07-22 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/016-volte-calls/spec.md`

## Summary

Place a real outbound call over the cellular IMS registration built in
`specs/015-volte-host-ims`, with the bridge — not the modem — handling the
audio, and **measure the result well enough to answer whether the audio is
actually better**.

As with feature 015, most of this is reuse. `ims::call::run_call` already
places outbound calls over the Wi-Fi path, `ims::sdp` already negotiates the
carrier's audio formats, and `CallConfig` already has a call duration, both
directions recorded separately, and per-direction sample counts. The genuinely
new work is narrow and concentrated in three places:

1. **Media condition measurement** — sequence numbers are parsed and thrown
   away today; loss and jitter must be derived from them (research R2).
2. **Preferential-handling evidence** — sampling the modem's per-context
   quality class before, during and after the call (research R4).
3. **A verdict on one-way audio** — the counts exist, the judgement does not
   (research R5).

Phase 0 verified the riskiest premise on hardware: the modem **will** report
the quality class per context (`AT+CGEQOSRDP` → context 3, class 5). That was
the open question behind FR-014, and it means the feature's headline
measurement is implementable rather than aspirational.

## Technical Context

**Language/Version**: Rust, toolchain pinned in `rust-toolchain.toml`; workspace-wide zero-`unsafe` policy enforced by `make lint`
**Primary Dependencies**: existing in-tree `ims/` stack (`call`, `sdp`, `rtp`, `amr_rtp`, `transcode`), `amr-safe` (behind the `amr-linked` feature), `volte` transport and guard from spec 015
**Storage**: existing SQLite store for call history; no new schema required for the diagnostic command
**Testing**: `cargo test --workspace`; integration-first per Constitution I. The new media-condition logic is pure and clock-injectable, so it is unit-testable without hardware — unusually for this project
**Target Platform**: Linux, inside the existing `privileged: true` + `network_mode: host` container
**Project Type**: Single Rust workspace — CLI plus long-running bridge daemon
**Performance Goals**: Real-time audio. A call must sustain continuous two-way audio for at least 30s (SC-006) without starving the media loop
**Constraints**: IMS PDN is IPv6-only; the wideband codec is behind a build feature and **absent from plain local builds** (present in the container image); the modem exposes one host data path; AT access must not collide with the registration loop's own AT usage
**Scale/Scope**: One call at a time, one modem, one SIM

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Assessment | Status |
|---|---|---|
| **I. Integration-First Testing** | The new media-condition logic (loss, jitter, one-way verdict) is pure over parsed packets and is tested directly, with no mocking of anything. Modem AT exchanges reuse the established wire-level simulation (`UnixStream` pair + real `AtCommander`). The call itself cannot be tested without a carrier, so live validation is mandated in `quickstart.md` rather than faked. | ✅ PASS |
| **II. Green-on-Commit** | `make format && make lint && make test` before every commit. No test may be added that requires a modem or a carrier to pass. | ✅ PASS |
| **III. Frequent Atomic Commits** | Phases map to independently committable units, each leaving the suite green. | ✅ PASS |
| **IV. Makefile-Driven Build** | No new entry points; new capability is a CLI subcommand reached through existing `make` targets. | ✅ PASS |
| **V. Simplicity & Refactorability** | **No new abstraction is introduced.** The `ImsTransport` seam from spec 015 already carries this feature. Changes to `ims::call` are additive — new optional configuration and new reporting — so the existing Wi-Fi diagnostic path keeps its behaviour. One deliberate deferral is recorded below rather than built speculatively. | ✅ PASS |

**Post-Phase-1 re-check**: ✅ Still passing. The design adds one module
(`ims::media_stats`) and extends two existing structs; it introduces no traits,
no indirection layers, and no new processes.

## Gates

### Gate C1 — Does the network actually prioritise the call? *(blocks US2's conclusion, not its implementation)*

Research R4 verified the modem *reports* the quality class; it did not — and
could not, without a call — verify that a **class-1 entry appears while a call
is up**. That is the measurement the entire quality argument rests on.

**Exit criteria**: from one live call, either a class-1 entry is observed
appearing for the call's duration and disappearing after, or its absence is
confirmed across repeated calls.

**Either outcome is a valid result.** If no class-1 entry ever appears, the
bridge's audio is being carried as ordinary data, the expected quality gain may
not materialise, and *that is the feature's finding* — the spec explicitly
permits it (spec Assumptions). What is not acceptable is shipping without
knowing.

**Why it is not a blocking gate**: unlike spec 015's Gate G1, this does not
prevent implementation. The sampling code is worth building regardless, because
reporting a confirmed absence is as useful as reporting a presence. It gates
the *conclusion*, not the work.

### Gate C2 — Wideband codec present in the running build *(blocks any quality judgement)*

The wideband codec sits behind the `amr-linked` build feature. It is **linked
in the container image** and **absent from a plain local build** (verified
during specification). A quality judgement made on a narrowband fallback would
be meaningless.

FR-010 requires this to be detected before dialling — `amr_safe::is_available()`
already exposes it — and any live validation must run the container build.

## Project Structure

### Documentation (this feature)

```text
specs/016-volte-calls/
├── plan.md              # This file
├── spec.md              # Feature specification (clarified)
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/
│   ├── volte-call-cli-contract.md   # CLI surface
│   └── media-report-contract.md     # What every call must report
├── checklists/
│   └── requirements.md
└── tasks.md             # Phase 2 output (/speckit-tasks — NOT created here)
```

### Source Code (repository root)

```text
gsm-sip-bridge/src/
├── ims/
│   ├── call.rs            # MODIFY: speech source, media stats, one-way verdict,
│   │                      #   end-reason reporting. Additive — ims-call unchanged
│   ├── media_stats.rs     # NEW: loss, jitter, one-way verdict (pure, testable)
│   ├── rtp.rs             # MODIFY: expose what parse_packet already extracts
│   ├── sdp.rs             # UNCHANGED — already negotiates the carrier's formats
│   ├── amr_rtp.rs         # UNCHANGED
│   └── transcode.rs       # UNCHANGED
│
├── volte/
│   ├── qos.rs             # NEW: sample the modem's per-context quality class
│   ├── mod.rs             # MODIFY: call orchestration over the LTE transport
│   ├── guard.rs           # REUSED as-is — must also cover the call command
│   └── registration.rs    # UNCHANGED
│
├── config/mod.rs          # MODIFY: per-card voice-path selection (FR-023/024)
├── cli.rs                 # MODIFY: `volte-call` subcommand
└── main.rs                # MODIFY: handler

gsm-sip-bridge/tests/
└── test_media_stats.rs    # NEW: loss/jitter/one-way verdict against synthetic streams
```

**Structure Decision**: No new crate, no new abstraction. `ims::media_stats`
is a new module because loss/jitter/verdict logic is pure and deserves to be
testable in isolation; `volte::qos` is new because AT-based quality sampling
belongs with the other modem interaction, not in the shared call code. Both
`ims::call` changes and the `volte` additions are strictly additive, which is
what keeps FR-019 (one shared call implementation) and FR-020 (Wi-Fi path
unchanged) simultaneously true.

## Complexity Tracking

> No Constitution violations to justify.

Feature 015 introduced the one abstraction this work needed (`ImsTransport`),
and it carries this feature unchanged. Two things were deliberately **not**
built, recorded so they are not added speculatively:

| Deferred | Why |
|---|---|
| Sharing one live registration between a maintenance loop and call handling | The diagnostic call owns its registration (research R1). Handing a live transport with installed security state between processes is real work that buys nothing for a one-shot call — and it is the *central* problem of the follow-up bridging feature, which is why that feature must be a single process |
| Symmetric measurement of the modem-internal path | Confirmed out of scope during clarification. The bridge receives already-decoded audio there and cannot obtain most of what FR-012/FR-013 require; promising a like-for-like comparison would promise a rigour the old path cannot supply. The operator compares by ear |

## Implementation Phasing

Ordered so each phase leaves a green, committable tree, and so the pure,
hardware-independent work lands before anything that needs a carrier.

| Phase | Delivers | Stories | Gate |
|---|---|---|---|
| 1 | `ims::media_stats` — loss, jitter, one-way verdict, fully unit-tested | US2, US3 (foundation) | — |
| 2 | `volte::qos` — quality-class sampling over AT, on a second port | US2 (foundation) | — |
| 3 | Speech-like audio source + `--audio`/`--tone` selection | US1 | **C2** |
| 4 | `volte-call` command: place the call over the LTE transport, wire in stats and QoS sampling, report the end reason | US1, US3 | — |
| 5 | **First live call** — settles Gate C1 and spec 015's R9 | US1, US2 | **C1** |
| 6 | Per-card voice-path selection | US4 | — |

Phases 1–2 are pure and testable without hardware, so they carry most of the
requirement surface with none of the carrier risk. Phase 5 is where the
feature's actual question gets answered.

## Notes carried forward

- **Do not use `samples/` as test audio.** It contains real call recordings
  named after real subscriber numbers (research R3). Sending one over a live
  carrier to a test number would be a privacy problem. Those files should stay
  out of the repository deliberately rather than by accident.
- **AT port contention is real.** The registration loop owns one AT port; QoS
  sampling during a call must use another (`ttyUSB5`/`ttyUSB6` are both usable,
  spec 015 research R5).
- **The registration loop and the call command cannot run at once** (research
  R1). The existing lock and VoWiFi guard must cover the call command, and the
  operator-facing message must say so plainly.
