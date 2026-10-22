# Gate spikes S1 and S2 — result

**Decision: GO.** The upgrade-on-answer design (research R1–R3) works on stock pjproject 2.16 with three refinements recorded below.

Harness: `specs/111-amr-passthrough-spike/harness/b3.c` + `run3.sh` + `cmp2.py` (extends `b2.c`; production call settings `aud_cnt=1, vid_cnt=0, txt_cnt=0`). Raw output: `specs/111-amr-passthrough-spike/results/s1-s2-reinvite-upgrade.txt`. Lab: X (veth stand-in) → B → Y (client), all AMR-WB, stock `pjsua` peers.

## S1 — upgrading a live call by re-INVITE

| Check | Result |
|---|---|
| Calls start on the normal transport (`on_create_media_transport` returns `base_tp` while no forward entry exists) | ✓ both legs bring up a normal call, conference-paired, two-way audio |
| After Y answers, registering an entry and `pjsua_call_reinvite2(PJSUA_CALL_REINIT_MEDIA)` makes the hook run again and return the loop transport | ✓ `loop_create2 -> 0`, `closing PJSIP's own transport` |
| Re-INVITE SDP carries the forwarder's `c=`/`m=` | ✓ `c=<B> m=audio <forwarder port>` in the local SDP (offer on the UAC leg, answer to X's later re-INVITEs) |
| PJSIP's old socket released | ✓ socket fds in B: 1 → 5 after upgrade (SIP + 2 forwarder pairs), i.e. the 4 PJSIP media sockets were closed rather than added to; 1 again after hangup (no leak) |
| Stream created paused | ✓ `on_stream_created2` fired for the re-created stream; PJSIP counters read **sent 0 / received 0** on the client leg after upgrade |
| Bit-identical frames both ways after upgrade | ✓ 550/550 (Y→X) and 535/535 (X→Y): seq, ts, SSRC and payload identical; PT relabelled 101↔96 |
| DTMF | ✓ RFC 4733 `1,2,3` seen by Y |
| Hold/resume (re-INVITEs from X) | ✓ both answered 200, stream re-created and re-paused each time, remote address/PT re-learned in `on_call_media_state` |
| Incompatible re-INVITE from the client | ✓ `on_call_rx_reinvite` with `*code = 488` — Y logged `SIP/2.0 488 Not Acceptable Here`; forwarded media kept flowing (forwarder counters unchanged by the rejected offer) |
| Downgrade (clear entry, re-INVITE `REINIT_MEDIA`) | ✓ base transport restored (new ports 4010/4012), conference re-paired, packets flow both ways again |
| No leaked UDP port after hangup | ✓ see fd row |

## S2 — the veth-side call

The unpaired veth call stays up for the whole call and X is undisturbed. **But it does not stay silent by itself:** after the client leg is upgraded the veth call's unconnected conference port still ticks, and PJSIP kept sending silent AMR frames to X for the full call (463 packets). Pausing its stream stops that completely (14 packets, all before the upgrade; 0 after).

## Refinements that go into the implementation

1. **Run the upgrade outside PJSIP's callback.** `pjsua_call_reinvite2` called from inside `on_call_state(CONFIRMED)` returned `PJ_EINVALIDOP (70013)` — and the hook had already swapped in the loop transport, leaving a half-upgraded call. Production issues it from Agent B's own thread (the bridge poll loop) after the call state settles, and **clears the forward entry on any failure** of `reinvite2`.
2. **The veth call must be paused explicitly.** There is no public `pjsua_call_get_stream`, so `pjsua-safe` records each call's `pjmedia_stream*` in `on_stream_created2` and drops it in `on_stream_destroyed` (under the registry mutex, so a pause never races a destroy). The registry therefore holds two kinds of entry: `Forward(addr)` for the client leg and `Quiet` for the veth leg; `on_stream_created2` pauses both (and re-pauses after any re-created stream, e.g. a session refresh). The veth call is also disconnected from every conference slot on upgrade.
3. **A downgrade is a second `REINIT_MEDIA` re-INVITE with the entry cleared** — verified; only reachable on failure paths.

## Not covered here

Real Jio / Vodafone behaviour of the *client's* answer to the re-INVITE (some SIP stacks refuse a re-INVITE that only changes `c=`/`m=`); that is the live-verification tasks (T028, T044). An answer-timeout or refusal falls back as research R3 specifies.
