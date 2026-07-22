# Quickstart: Voice Calls over the Host-Side LTE Registration

**Feature**: `016-volte-calls` | **Date**: 2026-07-22

How to prepare, place the first call, and answer the question the feature
exists for.

## Prerequisites

| Item | Requirement |
|---|---|
| Registration | `specs/015-volte-host-ims` complete — verified `200 OK` on Vi India |
| Hardware | Quectel EC200U, Vi India SIM, host interface `enx024bb3b9ebe5` |
| P-CSCF | `2400:5200:a100:819::6`, picked up automatically from the ePDG capture |
| Build | **The container build.** A plain local build lacks the wideband codec |
| Privilege | Everything runs in the privileged container; the dev host has no root |
| Test number | `+919789063708`, answered by a person |

### Stop the registration loop first

The call command owns its own registration (research R1), so it cannot run
alongside `volte-register`:

```bash
docker rm -f volte-soak      # or whatever is holding the registration
```

Leaving it running produces a refusal, not a confusing failure — but it is
still a wasted attempt.

### Confirm the wideband codec is present — Gate C2

```bash
docker run --rm --entrypoint sh docker-gsm-sip-bridge:latest -c \
  'ldd /usr/local/bin/gsm-sip-bridge | grep -E "amr|opencore"'
```

Expect `libopencore-amrnb`, `libopencore-amrwb`, `libvo-amrwbenc`. If they are
absent, **stop** — any quality judgement made on a narrowband fallback is
meaningless, and `volte-call` should refuse to dial anyway (FR-010).

## Step 1 — Baseline the connection's quality class

Before any call, record what the modem reports at idle. This is the "before"
sample and the baseline for Gate C1.

```bash
# Use a second AT port; the call will occupy the first
AT+CGEQOSRDP        ->  +CGEQOSRDP:3,5,0,0,0,0     # context 3, class 5 = IMS signalling
AT+CGEQOS?          ->  per-context configured classes
AT+CGACT?           ->  which contexts are active
```

Class **5** on the IMS context is the expected idle baseline (verified,
research R4). Note the active context list — a dedicated voice context would
be an *addition* to it.

## Step 2 — Place the first call — Gate C1

```bash
docker run --rm --privileged --network host -v /dev:/dev \
  docker-gsm-sip-bridge:latest \
  gsm-sip-bridge volte-call \
    --callee +919789063708 \
    --modem /dev/ttyUSB0 --iface enx024bb3b9ebe5 \
    --duration 30 --record /tmp/far-end.wav --record-sent /tmp/sent.wav
```

**Answer the phone with a HANDSET, not a speakerphone**, and **talk**.

You will hear **your own voice echoed back** over the full round trip. That is
the test: people are acutely sensitive to distortion, delay and dropouts in
their own voice, so you can judge the call directly while speaking naturally —
no script, and no audio files involved anywhere.

A speakerphone can feed back, because the microphone hears the returned audio
and sends it round again (Gate C3). Attenuation and re-echo suppression should
hold it, but a handset removes the question.

**What you are listening for**:

| What you hear | What it means |
|---|---|
| Your voice, clear, with a noticeable delay | Working. The delay is the round trip and is expected |
| Your voice, but muffled or watery | Codec or transcoding problem — check the negotiated format |
| Your voice cutting in and out | Loss or jitter — check those figures in the report |
| A periodic beep and nothing else | Nothing of yours is reaching us; the beep is the independent marker |
| Nothing at all | Neither direction is working, or the call never carried media |

### While it is up, sample the quality class again

On the **second** AT port (`/dev/ttyUSB5` or `/dev/ttyUSB6`), so it does not
collide with call control:

```bash
AT+CGEQOSRDP        # look for a class-1 entry that was not there before
AT+CGACT?           # look for an additional active context
```

**This is Gate C1.** A class-1 entry appearing for the call's duration and
gone afterwards means the network gave the call conversational-voice treatment.
Its absence means the audio is being carried as ordinary data — **a valid
finding**, and the one that would mean the expected quality gain may not
materialise. Record whichever happens.

## Step 3 — Read the report

```
call:            answered, ended by duration
format:          <name> (wideband: yes/no)
sent:            <n> packets / <n> samples
received:        <n> packets / <n> samples
loss:            <n> (<n> reordered)
jitter:          <n> ms
direction:       both-ways | send-only | receive-only | neither
network class:   before=5  during=?  after=5
recording:       /tmp/far-end.wav
```

**Read `direction` first.** Anything other than `both-ways` means the call
failed, regardless of how healthy the rest looks — and it names which side.
`send-only` says the carrier isn't sending or we can't decode it;
`receive-only` says our uplink isn't getting out.

## Step 4 — Answer the actual question

```bash
# Listen. This is not optional — measurements can look healthy while audio is unusable.
play /tmp/far-end.wav
```

Then judge, using both the recording and the report:

| Signal | What it tells you |
|---|---|
| Echo sounds clear, wideband format, class-1 present | The goal is met |
| Recording sounds clear, but no class-1 entry | It works, but without prioritisation — quality may degrade under load |
| Narrowband format negotiated | The judgement is not valid; find out why the wideband offer was refused |
| Low loss and jitter but poor audio | Suspect the codec path or the jitter handling, not the network |
| High loss or jitter | The network path is the problem |

Compare against the modem-internal path **by ear** — that comparison is
deliberately manual (clarification), because the old path cannot supply these
measurements.

## Step 5 — Validate the rest

```bash
# US1 — outcomes other than "answered"
volte-call --callee <number-that-rejects>       # expect a named rejection reason
volte-call --callee <number-nobody-answers>     # expect "no answer", not a generic failure

# US3 — stage attribution
volte-call --callee +919789063708 --pcscf <wrong-address>   # expect a named stage
# start volte-register in parallel, then:
volte-call --callee +919789063708               # expect refusal naming the conflict

# echo behaviour
volte-call --callee +919789063708 --echo-attenuation 0.5   # quieter return
volte-call --callee +919789063708 --marker-interval 2      # more frequent marker

# direction attribution: stay silent for the whole call.
# Expect SendOnly (the marker is still going out), never Neither.
volte-call --callee +919789063708 --duration 30
```

### Success criteria mapping

| Check | Criterion |
|---|---|
| Phone rings and can be answered with one command | SC-001 |
| Far end hears their own voice returned; recording contains their speech | SC-002 |
| Report states direction, and which side failed | SC-003 |
| Recording + measurements support a quality judgement | SC-004 |
| Every induced failure names its stage | SC-005 |
| 30s unattended call, continuous two-way audio | SC-006 |
| Wi-Fi suite passes unchanged **and a live Wi-Fi call completes** | SC-007 |
| Call setup, format negotiation and audio exist once | SC-008 |

## Warnings

**No audio files are involved at all.** The test signal is the far end's own
voice, returned. This is deliberate: the only recordings to hand are in
`samples/`, which holds real calls named after real subscriber numbers
(research R3), and sending one over a live carrier would be a privacy problem
rather than a test. With echo there is nothing to choose, so there is nothing
to choose wrongly.

**Use a handset at the far end.** Echoing into a speakerphone can feed back.

**Restart the registration loop afterwards** if you had one running — the call
command does not restore it.

**The attachment can drop mid-call.** The carrier tears the IMS connection down
periodically (spec 015 research R15). A call in progress will fail, and the
report should say the attachment ended it rather than blaming the far end.
