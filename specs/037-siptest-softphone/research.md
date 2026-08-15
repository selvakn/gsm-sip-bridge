# Phase 0 Research: siptest

Every finding below was verified by reading code or probing the live system on
2026-08-15, not inferred. Line references are to the tree at that date.

---

## R1. Which `ims` primitives are actually reachable from a sibling crate

**Decision**: Reuse `ims::{rtp, sip_client, digest, media_stats}`, which
requires widening three module declarations in `gsm-sip-bridge` and adding one
public parse helper.

**Finding**: `lib.rs:7` says `pub mod ims;`, but the submodules are not all
public (`ims/mod.rs`):

```
33: pub(crate) mod digest;
39: mod rtp;                  <- fully private
45: pub(crate) mod sip_client;
```

So `ims::rtp`, `ims::digest` and `ims::sip_client` are **unreachable from
`siptest` today** — `build_packet`, `WavWriter`, `SipRequest`, `ha1`, none of
it. Only `media_stats`, `sdp`, `echo`, `call`, `session`, `lifecycle`,
`transport`, `agent` and `observability` are `pub`.

Worse, the two parsers siptest most needs are private even within their
module: `SipResponse::try_parse` (`sip_client.rs:117`) has no visibility
modifier at all, and `SipRequest::try_parse` (`:230`) is `pub(crate)`. The only
public route to a parsed message is through `SipTransport`, which is exactly
the thing siptest cannot use (R2).

**Rationale**: Widening is precedent-following, not novel — each of these
modules already carries a doc comment explaining why it was widened from
private to `pub(crate)` when `sip::server` needed it. The structs and all their
fields are already `pub`; only the module gates and the two parse fns are
closed. Add `sip_client::parse_datagram(&str) -> BridgeResult<Option<SipMessage>>`
and refactor `recv_message_deadline` (`:901`) onto it, so exactly one
implementation performs the request-vs-response discrimination.

**Alternatives considered**:
- *A `pub mod ims::wire` facade re-exporting only the ~16 needed items* —
  avoids exposing IMS-specific builders like `build_register` that siptest must
  not use. Viable fallback if a reviewer objects to widening all of
  `sip_client`; rejected as the default because it is a new indirection layer
  where three tokens suffice.
- *Copy the primitives into siptest* — rejected; a second μ-law table, WAV
  writer and jitter calculation is exactly the duplication Principle V targets.
- *Extract a shared `sip-core` crate* — the right end state, rejected as
  premature. Tracked in the plan's Complexity Tracking.

---

## R2. Socket model: `SipTransport` cannot be used

**Decision**: siptest owns **one unconnected `UdpSocket`** bound to
`0.0.0.0:<local_port>`, using `send_to`/`recv_from` exclusively.

**Finding**: Every `SipTransport` constructor ends in `socket.connect(dst)`
(`sip_client.rs:735` and `:758`) and then reads with `socket.recv(...)`
(`:809`). A *connected* UDP socket carries a kernel-level source filter:
datagrams from any address other than `dst` are discarded before userspace ever
sees them.

Three independently fatal consequences:

1. **Inbound calls would vanish.** The bridge rings from the telephony agent on
   `:5072`, not from the registrar on `:5060`. A socket connected to the
   registrar cannot receive them — silently. No log, no error; the bridge
   retransmits for 32 s and gives up.
2. **The redirect needs two peers in one dialog.** The INVITE and the redirect's
   ACK go to `:5060`; everything after goes to `:5072`.
3. **The source address is load-bearing.** `bindings.rs:113` matches
   `b.source == addr` — the **full `SocketAddr`, IP and port**. Confirmed by
   reading it. So an outbound INVITE must leave from the byte-identical local
   endpoint that sent the REGISTER, or the bridge answers `403`. A fresh socket
   per transaction is therefore forbidden, and the failure looks like an
   authentication problem, which is a nasty debugging trap.

A fourth reason from the other direction: the bridge dials the registered
`Contact` **verbatim**, so that URI's host:port must name the same socket.

