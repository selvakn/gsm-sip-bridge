# Phase 0 Research: Voice Calls over the Host-Side LTE Registration

**Feature**: `016-volte-calls` | **Date**: 2026-07-22

Establishes what already exists, what has to be built, and what the hardware
will actually tell us. Everything marked **verified** was executed against the
live EC200U + Vi India SIM, or read directly from the tree.

---

## R1: How does the call obtain a registration? *(FR-002)*

**Decision**: The diagnostic call command **owns its registration for the
duration of the call**, exactly as `ims::call::run_call` does today. FR-002
means "do not register twice within one call attempt", not "attach to a
separately-running registration process".

**Status**: ✅ Decided. This was the feature's main open design question.

`run_call` today opens with:

```rust
let mut session = super::register_session(&cfg.register)?;
```

It registers, then places the `INVITE` on that same session — reusing the
session's transport, its Service-Route set (RFC 3608), and its protected
ports. That already satisfies the *intent* of FR-002: one registration, one
call, no second registration.

**Rationale**: the alternative — having the call attach to a registration held
by a separately-running `volte-register` — needs inter-process handover of a
live transport with installed signalling-security state. That is a genuinely
hard piece of work, and it buys nothing for a one-shot diagnostic call, which
is all this feature delivers.

**Consequence, and it is a real one**: a running `volte-register` and a
`volte-call` cannot coexist. Both hold the subscriber's registration, so the
existing lock and the VoWiFi mutual-exclusion guard must cover the call
command too (FR-022). The operator stops the registration loop, runs the call,
and restarts it. That is acceptable for a diagnostic; it is **not** acceptable
for the follow-up bridging feature, which must hold one long-lived
registration and place calls on it.

**Recorded for the follow-up**: sharing one live registration across
registration-maintenance and call-handling is the central design problem of
the bridging feature, and it is *why* that feature must be a single process
(spec Assumptions). Deferring it here is deliberate, not an oversight.

**Alternatives considered**:
- *IPC handover of the live session* — rejected for this feature's scope; it
  is the follow-up's core problem.
- *Placing the call without registering at all* — not viable; the network
  routes and authorises calls against the registration binding.

---

## R2: Media quality measurement *(FR-012, FR-013)*

**Decision**: Sent/received volume already exists. **Loss and jitter do not
and must be built.**

**Status**: ✅ Verified by code audit; new work identified.

| Requirement | Present today? |
|---|---|
| How much audio sent / received (FR-012) | ✅ `CallOutcome::Answered` already carries `recorded_samples` and `sent_samples`, separately per direction |
| Both directions recorded separately | ✅ `record_path` and `record_sent_path` |
| Sequence numbers available | ✅ `rtp::parse_packet` returns `seq` |
| **Loss / gap detection** | ❌ **Nothing tracks expected-vs-received sequence** |
| **Arrival jitter** | ❌ **Not computed** |

`rtp.rs` parses the sequence number and then discards it. So the raw material
for FR-013 is on the wire and simply not being used. The work is a
receive-side tracker: expected-vs-observed sequence to derive gaps and
reordering, and inter-arrival variance for jitter.

**Rationale**: FR-013 asks for "enough about the received audio's condition in
transit for an operator to judge quality without specialist tooling". Sample
counts alone cannot distinguish a clean call from one that arrived in bursts
with a third of it missing — both yield a similar total.

**Note**: this is pure, clock-injectable logic over parsed packets, so it is
unit-testable without hardware, unlike most of this feature.

---

## R3: What speech does the bridge send? *(FR-025)*

**Decision**: Generate a **speech-like signal** in code by default; accept a
real recording via an explicit option; keep the existing tone pattern
available.

**Status**: ✅ Decided, with a privacy constraint discovered during research.

### ⚠️ The obvious asset must not be used

The repository working tree contains `samples/` with **real call recordings**,
named after **real subscriber numbers**:

```
samples/Call_record_between_+9198947xxxxx_and_201_from_02-06-2026 ....mp3
samples/Call_record_between_+9199941xxxxx_and_201_from_02-06-2026 ....mp3
```

**These must not be used as the outgoing test audio.** Doing so would transmit
a real person's recorded conversation over a live carrier network to a test
number — a privacy problem, not a technical one. They are currently untracked;
they should stay out of the repository deliberately rather than by accident.

### What is sent instead

Today's outgoing audio is a deliberately-designed three-tone pattern
(440/660/880 Hz with silence gaps, `call.rs`), chosen so a dropout is audible
where a continuous sine would hide it. That reasoning is sound and the pattern
is kept — but a tone still cannot support a *quality* judgement, which is why
the spec's clarification changed the default.

Default becomes a synthetic **speech-like** signal: voiced formant structure,
varying pitch, a natural amplitude envelope, and pauses. It stresses a
speech-optimised codec far closer to real speech than steady tones do, needs
no asset and carries no licence or privacy question.

