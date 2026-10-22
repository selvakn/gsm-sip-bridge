# Spike #111, phase 1: the three blocking risks

**Date**: 2026-10-22 · **Issue**: #111 (option 2b of #109) · **pjproject**: 2.16, the version pinned in `docker/Dockerfile`

**Constraint**: no patches to pjsip. Only config-site macros, build or link flags, and public pjsua/pjmedia API are allowed.

## Verdict

**No-go for option 2b as framed.** Of the three risks:
- **Risk 1** (passthrough registration) works for AMR-NB through public API.
- **Risk 1** is **blocked for AMR-WB**, the codec carriers actually use on VoWiFi/VoLTE, because pjmedia 2.16 has no AMR-WB passthrough codec.
- **Risk 2** (ringback) is harmless in Agent B but breaks the GSM path.
- **Risk 3** (format matching) shows the switchboard runs **one clock rate per process** and can't connect passthrough to PCM. Today's per-call transcode fallback can't live in the same process as a passthrough call.

Recommendation: do not pursue option 2b. Pursue **option 2a** (its PJSIP hook is proven in [phase 2](findings-phase2.md)) (PJSIP for signalling only, plus a Rust RTP forwarder in Agent B). It sidesteps all three risks:
- no pjmedia codec table;
- no process-global conference mode;
- AMR-WB framing handled by `ims/amr_rtp.rs`, which already exists.

The details below are kept as input for that spec.

| Risk | Result | Without patching pjsip? |
|---|---|---|
| 1. Passthrough codec registered and used, with no sound device | AMR-NB: frames pass **bit-identical** (548/548). AMR-WB: impossible. | AMR-NB: yes, with 4 conditions. AMR-WB: **no** |
| 2. Ringback on the switchboard | Agent B: harmless. GSM path: the tone silently takes the call's place on slot 0. | Agent B: yes. GSM: needs the stock build |
| 3. Switchboard format/clock matching | Only same-format, same-rate pairs at the *conference* clock work | Yes, but every leg in the process must share one clock |

All lab runs were on loopback inside a throwaway container. Reproduce with `harness/run.sh`. Raw output is in `results/`.

The Vodafone rig was **not** needed: every risk was settled with synthetic AMR-NB from stock pjsua. Real carrier frames only matter for the phase-2 questions (CMR, SID/DTX), which are moot given the verdict.

## Lab setup

`harness/Dockerfile` builds pjproject 2.16 twice, configured the same way as `docker/Dockerfile`'s `pjsip-builder`:
- `stock`: `docker/pjsip-config-site.h`, i.e. today's build.
- `passthru`: the stock config plus `PJMEDIA_CONF_USE_SWITCH_BOARD 1` and `PJMEDIA_HAS_PASSTHROUGH_CODECS 1`. It also sets `PASSTHROUGH_CODEC_PCMU/PCMA/G722 0`; see finding 1d.

`harness/b.c` is a minimal "Agent B" that mirrors `pjsua-safe/src/endpoint.rs`:
- null sound device, conference clock 16 kHz;
- an inbound call held at 180 while it places the outbound call;
- the two-way `pjsua_conf_connect` pairing (endpoint.rs ~L949–990);
- `start_ringback_tone`'s exact sequence (endpoint.rs ~L1296–1356).

X (the veth stand-in, caller) and Y (the client/PBX stand-in, auto-answer, plays 8 s of synthetic speech) are stock `pjsua` processes. `amrdiff.py` checks AMR frames bit by bit from the pcap: speech bits Y sent versus speech bits B forwarded to X.

## Risk 1: passthrough registration without a sound device

### Findings

**1a. pjsua registers passthrough codecs only for formats a sound device reports.** In `pjsua_aud.c:235-282`, the `ext_fmt` list of every audio device is collected and passed to `pjmedia_codec_passthrough_init2()`. ALSA and the null device report none. `init2` then disables every codec that isn't in that list (`passthrough.c:396-412`). So the stock pjsua flow registers **zero** passthrough codecs (`results/r1-registration.txt`: 14 codecs, all the normal ones).

**1b. The public re-init works.** `pjmedia_codec_passthrough_deinit()` followed by `pjmedia_codec_passthrough_init2(endpt, {PJMEDIA_FORMAT_AMR})` adds a passthrough `AMR/8000/1`. It sits beside opencore's `AMR/8000/1`, so there are two codecs with the same ID.