**Alternatives considered**: *A socket per peer* — breaks (3). *`SipTransport`
plus a second listening socket* — breaks (3) and splits state for no gain.

---

## R3. The outbound redirect contract

**Decision**: Always take the redirect target from the `302`'s own `Contact`
header; never from configuration.

**Finding**: `sip/server/mod.rs:277-308` — the registrar verifies the INVITE's
source is a live binding, then answers `302 Moved Temporarily` with
`Contact: sip:{destination}@{host}:{outbound_local_port}`. That port is
**whichever subsystem is hosting the pjsua endpoint**:

| Deployment | Port | Source |
|---|---|---|
| VoWiFi telephony agent (the live config) | 5072 | `vowifi/mod.rs:68` |
| Circuit-switched daemon | 5062 | `[sip].local_port` |
| VoLTE | 5073 | `volte/bridge.rs:69` |

Live check on 2026-08-15 confirmed `[vowifi] enabled = true`, and `ss` showed
`0.0.0.0:5072` listening — so this deployment redirects to 5072. Hardcoding it
would break silently under a different configuration. Configuration carries an
*expected* port used only to log a warning on mismatch, never to gate.

Destinations are validated as `[0-9*#+]+` (`sip/outbound.rs:56-67`). Refusals
to map distinctly: `403` untrusted source (i.e. our registration lapsed), `484`
invalid destination, `503` no idle line, `400` no user part.

---

## R4. Provisioning: siptest needs its own account

**Decision**: siptest registers under a dedicated account, and refuses to start
under the bridge's current ring target unless explicitly overridden.

**Finding**: the binding table is `HashMap<String, Binding>` keyed by AOR with
`upsert` replacing (`bindings.rs:53, 70`) — **one binding per account, no
forking**. The live system on 2026-08-15 reported
`gsm_sip_bridge_sip_server_bindings 1` and `ring_aor_registered 1`: a physical
handset is registered as `1001` right now. siptest registering as `1001` would
silently evict it and steal its calls.

Consequences, both of which belong in the quickstart rather than being
discovered:
- **Outbound needs no bridge change** — any live binding may dial out, so
  `ring_aor` is irrelevant to it.
- **Inbound requires `ring_aor` to name siptest's account**, because exactly
  one account rings. That is a bridge config change plus a restart, and it
  takes the handset out of service for the duration.

---

## R5. Codecs available on the local leg

**Decision**: PCMU first; G.722 in a later slice. Offer `PCMU`, `G722` and
`telephone-event`.

**Finding**: the bridge's pjproject build has **no AMR and no Opus**
(`config_auto.h` shows all three AMR/Opus flags at 0); AMR is handled entirely
by the separate `amr-safe` crate on the carrier leg, never through pjsip. The
telephony agent prioritises `G722/16000/1` at 200 and runs its conference
bridge at 16 kHz (`vowifi/mod.rs:1412-1450`); PCMU is always present.

`ims::sdp` cannot be reused for this: `build_offer` emits only
PCMU/AMR-NB/AMR-WB/L16, and `parse_answer` (`sdp.rs:221-231`) hard-rejects
every payload type except 0 and 96 — G.722's PT 9 errors out. siptest needs its
own small SDP module.

