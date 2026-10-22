# Live verification — AMR passthrough (#111, spec 120)

Rig: the local Vodafone VoWiFi line (`test/`, built-in `[sip_server]`), a baresip
AMR-WB client (`harness/`, Ubuntu baresip with the `amr` module), and the Jio line on
the Pi (`[sip_server]` mode, test config) used as the *caller* through the carrier.
Images built from this branch: `gsm-sip-bridge:amrpt-<sha>-amd64` / `-arm64`.
Phone numbers are runtime-only and deliberately absent from this file.

## Local rig (Vodafone), inbound — T028

Call placed from baresip → Pi (Jio, outbound) → Jio → Vodafone → local bridge → baresip (AMR-WB, auto-answer).

| Check | Result |
|---|---|
| Path decided | `path=forwarded flavour=amr-wb carrier_packing=bandwidth-efficient client_packing=octet-aligned` (calls 1, 2, 4, 5, 6); Agent A `forwarding=true` |
| Bit-identical frames (pcap, hop `veth-sip0` vs client leg, `harness/rtpflows.py … frames`) | carrier→client **5112/5112** and **2710/2710** packets: same CMR, frame types, speech bits, SSRC and timestamp, though the packing differs (BE → OA); client→carrier **5134/5134**, **2734/2734** |
| DTMF client→carrier | 2177/2177 and 1051/1051 telephone-event packets, payloads identical (PT relabelled per leg) |
| DTMF carrier→client | not observed: no events arrived from the carrier side on this path (the caller's digits go baresip → Pi → Jio → Vodafone; the Pi's transcoded outbound leg does not appear to deliver RFC 2833 end to end). The same relabel in the other direction is unit-tested |
| Hold / resume (client `/hold`, `/resume`, and a plain `/reinvite`) | client leg reads `send_to_client=false` then `true`; with the gate closed the hop kept receiving carrier frames (3160) while 450 fewer were sent to the client (2710), audio resumed bit-exact afterwards |
| Soak | 6 min 46 s, three hold/resume cycles, a re-INVITE and digits per round: no warnings; Agent A verdict `both-ways`, `carrier_rx=20278`, far-end loss 0, `local_lost_packets=2` |
| Leaks | Agent A/B socket count: idle 9/9, in call 14/16, after hangup 9/9 |
| One-way monitor (FR-013) | counters feed Agent A's verdict exactly as before (`carrier_rx`/`pbx_rx`). A 40 s block of the client's uplink gave `pbx_rx=316` against `carrier_rx=2184`; the verdict stayed `both-ways` because the existing ratio threshold passes ~14 %, the same code path as a transcoded call |

### Found live and fixed
After a hold/resume the media-state callback connected a forwarded call to the sound
device (stream paused, harmless but wrong). Fixed in `pjsua-safe` (`is_registered` early
return); re-checked on the next image: 0 sound-device wiring lines across hold/resume.

### Carrier offers decide whether a call qualifies
Across six Jio→Vodafone calls the carrier's AMR-WB offer varied: most carried **no
`mode-set`** (unrestricted, forwarded); one carried `octet-align=1; mode-set=0,1,2,3`,
which the rule (FR-003 b) transcodes with `reason=mode_set` because the client is
unrestricted. This is the rule working as specified, but it bounds how often a real call
is forwarded. Reported to the owner as an open design point (propagating the carrier's mode-set into the client leg would widen it).

## Regression, SC-002 / T034 / T037 / T041 (local rig)

| Case | Result |
|---|---|
| Client answers PCMU only | `transcoded:client_not_amr`, `both-ways` (carrier_rx 1951 / pbx_rx 1851) |
| Setting off, recreate | `transcoded:disabled` for a call with an AMR client; no Forward* message exchanged |
| Setting on, recreate | `forwarded` again |
| Outbound from baresip (setting on) | log `media_path=transcoded:outbound`; audio both ways on the client leg (1431 / 1427 packets) |
| Circuit-switched path | no diff under `src/sip`, `modules`, `line` |

## Jio line on the Pi — T044 / T037

Pi deployed with the arm64 image `amrpt-7b0dd7f` (built with `arm-build-200`, loaded with `docker save | docker load`), in `[sip_server]` test mode with `amr_passthrough = true`; the original config and compose tag were backed up (`*.bak-pre-amrpt`) and the original config restored afterwards. The new image stays deployed with the setting at its default (off).

| Check | Result |
|---|---|
| Outbound over Jio (baresip → Pi → Jio → Vodafone), setting on | log `media_path=transcoded:outbound`; Agent A verdict `both-ways` (`carrier_rx=1601`, `pbx_rx=1499`) |
| Inbound over Jio from a mobile (two calls, the second ~5 min) | `path=transcoded reason=mode_set` both times, AMR-WB client; verdict `both-ways` (`carrier_rx=2852`, `pbx_rx=14879` on the long call). Jio's offer is always `a=fmtp:110 mode-set=0,1,2,3;octet-align=1;…` and the client answers unrestricted, so FR-003(b) transcodes. **Not forwarded; owner decided to keep the rule** |
| Inbound over Jio from the Vodafone rig | arrives as PCMU: `transcoded reason=carrier_not_amr` (3 of 3) |
| Soak, DTMF and hold/resume on a *forwarded* Jio call | not possible while the rule stands; covered on the Vodafone rig above |

## Cost, T045 (x86 local rig only; Pi not measured)

`harness/cpu.sh`, 60 s steady call, CPU seconds of Agent A + Agent B:

| Path | Agent A | Agent B | Total |
|---|---|---|---|
| Transcoded (setting off) | 1.44 s (2.4 %) | 1.77 s (3.0 %) | 3.21 s (5.3 % of a core) |
| Forwarded | 0.43 s (0.7 %) | 0.74 s (1.2 %) | 1.17 s (1.9 % of a core) |

A 63.5 % reduction overall (70 % in Agent A, 58 % in Agent B, whose conference-bridge clock
and SIP work remain). SC-004 asks for at least 70 % **on the Pi**: not measured (no forwarded
call was possible on Jio). SC-005 (added delay) was not measured either.