**1c. With duplicate IDs, opencore wins, so the call is transcoded.** The stream comes up as PCM (`bps=128000`, `results/r1-calls.txt`). The two can't be told apart by priority, because `pjsua_codec_set_priority` matches the ID string. The only public fix is `pjmedia_codec_opencore_amr_deinit()`. That removes **both** opencore AMR-NB and AMR-WB from the process, so it also drops spec 085's transcoded AMR on the phone leg.

**1d. Turning passthrough on silently drops the software G.711 and G.722 codecs.** `pjmedia-codec/config.h:370-391` disables them whenever their passthrough twins are on, and those twins are on by default. The fix is config only: set `PJMEDIA_HAS_PASSTHROUGH_CODEC_{PCMU,PCMA,G722} 0`. Without it, Agent B would lose PCMU, which spec 085 R4 makes mandatory.

**1e. Passthrough and opencore do not link together.** `amr_helper.h` defines its tables non-`static` in a header, and both `passthrough.o` and `opencore_amr.o` include it. That produces duplicate symbols, and pjproject's own `make` fails. The workaround is link-only: `-Wl,--allow-multiple-definition` (the tables are identical). It would have to go into both the pjproject build and `pjsua-sys/build.rs`. The alternative is building without opencore (`--without-opencore-amr`).

**1f. Default packetisation is wrong for a call-to-call bridge.** Passthrough AMR defaults to `frm_per_pkt=2`. Against a 20 ms peer, B sent one speech frame plus one NO_DATA frame per 40 ms packet, at a 20 ms cadence (ts +320). The receiver's timeline ran at 2× and was half silence (`results/r1-calls.txt`, `rtptiming` output). The public fix: `pjsua_codec_set_param("AMR/8000")` with `setting.frm_per_pkt = 1`.

**1g. The conference clock must equal the codec clock.** At Agent B's 16 kHz conference clock, the 8 kHz AMR port was pulled twice per tick: 1096 packets in 11 s, alternating speech/NO_DATA (`results/r1-ptime20.txt`). At an 8 kHz conference clock, the stream was clean (`results/r1-clock8k.txt`):
- 549 packets in, 549 out;
- ts +160 on both legs;
- **548 consecutive speech frames bit-identical**;
- the audio X decoded reproduced the 1 s talk / 0.5 s pause pattern of the source.

**1h. There is no AMR-WB passthrough, and no public way to add one.**
- `passthrough.c:178-227`'s `codec_desc[]` lists only `AMR` (8000), G722, G729, iLBC, PCMU and PCMA. The AMR-WB branches in `codec_open`/`codec_decode` (`passthrough.c:707`, `:1016`) are unreachable.
- `pjmedia/format.h:79` defines `PJMEDIA_FORMAT_AMR` but no AMR-WB format ID. So no `init2` setting can enable it.
- The only route that doesn't patch pjsip is writing our own codec factory and registering it with the public `pjmedia_codec_mgr_register_factory`. That means reimplementing passthrough.c for AMR-WB, at which point PJSIP adds nothing but the switchboard (see risk 3).

### Verdict
- **AMR-NB:** works with public API. It needs all four of: opencore deinit, the passthrough re-init, `frm_per_pkt=1` and an 8 kHz conference clock. The build also needs 1d and 1e.
- **AMR-WB:** **blocked** without patching pjmedia or writing our own codec.

## Risk 2: ringback under the switchboard

### Findings

**2a. Agent B (paired calls): harmless.** The ringback sequence ran at the CALLING/EARLY points, both with passthrough AMR (8 kHz clock) and with PCMU (16 kHz clock). Every call returned 0:
- `tonegen_create`, `conf_add_port`;
- `conf_connect(tone, 0)`, `conf_disconnect`, `conf_remove_port`.

There was no assertion and no crash. The paired call still carried 547/547 bit-identical frames (`results/r2-ringback.txt`, R2a/R2b). In Agent B the tone feeds slot 0, which is the null device and has no other user, so it's inaudible on either build (unchanged from today).

**2b. GSM-style slot 0: the tone takes the call's place.**
- A switchboard sink accepts one transmitter, and a second connect silently disconnects the first (`conf_switch.c:595-617`, logged only at level 2).
- With call↔slot 0 connected, starting the tone left `slot 2 (tone) -> slot 0` as the only transmitter.
- Stopping the tone left slot 0 with **no transmitter**: the call's audio is gone until something reconnects it (`results/r2-ringback.txt`, R2d: `call->0 LOST`).