**The G.722 trap, recorded because it is silent**: `a=rtpmap` says `G722/8000`
(RFC 3551's historical error), the audio is **16 kHz**, and the RTP timestamp
advances at **8 kHz** — 160 per 20 ms frame, while the frame holds 320 samples.
Three consumers need different values: jitter maths wants the RTP clock, the
WAV writer wants the audio rate, the tone detector wants the audio rate. Encode
all of it in one `CodecProfile` struct with a field-by-field regression test.

---

## R6. Audio verification: what packet counts cannot tell you

**Decision**: three orthogonal verdicts, reported side by side, never collapsed
— packet-count direction verdict, signal detection, and received energy level.

**Finding**: the repo has **no RMS, energy or level-measurement code at all**;
direction attribution today is purely packet counting. That is deliberate:
`media_stats.rs:82-84` argues a quiet party still produces frames, so packet
counts separate "nothing is reaching us" from "they said nothing" in a way
loudness cannot. And `:89-90` records a past one-way-audio incident, warning
that `SendOnly` and `ReceiveOnly` "must never be collapsed into a single 'no
audio' state".

So tone detection **complements** rather than replaces the existing verdict.
Reuse `media_stats::{verdict, DirectionVerdict, ReceiveTracker}` unchanged; add
level measurement as new, separately-reported information.

One correction to existing practice: `ims/call.rs:635` passes *samples* to
`verdict()`. siptest passes **packets**, which is what `ReceiveTracker` counts
and what the ratio means. Same semantics, less conversion.

**Detection design**: Goertzel over 20 ms windows aligned to received packets
(160 samples at 8 kHz, 320 at 16 kHz), with four gates that must all pass —
relative in-band energy, twist between the two tones, an absolute floor above a
running noise-floor percentile, and a broadband guard that rejects speech and
music. This is the standard DTMF-detector recipe, and it is what makes "is this
a tone or is this noise" actually answerable.

**Why a two-tone symbol grid rather than one sine**: a single sine proves
something arrived but not *when it was sent*, so it cannot yield round-trip
delay. Eight non-harmonic frequencies in two groups give 16 symbols; a
recovered symbol index identifies its transmit time, which is what makes RTT
measurable. Deliberately **not** DTMF frequencies: a carrier or PBX may detect
real DTMF and regenerate it out-of-band, destroying the in-band signal being
measured.

---

## R7. G.722 implementation — deliberately deferred

**Decision**: build the whole pipeline on PCMU first; choose the G.722
implementation when that slice starts.

**Finding**: the crates.io survey came out poorly.

| Crate | License | Downloads | Problem |
|---|---|---|---|
| `ezk-g722` 0.1.2 | MIT | 38k | Mandatorily pulls `ezk` + `ezk-audio` + `ezk-rtp` — a whole SIP framework for one codec (verified: all four deps non-optional) |
| `audio-codec` 0.4.2 | MIT | 9.9k | Pulls C FFI: `g729-sys`, `opus-rs` |
| `oxideav-g722` 0.0.7 | MIT | 540 | Adoption too low to trust for a codec whose correctness silently corrupts the measurement |

The likely answer is ~400 lines of in-crate sub-band ADPCM validated against
the ITU-T G.191 reference vectors — consistent with this repo already
hand-rolling μ-law and its own WAV writer rather than taking `hound`. Deferring
means the tone pipeline is already proven on PCMU when it lands, so a codec bug
is trivially isolated. All licences listed are on the `deny.toml` allow-list,
so either route passes policy.

---

## R8. Concurrency: threads for SIP and media, tokio only for axum

**Decision**: `std::thread` + `crossbeam-channel` throughout; exactly one tokio
runtime, owned by axum, built explicitly with
`Builder::new_multi_thread().enable_all()` + `rt.block_on(...)`.

**Rationale**: house style is threads with tokio only where needed
(`runtime.rs:8-13`), and the only thing that needs it is axum 0.8. A 20 ms
cadence with a single-digit-millisecond jitter budget is precisely the workload
where a shared work-stealing runtime under load is a liability — the codebase
already made this call for `ims::call`. And the dialog engine's simplicity comes
from being one blocking `select!` over three sources; as async it gains
`Send + 'static` obligations on every state machine for no benefit.

Five thread roles: a socket reader that only demultiplexes; a single-threaded
dialog engine owning all SIP state (so no locks on the hot path and
deterministic event ordering, the same shape as the registrar's own `serve`
loop); a per-call media transmitter; a per-call media receiver; and the axum
thread.

**One improvement over existing practice**: `ims/call.rs:609` sleeps a fixed
20 ms *after* doing the work, so it drifts by the per-packet work time — at
50 pps over a 30 s call that is cumulative and would corrupt the RTT
measurement. siptest schedules against absolute deadlines
(`start + n * ptime`).

**Direction independence, made structural**: `ims/echo.rs:12-25` warns that a
receive-dependent marker makes the two directions dependent and destroys
attribution. siptest takes the stronger form — there is **no channel from the
media receiver to the media transmitter at all**, so total receive failure
cannot alter what is sent. Asserted by a test named for the invariant.

---

## R9. Agent-facing interface: cursor long-poll, not SSE

**Decision**: ship `GET /events?since=N` long-poll only. No SSE.

**Rationale**: SSE never terminates, which is wrong for an agent driving
`curl`; it requires a broadcast channel, which is the only thing that would
force tokio into the event path; and the cursor form is strictly more useful —
replayable, gap-detectable, and resumable after a crash. The agent's loop is
literally `n=$(curl -s "$B/events?since=$n" | jq ...)`. Add SSE later only if a
human wants a live tail.

Also included: `GET /log/tail`, so an agent can diagnose without locating the
daemon's stderr — in practice the difference between diagnosing a `403` and
giving up.

**On delay reporting**: one-way audio delay is *not* honestly measurable
without clock synchronisation with the far end. Report round-trip delay when
the signal returns, and otherwise report the three delays that genuinely are
one-way and genuinely measurable: invite→180, invite→200, and answer→first RTP.
That last one is the one that actually catches broken media paths. Do not
report `rtt/2` dressed up as a one-way figure.

---

## R10. Protocol hazards to avoid

**Decision**: do not advertise `Supported: 100rel, timer`; answer any unknown
`Require:` with `420 Bad Extension`.

**Rationale**: `ims::agent::inbound.rs:50` advertises `100rel` for the carrier
leg. siptest must not: a pjsua that sees it may send `Require: 100rel` on a
183, and the call dies when siptest never PRACKs. Likewise, without
`Supported: timer` the session timer stays off.

Two more, both silent until they bite:
- **Retransmit the final response until acknowledged.** On UDP this works right
  up until the first lost ACK, then the call establishes on one side only. T1
  ladder, 500 ms doubling, abandon at 64×T1.
- **The advertised `Contact` must carry the routable LAN address.**
  `sip:1001@0.0.0.0:5065` makes inbound silently never arrive. Discover it with
  the connect-trick, log it at INFO on every REGISTER, and surface it in
  `/status` — it is the most likely first-run failure and otherwise invisible.

---

## R11. Test strategy without hardware

**Decision**: run the bridge's **real** registrar in-process; stand in only for
pjsua.

**Finding**: `Registrar::start_on` and `start_on_with_outbound`
(`sip/server/mod.rs:90, 96`) are both already `pub`, and the registrar is pure
safe Rust on its own `UdpSocket` — deliberately not a PJSIP module, precisely
so it is testable without the `pjsip-linked` feature.
`gsm-sip-bridge/tests/test_sip_server_registrar.rs` is a 750-line harness
already doing this, binding `127.0.0.1:0` and asserting on raw datagrams.

So the headline integration test drives siptest's production FSMs against the
real registrar, receives a real `302`, and re-INVITEs a loopback stub that
echoes RTP back **with a deliberate fixed delay** — giving ground truth to
assert the measured RTT against. The stub stands in for pjsua, which CI never
compiles; that is the constitution's sanctioned carve-out and gets the required
in-file justification.

**Constraint**: `.config/nextest.toml` sets a 20 s slow-timeout with
`terminate-after = 1`. Call duration and registration expiry must therefore be
parameters, never constants, so test calls run 1–2 s.

---

## R12. Build integration

**Decision**: workspace member only; no new build entry point.

**Findings**:
- Adding `"siptest"` to `members` is the sole build change — `make build`,
  `test`, `lint` and `coverage` all already pass `--workspace`.
- **`tools/count-unsafe.sh` must be extended.** It hardcodes
  `gsm-sip-bridge/src/` and `pjsua-safe/src/`, so a new crate is simply never
  checked. While there, tighten the match: it currently greps the bare word
  `unsafe` filtered by `grep -v "//.*unsafe"` (verified at `tools/count-unsafe.sh:7`),
  so a doc comment or a string containing "unsafe" fails the crate.
- Two Makefile targets, each with the constitution-required `## ` description.
