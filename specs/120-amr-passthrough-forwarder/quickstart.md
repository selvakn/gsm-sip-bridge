# Quickstart: verifying AMR passthrough

## 0. Gate spikes (before any product code)

Reuse `specs/111-amr-passthrough-spike/harness` (`run.sh`, `b2.c`).
- **S1**: B places a call to Y with a normal transport, Y answers, B re-INVITEs with `PJSUA_CALL_REINIT_MEDIA` after registering a forward entry. Expect: new `c=`/`m=` of the forwarder, paused stream (sent 0 / received 0), AMR-WB frames identical both ways, hold/resume intact, an incompatible re-INVITE from Y answered 488, no leaked UDP port after hangup. Record in `specs/120-amr-passthrough-forwarder/spike-s1.md`.
- **S2**: unpair the veth-side call; confirm it stays quiet and A-stand-in tolerates it.

## 1. Lab (no carrier)

```bash
make test                                   # unit, loopback relay, control protocol
make test-passthrough                       # real PJSIP hook on loopback: upgrade by re-INVITE,
                                            # hold/resume, 488, downgrade, no leaked port
                                            # (needs libpjproject for the pjsip-linked build)
```

## 2. Local rig (Vodafone), inbound

1. `[vowifi] amr_passthrough = true`, `[sip_server]` mode, baresip registered with AMR-WB first.
2. Call the line from a second phone. Answer on baresip.
3. Agent B log: `media path decided … path=forwarded`. Agent A: `forwarding=true`.
4. `tcpdump -i <veth>` and on the client side: carrier-side and client-side AMR frames bit-identical (script from the spike: `bytecmp.py`). Press digits both ways; hold/resume from baresip. 5-minute soak.
5. Dial out from baresip: `media_path=transcoded:outbound`, two-way audio.
6. Switch baresip to `audio_codecs=pcmu`: `transcoded:client_not_amr`, two-way audio.
7. `amr_passthrough = false`, recreate: `transcoded:disabled`.

## 3. Jio (remote Pi)

Same as 2 on the Jio line via the arm64 pipeline (`arm-build-200`) and the Pi; steps 3–6. Recreate the container when switching the setting.

## 4. Cost (SC-004/005)

Same call forwarded vs transcoded on the Pi: `pidstat -u -p <agent pids> 1` over 60 s; added delay from timestamped pcaps on both sides of Agent B.
