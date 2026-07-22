# Tasks: Voice Calls over the Host-Side LTE Registration

**Input**: Design documents from `/specs/016-volte-calls/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: Test tasks ARE included. The project constitution makes
integration-first testing non-negotiable (Principle I) and requires a green
suite on every commit (Principle II).

**Organization**: Grouped by user story, in the spec's priority order. US1 and
US2 are both P1; US1 is sequenced first only because US2 measures the call US1
places.

**Status**: Generated before implementation. Every task is unchecked —
verified against the tree: `ims/echo.rs`, `ims/media_stats.rs` and
`volte/qos.rs` do not exist and `volte-call` is not in the CLI.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story this task belongs to

## Path Conventions

Single Rust workspace. Source at `gsm-sip-bridge/src/`, integration tests at
`gsm-sip-bridge/tests/`.

---

## Phase 1: Setup

- [ ] T001 Verify the wideband codec is linked in the container image (**Gate C2**) — `ldd` the shipped binary for `libopencore-amrnb`, `libopencore-amrwb`, `libvo-amrwbenc`
- [ ] T002 [P] Add `samples/` to `.gitignore` so the real subscriber call recordings stay out of the repository deliberately rather than by accident (research R3)

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: The pure signal and statistics logic every user story depends on.
**MUST complete before any user story.** All of it is hardware-independent and
carrier-independent, which is why it comes first — it carries most of the
requirement surface with none of the carrier risk.

### Media statistics — `ims::media_stats`

- [ ] T003 [P] Create `gsm-sip-bridge/src/ims/media_stats.rs` and register it in `gsm-sip-bridge/src/ims/mod.rs`
- [ ] T004 Expose the sequence number and arrival time `rtp::parse_packet` already extracts but discards, in `gsm-sip-bridge/src/ims/rtp.rs`
- [ ] T005 Implement receive-side sequence tracking: expected-vs-observed, deriving lost packets, in `gsm-sip-bridge/src/ims/media_stats.rs`
- [ ] T006 Distinguish reordered packets from lost ones in `gsm-sip-bridge/src/ims/media_stats.rs`
- [ ] T007 Implement inter-arrival jitter over a clock-injectable time source in `gsm-sip-bridge/src/ims/media_stats.rs`
- [ ] T008 Implement the one-way verdict as a **ratio** of received to sent, per direction, with a documented default threshold of 10% (FR-016, FR-028) in `gsm-sip-bridge/src/ims/media_stats.rs`
- [ ] T009 Implement round-trip delay estimation, available because the outbound audio *is* the inbound audio (research R3) in `gsm-sip-bridge/src/ims/media_stats.rs`

### Echo — `ims::echo`

- [ ] T010 [P] Create `gsm-sip-bridge/src/ims/echo.rs` and register it in `gsm-sip-bridge/src/ims/mod.rs`
- [ ] T011 Implement the echo path: return received audio attenuated below unity (FR-025) in `gsm-sip-bridge/src/ims/echo.rs`
- [ ] T012 Implement a re-echo suppression window so a returned signal is not returned again in `gsm-sip-bridge/src/ims/echo.rs`
- [ ] T013 Implement the independent generated marker, emitted on an interval **regardless of what was received** (FR-029) in `gsm-sip-bridge/src/ims/echo.rs`
- [ ] T014 Reuse the existing three-tone pattern from `gsm-sip-bridge/src/ims/call.rs` as the marker source rather than generating a second signal

### Tests

- [ ] T015 [P] Create `gsm-sip-bridge/tests/test_media_stats.rs` covering the media-report contract's 9 statistics tests against synthetic packet streams
- [ ] T016 [P] Test that a long call with a tiny absolute receive count **fails**, proving an absolute floor would have wrongly passed it, in `gsm-sip-bridge/tests/test_media_stats.rs`
- [ ] T017 [P] Test that a short, proportionally-healthy call **passes**, proving length-independence, in `gsm-sip-bridge/tests/test_media_stats.rs`
- [ ] T018 [P] Create `gsm-sip-bridge/tests/test_echo.rs`: attenuation is below unity, suppression prevents re-echo, echoed output matches input
- [ ] T019 **Test the FR-029 invariant explicitly**: with a receive stream of zero, outbound audio is still non-zero, so the verdict is `SendOnly` and never `Neither` — in `gsm-sip-bridge/tests/test_echo.rs`

---

## Phase 3: User Story 1 — Place a call and exchange audio (P1)

**Goal**: A real call to a real number, with the answering party hearing their
own voice returned.

**Independent test**: One command; the phone rings, a person answers and
speaks, they hear themselves, and a recording of what arrived exists.

- [ ] T020 [US1] Add `echo_attenuation` and `marker_interval` to `CallConfig`, with defaults, in `gsm-sip-bridge/src/ims/call.rs`
- [ ] T021 [US1] Replace the outbound test-pattern generator with the echo path plus marker, keeping the change additive so `ims-call` is unaffected, in `gsm-sip-bridge/src/ims/call.rs`
- [ ] T022 [US1] Add `end_reason` (duration elapsed / far end hung up / operator interrupted / attachment lost) to `CallOutcome` (FR-005) in `gsm-sip-bridge/src/ims/call.rs`
- [ ] T023 [US1] End the call early when the far end hangs up rather than holding it open for the remaining duration (FR-027) in `gsm-sip-bridge/src/ims/call.rs`
- [ ] T024 [US1] Detect a missing wideband codec **before dialling** via `amr_safe::is_available()` and refuse with the reason (FR-010, **Gate C2**) in `gsm-sip-bridge/src/ims/call.rs`
- [ ] T025 [US1] Report the audio formats offered when the carrier refuses them all (FR-009) in `gsm-sip-bridge/src/ims/call.rs`
- [ ] T026 [US1] Implement call orchestration over the LTE transport in `gsm-sip-bridge/src/volte/mod.rs`
- [ ] T027 [US1] Add the `volte-call` subcommand per the CLI contract in `gsm-sip-bridge/src/cli.rs`
- [ ] T028 [US1] Wire the handler, resolving the P-CSCF in the same order as `volte-register`, in `gsm-sip-bridge/src/main.rs`
- [ ] T029 [US1] Refuse before dialling when a Wi-Fi calling agent is running or another host-side registration holds the lock (FR-022), reusing `gsm-sip-bridge/src/volte/guard.rs`
- [ ] T030 [US1] Make the refusal message name the remedy — stop the registration loop, run the call, restart it (research R1) — in `gsm-sip-bridge/src/main.rs`

### Tests

- [ ] T031 [P] [US1] Test that a missing wideband codec is refused before any dial is attempted, in `gsm-sip-bridge/tests/test_echo.rs`
- [ ] T032 [US1] Verify `ims-call` behaviour is unchanged after the shared call path is modified (FR-020)

---

## Phase 4: User Story 2 — Establish whether the audio is actually better (P1)

**Goal**: Evidence, not assertion. This is the question the whole effort
exists to answer.

**Independent test**: After a call the operator has a report of what happened
to the media and how the network treated it, plus a recording to listen to.

### Quality-class sampling — `volte::qos`

- [ ] T033 [P] [US2] Create `gsm-sip-bridge/src/volte/qos.rs` and register it in `gsm-sip-bridge/src/volte/mod.rs`
- [ ] T034 [US2] Parse `AT+CGEQOSRDP` into per-context quality classes in `gsm-sip-bridge/src/volte/qos.rs`
- [ ] T035 [US2] Sample before, during and after the call and report the change (FR-014) in `gsm-sip-bridge/src/volte/qos.rs`
- [ ] T036 [US2] Use a **second AT port** for sampling so it does not collide with call control (research R4) in `gsm-sip-bridge/src/volte/qos.rs`
- [ ] T037 [US2] Report an explicit, reasoned `unavailable` naming what was asked when the modem declines (FR-026) — never a silent omission — in `gsm-sip-bridge/src/volte/qos.rs`

### The media report

- [ ] T038 [US2] Assemble the `MediaReport` from statistics, format and quality-class observations in `gsm-sip-bridge/src/ims/call.rs`
- [ ] T039 [US2] Report the negotiated format and whether it is wideband (FR-011) in `gsm-sip-bridge/src/ims/call.rs`
- [ ] T040 [US2] Report sent and received volumes separately, in comparable units (FR-012) in `gsm-sip-bridge/src/ims/call.rs`
- [ ] T041 [US2] **Report an answered call whose verdict is not `BothWays` as a failure, not a success** (FR-016) in `gsm-sip-bridge/src/main.rs`
- [ ] T042 [US2] Exit non-zero on a one-way or silent call so it cannot pass in a script, in `gsm-sip-bridge/src/main.rs`
- [ ] T043 [US2] Render the operator-facing report in the shape `quickstart.md` documents, in `gsm-sip-bridge/src/volte/mod.rs`

### Tests

- [ ] T044 [P] [US2] Test `AT+CGEQOSRDP` parsing against the verbatim hardware transcript `+CGEQOSRDP:3,5,0,0,0,0`, in `gsm-sip-bridge/src/volte/qos.rs`
- [ ] T045 [P] [US2] Test that a class-1 entry present only during the call is reported as preferential handling established, in `gsm-sip-bridge/src/volte/qos.rs`
- [ ] T046 [P] [US2] Test that a class-1 entry never appearing is reported as a **result**, not an error, in `gsm-sip-bridge/src/volte/qos.rs`
- [ ] T047 [P] [US2] Test that a modem refusal produces `unavailable` with a reason, in `gsm-sip-bridge/src/volte/qos.rs`

---

## Phase 5: User Story 3 — Diagnose a failed call by stage (P2)

**Goal**: A failure names where it broke, without re-running under
instrumentation.

**Independent test**: Induce each failure stage and confirm each produces a
distinct, accurate report.

- [ ] T048 [US3] Refuse to dial when there is no accepted registration, reporting that as the cause (FR-006) in `gsm-sip-bridge/src/volte/mod.rs`
- [ ] T049 [US3] Report the reason the network gave when it rejects the call (FR-018) in `gsm-sip-bridge/src/ims/call.rs`
- [ ] T050 [US3] Distinguish busy, no-answer and rejected as separate outcomes rather than a generic failure (US1 scenario 4) in `gsm-sip-bridge/src/ims/call.rs`
- [ ] T051 [US3] Detect the attachment being lost mid-call and report it as distinct from the far end hanging up (FR-017) in `gsm-sip-bridge/src/volte/mod.rs`
- [ ] T052 [US3] Attribute every failure to a named stage per the CLI contract (FR-017) in `gsm-sip-bridge/src/main.rs`

### Tests

- [ ] T053 [P] [US3] Test that each stage maps to a distinct reported cause, in `gsm-sip-bridge/tests/test_media_stats.rs`

---

## Phase 6: First Live Call — settles the feature's actual question

**Gates**: C1 (does the network prioritise the call), C3 (does echo feed back)

These cannot be done earlier and cannot be simulated. Everything above is
hardware-independent; this phase is where the feature's premise is tested.

- [ ] T054 Stop the registration loop, since the call command owns its own registration (research R1)
- [ ] T055 Baseline the quality class at idle — expect class 5 on the IMS context (research R4)
- [ ] T056 **Place the first live call to +919789063708 with a handset at the far end**, speak, and confirm the echo is audible
- [ ] T057 **Gate C1**: sample the quality class during the call on a second AT port; record whether a class-1 entry appears. **A confirmed absence is a valid result** and must be recorded as such
- [ ] T058 **Gate C3**: confirm no feedback on a handset call; characterise the speakerphone case so the limitation is documented rather than discovered by a user
- [ ] T059 Settle spec 015 research R9 — confirm which source address the network actually routes, now that media proves it
- [ ] T060 Verify SC-006: a 30-second unattended call with continuous two-way audio, ending without operator intervention
- [ ] T061 Verify direction attribution on hardware: stay silent for a whole call and confirm the verdict is `SendOnly`, never `Neither` (FR-029)
- [ ] T062 Record the findings — including any negative result — in `specs/016-volte-calls/research.md`

---

## Phase 7: User Story 4 — Choose which voice path a card uses (P3)

**Goal**: Select per card, defaulting to the existing modem-internal path.

- [ ] T063 [US4] Add per-card voice-path selection to the `[volte]` section in `gsm-sip-bridge/src/config/mod.rs`
- [ ] T064 [US4] **Default to the modem-internal path when unset** (FR-024) — this is what makes the feature safe to merge — in `gsm-sip-bridge/src/config/mod.rs`
- [ ] T065 [P] [US4] Document the selection in `config.toml.example`
- [ ] T066 [P] [US4] Test that an absent selection yields the modem-internal path, in `gsm-sip-bridge/src/config/mod.rs`

---

## Phase 8: Polish & Cross-Cutting Concerns

- [ ] T067 Verify the Wi-Fi calling suite passes unmodified and a **live Wi-Fi call completes** (SC-007) — still outstanding from feature 015
- [ ] T068 [P] Add VoLTE call metrics alongside the registration gauges in `gsm-sip-bridge/src/metrics/mod.rs`
- [ ] T069 [P] Document `volte-call` in `docs/operations.md`
- [ ] T070 Keep `make lint` clean and the workspace suite green on every commit (Constitution II)
- [ ] T071 [P] Update `specs/016-volte-calls/quickstart.md` with anything the first live call proved wrong

---

## Dependencies

```
Phase 1 (Setup)
      ↓
