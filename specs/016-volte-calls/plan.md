# Implementation Plan: Voice Calls over the Host-Side LTE Registration

**Branch**: `016-volte-calls` | **Date**: 2026-07-22 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/016-volte-calls/spec.md`

> **Revision (2026-07-22)**: replanned after a change of approach — the test
> call now **echoes the far end's audio back to them** rather than sending any
> audio sample. See research R3 for why this is better, and for the
> non-obvious consequence it forced (FR-029).

## Summary

Place a real outbound call over the cellular IMS registration built in
`specs/015-volte-host-ims`, with the bridge — not the modem — handling the
audio, and **measure the result well enough to answer whether the audio is
actually better**.

The test signal is the answering party's **own voice, returned to them**. That
needs no audio asset, and it is a stronger test than playing a recording:
people notice distortion, delay and dropouts in their own voice far more
readily than in a stranger's, the echo carries the degradation of *both*
directions at once, and round-trip delay becomes directly audible.

As with feature 015, most of the work is reuse. `ims::call::run_call` already
places outbound calls over the Wi-Fi path, `ims::sdp` already negotiates the
carrier's formats, and `CallConfig` already carries a call duration, separate
recordings per direction, and per-direction sample counts. The genuinely new
work is narrow:

1. **The echo path itself** — return received audio, attenuated, with a
   generated marker mixed in (research R3).
2. **Media condition measurement** — sequence numbers are parsed and discarded
   today; loss and jitter must be derived from them (research R2).
3. **Preferential-handling evidence** — sampling the modem's per-context
   quality class before, during and after the call (research R4).
4. **A verdict on one-way audio** — the counts exist, the judgement does not
   (research R5).

Phase 0 verified the riskiest premise on hardware: the modem **will** report
the quality class per context (`AT+CGEQOSRDP` → context 3, class 5). That was
the open question behind FR-014.

## Technical Context

**Language/Version**: Rust, toolchain pinned in `rust-toolchain.toml`; workspace-wide zero-`unsafe` policy enforced by `make lint`
**Primary Dependencies**: existing in-tree `ims/` stack (`call`, `sdp`, `rtp`, `amr_rtp`, `transcode`), `amr-safe` (behind the `amr-linked` feature), `volte` transport and guard from spec 015
**Storage**: existing SQLite store for call history; no new schema for the diagnostic command
**Testing**: `cargo test --workspace`; integration-first per Constitution I. The echo mixer, media statistics and one-way verdict are all pure and clock-injectable, so they are unit-testable without hardware — unusually for this project
**Target Platform**: Linux, inside the existing `privileged: true` + `network_mode: host` container
**Project Type**: Single Rust workspace — CLI plus long-running bridge daemon
**Performance Goals**: Real-time audio. Echo adds a return path in the media loop; it must not add enough delay to make the call unusable, and must sustain continuous two-way audio for at least 30s (SC-006)
**Constraints**: IMS PDN is IPv6-only; the wideband codec is behind a build feature, **absent from plain local builds**, present in the container image; the modem exposes one host data path; AT access must not collide with the registration loop's own AT usage
**Scale/Scope**: One call at a time, one modem, one SIM

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Assessment | Status |
|---|---|---|
| **I. Integration-First Testing** | The echo mixer, loss/jitter tracking and one-way verdict are pure over parsed packets and tested directly, with nothing mocked. Modem AT exchanges reuse the established wire-level simulation. The call itself cannot be tested without a carrier, so live validation is mandated in `quickstart.md` rather than faked. **Echo improves testability**: the expected output is a deterministic function of the input, so a synthetic input stream has an exactly predictable echo. | ✅ PASS |
| **II. Green-on-Commit** | `make format && make lint && make test` before every commit. No test may require a modem or a carrier to pass. | ✅ PASS |
| **III. Frequent Atomic Commits** | Phases map to independently committable units, each leaving the suite green. | ✅ PASS |
| **IV. Makefile-Driven Build** | No new entry points; the new capability is a CLI subcommand reached through existing `make` targets. | ✅ PASS |
| **V. Simplicity & Refactorability** | **No new abstraction.** The `ImsTransport` seam from spec 015 carries this feature unchanged. Echo *removes* planned complexity — no asset loading, no speech synthesis, no file format handling. Changes to `ims::call` are additive, so the existing Wi-Fi diagnostic keeps its behaviour. | ✅ PASS |

**Post-Phase-1 re-check**: ✅ Still passing. The design adds two small modules
and extends two existing structs; no traits, no indirection, no new processes.
The replan reduced scope rather than adding to it.

## Gates

### Gate C1 — Does the network actually prioritise the call? *(gates the conclusion, not the work)*

Research R4 verified the modem *reports* the quality class; it could not verify
that a **class-1 entry appears while a call is up**, which is the measurement
the entire quality argument rests on.

**Exit criteria**: from one live call, either a class-1 entry is observed
appearing for the call's duration and disappearing after, or its absence is
confirmed across repeated calls.

**Either outcome is valid.** If no class-1 entry ever appears, the bridge's
audio is being carried as ordinary data and the expected quality gain may not
materialise — a finding the spec explicitly permits. Shipping without knowing
is what is unacceptable.

It does not block implementation: the sampling code is worth building either
way, because a confirmed absence is as useful as a presence.

### Gate C2 — Wideband codec present in the running build *(blocks any quality judgement)*

The wideband codec sits behind the `amr-linked` build feature — **linked in the
container image, absent from a plain local build** (verified during
specification). A quality judgement made on a narrowband fallback is
meaningless. FR-010 requires detecting this before dialling;
`amr_safe::is_available()` already exposes it.

### Gate C3 — Echo does not feed back *(new; blocks unattended use)*

Returning audio to a device whose microphone can hear its own speaker forms a
loop that grows until it howls. Attenuation plus a short re-echo suppression
window should hold loop gain below unity, but this is a physical-world property
that cannot be proven in a unit test.

**Exit criteria**: a live call where the answering party uses a **handset**
completes without feedback; a deliberate speakerphone test is characterised so
the limitation is documented rather than discovered by a user.

## Project Structure

### Documentation (this feature)

```text
specs/016-volte-calls/
├── plan.md              # This file
├── spec.md              # Feature specification (clarified + echo revision)
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
│   ├── call.rs            # MODIFY: echo path, media stats, one-way verdict,
│   │                      #   end-reason reporting. Additive — ims-call unchanged
│   ├── echo.rs            # NEW: echo mixer — attenuation, re-echo suppression,
│   │                      #   independent marker injection (pure, testable)
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
├── test_media_stats.rs    # NEW: loss/jitter/one-way verdict vs synthetic streams
└── test_echo.rs           # NEW: attenuation, suppression, marker always present
```

**Structure Decision**: No new crate, no new abstraction. `ims::echo` and
`ims::media_stats` are separate modules because both are pure signal/statistics
logic that deserves isolated tests; `volte::qos` is separate because AT-based
sampling belongs with the other modem interaction rather than in shared call
code. All `ims::call` changes are additive, which is what keeps FR-019 (one
shared call implementation) and FR-020 (Wi-Fi path unchanged) simultaneously
true.

## Complexity Tracking

> No Constitution violations to justify.

The replan **reduced** complexity: no audio assets, no speech synthesis, no
file loading or format handling. Three things are deliberately not built:

| Deferred / rejected | Why |
|---|---|
| Sharing one live registration between a maintenance loop and call handling | The diagnostic call owns its registration (research R1). Handing a live transport with installed security state between processes buys nothing for a one-shot call — and is the *central* problem of the follow-up bridging feature, which is why that feature must be a single process |
| Symmetric measurement of the modem-internal path | Out of scope by clarification. That path yields already-decoded audio and cannot supply most of what FR-012/FR-013 require; promising a like-for-like comparison would promise a rigour it does not have. The operator compares by ear |
| Full acoustic echo cancellation | Far beyond scope. Attenuation plus re-echo suppression is enough to keep a handset call stable; a speakerphone is an operational limitation to document (Gate C3), not a signal-processing project |

## Implementation Phasing

Ordered so each phase leaves a green, committable tree, and so all pure work
lands before anything needing a carrier.

| Phase | Delivers | Stories | Gate |
|---|---|---|---|
| 1 | `ims::media_stats` — loss, jitter, one-way verdict, fully unit-tested | US2, US3 (foundation) | — |
| 2 | `ims::echo` — attenuation, re-echo suppression, always-present marker | US1 (foundation) | — |
| 3 | `volte::qos` — quality-class sampling over a second AT port | US2 (foundation) | — |
| 4 | `volte-call`: place the call over the LTE transport, wire in echo, stats and QoS sampling, report the end reason | US1, US3 | **C2** |
| 5 | **First live call** — settles C1, C3, and spec 015's R9 | US1, US2 | **C1, C3** |
| 6 | Per-card voice-path selection | US4 | — |

Phases 1–3 are pure and hardware-independent, so they carry most of the
requirement surface with none of the carrier risk. Phase 5 is where the
feature's actual question gets answered.

## Notes carried forward

- **No audio assets. At all.** Echo removes the need, and with it the
  temptation to reach for `samples/`, which holds real call recordings named
  after real subscriber numbers (research R3). Those files should be excluded
  from the repository deliberately rather than left merely untracked.
- **The independent marker is not optional** (FR-029). Without it, echo makes
  the two directions dependent and silently destroys the direction attribution
  that diagnosed the previous one-way-audio incident.
- **AT port contention is real.** The registration loop owns one AT port; QoS
  sampling during a call must use another (`ttyUSB5`/`ttyUSB6` are both usable,
  spec 015 research R5).
- **The registration loop and the call command cannot run at once** (research
  R1). The existing lock and VoWiFi guard must cover the call command, and the
  message must say so plainly.
- **Tell the operator to use a handset** (Gate C3).