Whether this bites in practice depends on order:
- Today the tone starts at CALLING, before media. Media-active then displaces the tone (early media replaces ringback instead of mixing with it). The disconnect at CONFIRMED is then a no-op.
- Any path where the tone starts *after* the call is connected to slot 0 loses the downlink audio.

(The stock build's count reads 0 only because 2.16's mixing bridge queues connects asynchronously. It mixes, so there is no displacement there.)

**2c. `pjsua_conf_adjust_tx_level(0, 1.5)` works on the switchboard** for PCM ports (returns 0; applied at `conf_switch.c:1134`). On a non-PCM port it hits `PJ_ASSERT_RETURN(... == PJMEDIA_FORMAT_L16)` (`conf_switch.c:1052`). That aborts in a build with assertions on, but nothing in this repo calls it on a call slot.

### Verdict
Agent B: no problem. The circuit-switched GSM path must not run on a switchboard build:
- because of 2b;
- and because of risk 3: its 8 kHz ALSA slot 0 can't pair with a 16 kHz PBX leg.

So option 2b would need a **separate Agent B binary/image**, built against a second pjproject variant.

## Risk 3: switchboard format and clock matching

### Findings
`pjmedia_conf_connect_port` in `conf_switch.c:563-590` requires all of the following to be equal on source and sink, and never resamples or transcodes:
- format ID and `avg_bps`;
- clock rate;
- channel count;
- a compatible ptime.

The switchboard also does **not** keep a per-port clock. It pulls each port by the conference clock's sample count. A port whose rate differs from the conference clock is read 2× or ½× too often, even when the pair itself "matches".

Measured, all on the passthru build (`results/r3-matrix.txt`):

| Pair (veth leg ↔ PBX/phone leg) | Conf clock | connect | Media |
|---|---|---|---|
| L16/16k ↔ G.722 | 16k | ok | 50 pps each way ✔ |
| L16/16k ↔ PCMU | 16k | **ENOTCOMPATIBLE** | no bridging |
| PCMU ↔ G.722 | 16k | **ENOTCOMPATIBLE** | no bridging |
| PCMU ↔ PCMU | 16k | ok | **100 pps out** (2×) ✘ |
| L16/16k ↔ G.722 | 8k | ok | **25 pps out** (½×) ✘ |
| PCMU ↔ passthrough AMR | 8k | **ENOTCOMPATIBLE** | no bridging |
| L16/16k ↔ opencore AMR-WB | 16k | ok | 50 pps each way ✔ |

Notes:
- Mixed-rate pairs fail on the `avg_bps` check (256 000 vs 128 000) before the clock check is ever reached.
- `avg_bps` is per *negotiated mode* for passthrough AMR (`passthrough.c:798`, `stream.c:2019`). So two passthrough AMR legs also only connect if both ends settled on the same mode. In the lab both did (7.40 kbit/s).

### Verdict
In a switchboard process:
- every call leg must run at the conference clock;
- both legs of a pair must have the same format and the same rate.

So:
- **passthrough AMR-NB requires an 8 kHz process**, which excludes the L16/16k wideband path.
- **today's fallback pairs that change rate** (L16 ↔ PCMU, PCMU ↔ G.722) don't work at all.
- **narrowband PCMU ↔ PCMU breaks at the 16 kHz clock Agent B uses today.**

The per-call fallback can't share a process with passthrough. Supporting both means:
- choosing the Agent B process per call, before media;
- and running one Agent B per clock rate, each built against the switchboard variant;
- while the GSM path stays on the stock build (risk 2).

## Other facts surfaced (for the next spike)
- The passthrough encoder hardcodes `cmr = 15` (`passthrough.c:786`), so a carrier's CMR is never forwarded. #111 Q4 ("CMR preserved?") is answered from source: **no**.
- `pjsua_conf_connect` in switchboard mode may reopen the (null) sound device to match a peer's format whenever slot 0 is idle (`pjsua_aud.c:994-1069`). It was harmless in every run here.

## Why option 2a looks better
Every blocker above comes from routing audio through pjmedia:
- the codec table (no AMR-WB passthrough);
- the process-global conference mode (one clock, no mixing);
- the opencore/passthrough clash.

A Rust forwarder in Agent B, with PJSIP only negotiating SDP, avoids all three. It also reuses `ims/amr_rtp.rs`, which already repacketises octet-aligned ↔ bandwidth-efficient for both AMR-NB and AMR-WB. It keeps the stock pjproject build, the GSM path and today's transcode fallback untouched.