Phase 2 (Foundational — media_stats + echo, all pure)   ← blocks everything
      ↓
Phase 3 (US1 — place the call, P1)
      ↓
Phase 4 (US2 — measure it, P1)          ← needs a call to measure
      ↓
Phase 5 (US3 — stage diagnostics, P2)
      ↓
Phase 6 (First live call)               ← Gates C1, C3
      ↓
Phase 7 (US4 — per-card selection, P3)  ← independent; could run earlier
      ↓
Phase 8 (Polish)
```

**Story independence**: US1 is the only story that must come first. US2
measures US1's call, so it cannot precede it. US3 is independent of US2 and
could be built in parallel. US4 touches only configuration and is independent
of everything else.

## Parallel Opportunities

- **Phase 2**: T003–T009 (`media_stats`) and T010–T014 (`echo`) are separate
  files with no shared state — two people, or two sessions, can work in
  parallel. T015–T019 test different modules.
- **Phase 4**: T044–T047 are independent parsing tests.
- **Phase 7 / 8**: T065, T066, T068, T069, T071 touch disjoint files.

## Implementation Strategy

**MVP**: Phases 1–3 (US1). A real call over the bridge's own registration with
the far end hearing their own voice. Independently valuable as a diagnostic
even before any measurement exists.

**Increment 2**: Phase 4 (US2) — the evidence that answers the quality
question.
**Increment 3**: Phases 5–6 — diagnostics, then the live call that settles the
gates.
**Increment 4**: Phases 7–8.

**Sequencing note**: Phases 2–5 are entirely hardware-independent and
carrier-independent. They can be built and fully tested without a SIM, a
modem, or a carrier — which is unusual for this project and is the reason the
phasing front-loads them. Only Phase 6 needs the network, and it is where the
feature's premise is actually tested.

---

# Risk Register

Carried from `plan.md` so the gates are visible next to the work.

| Gate | Question | Blocks | If it fails |
|---|---|---|---|
| **C1** | Does the network give the call preferential handling? | The *conclusion* of US2, not the work | The audio is carried as ordinary data and the quality gain may not materialise. **A legitimate finding the spec permits** — what is unacceptable is shipping without knowing |
| **C2** | Is the wideband codec in the running build? | Any quality judgement | A judgement made on a narrowband fallback is meaningless. Verified present in the container image, absent locally |
| **C3** | Does echo feed back? | Unattended use | Attenuation and suppression should bound it; a speakerphone becomes a documented limitation, not a signal-processing project |

**The most important single rule in this feature**: an answered call whose
direction verdict is not `BothWays` is a **failure**, not a success (T041,
T042). The previous one-way-audio incident was painful precisely because a
broken call looked like a working one.

**The subtlest**: T013 and T019. Echo makes outbound audio depend on inbound,
which would silently destroy direction attribution. The independent marker is
what prevents that, and T019 is the test that proves it — an implementation
that dropped the marker would pass every other test in this list.