**Honest limitation**: synthetic speech-like audio is not real speech, and for
a final quality judgement a real sample is better. Hence the explicit option
to supply one — the operator can use their own voice recording, which carries
no third-party privacy issue.

---

## R4: Evidence that the network prioritised the call *(FR-014, FR-026)*

**Decision**: Sample the modem's per-context quality class before, during and
after the call, and report the change.

**Status**: ✅ **VERIFIED on hardware** — the command works and returns usable
data.

Probed on the second AT port (`/dev/ttyUSB5`, while the registration soak held
`/dev/ttyUSB0`):

```
AT+CGEQOSRDP=?   -> +CGEQOSRDP: (1..7)
AT+CGEQOSRDP     -> +CGEQOSRDP:3,5,0,0,0,0
AT+CGEQOS?       -> +CGEQOS: 1,6,... / 3,5,... / (others zero)
AT+CGACT?        -> +CGACT: 3,1
```

Context 3 — the IMS PDN — reports **quality class 5**, which is the class used
for IMS signalling. That is exactly right at idle, and it means the modem *is*
willing to report the class, which was the open question.

**The test**: a voice call should cause a **class-1 entry to appear** (the
class reserved for conversational voice), on a dedicated context, for the
duration of the call and not after. Sampling before/during/after and reporting
the delta is therefore both meaningful and implementable.

**Why this matters more than it looks**: this is the measurement that decides
whether the feature's quality goal is met at all. If no class-1 entry ever
appears, the bridge's audio is being carried as ordinary data and the expected
quality gain may not materialise — a legitimate finding the spec explicitly
permits.

**Unavailable, and therefore not attempted**: `AT+QCAINFO` and
`AT+QNWCFG="qoos"` both return errors on this firmware. There is no
lower-level radio view to fall back on, which is precisely why FR-026 requires
an explicit reasoned "undetermined" rather than silence.

**Note on port contention**: the probe deliberately used a second AT port
because the registration loop owns the first. `ttyUSB5` and `ttyUSB6` are both
usable AT ports (spec 015 research R5). Sampling during a call will need this,
since the call's own AT usage and the QoS sampling would otherwise collide.

---

## R5: Judging one-way audio *(FR-016, FR-028)*

**Decision**: Compare `recorded_samples` against `sent_samples` as a ratio,
per direction, with a documented default threshold of 10%.

**Status**: ✅ The counts already exist; only the judgement is new.

`CallOutcome::Answered` already carries both counts separately. What does not
exist is any verdict derived from them — a call that sent 30 seconds and
received 200 milliseconds is currently reported as `Answered`, i.e. success.

This is the exact failure that
`docs/incidents/2026-07-15-vowifi-oneway-audio.md` records on the Wi-Fi path,
and the lesson from it — that the *delta between directions* is what separates
"the carrier isn't sending" from "we can't decode what it sends" — is why the
spec requires the ratio rather than an absolute floor.

---

## R6: Which address does the media use? *(settles spec 015 research R9)*

**Decision**: No new mechanism. The RTP socket already inherits the
registration's chosen source address.

**Status**: ✅ Verified by code audit; the open question resolves itself.

```rust
let rtp_socket = UdpSocket::bind((session.local_addr.ip(), 0))
```

Media binds to whatever address the signalling transport selected, so audio
and signalling always agree. Spec 015 R9 asked which of the interface's two
addresses the network actually routes; **this feature answers it empirically**
— audio that is sent but never returns would be the symptom of having chosen
the wrong one.

---

## R7: Per-card path selection *(FR-023, FR-024)*

**Decision**: Extend the existing `[volte]` configuration with a per-card
voice-path choice, defaulting to the modem-internal path.

**Status**: ✅ Straightforward; the configuration section already exists from
spec 015 and the multi-card line resolution already exists from spec 013.

FR-024's "when no choice is made, use the modem-internal path" is what makes
this safe to merge: nothing changes for any existing deployment unless it is
asked for.

---

## R8: Non-regression surface

**Status**: ✅ Verified — the blast radius is small.

`run_call`/`CallConfig` have exactly **one** other caller: `main.rs`'s
`ims-call` command (the VoWiFi diagnostic). So changes to the call path touch
one existing command, and the requirement is that its behaviour is unchanged
(FR-020).

`ims::call` itself is shared, which is what FR-019 wants — one call
implementation serving both transports. The changes must therefore be
*additive* (new optional configuration, new reporting) rather than altering
existing defaults.

---

## Unresolved items carried into planning

| ID | Item | Blocking? | Where resolved |
|---|---|---|---|
| R4a | Whether a class-1 entry actually appears during a call | **Yes, for US2** | Gate C1 — first live call |
| R6a | Which source address the network routes (spec 015 R9) | No — media proves it either way | First live call |
| R2a | Whether the carrier's jitter/loss is low enough for the quality goal | No | Measured, not gated |
