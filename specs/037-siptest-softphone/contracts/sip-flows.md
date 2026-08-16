# Contract: SIP wire behaviour siptest must satisfy

Every sequence here was derived by reading the bridge's registrar and telephony
agent, not from a specification in the abstract. Addresses use the live
deployment: registrar `192.168.15.10:5060`, telephony agent `:5072`, siptest
`:5065`.

## C-0. Transport invariant (governs everything below)

siptest uses **one unconnected UDP socket** bound to `0.0.0.0:5065`, with
`send_to`/`recv_from`. Non-negotiable, for three separately fatal reasons:

1. The bridge rings from `:5072`, not the `:5060` siptest registered to. A
   connected socket is kernel-filtered to one peer and would discard those
   datagrams before userspace — silently.
2. One dialog spans two peers: INVITE and the redirect ACK go to `:5060`,
   everything after to `:5072`.
3. `bindings.rs:113` matches `b.source == addr` on the **full `SocketAddr`**.
   Every request must leave from the byte-identical endpoint that registered,
   or the bridge answers `403` — a refusal that looks like an auth fault.

`Via` and `Contact` must advertise the routable LAN address. `0.0.0.0` makes
inbound silently unreachable.

## C-1. Registration

```
siptest ──REGISTER sip:gsm-sip-bridge──────────────────────► :5060
siptest ◄─401 Unauthorized, WWW-Authenticate: Digest ...────
siptest ──REGISTER + Authorization: Digest ... ────────────► :5060
siptest ◄─200 OK, Expires: 300 ─────────────────────────────
```

```
REGISTER sip:gsm-sip-bridge SIP/2.0
Via: SIP/2.0/UDP 192.168.15.10:5065;branch=z9hG4bK{rand};rport
Max-Forwards: 70
From: <sip:1002@gsm-sip-bridge>;tag={from_tag}
To: <sip:1002@gsm-sip-bridge>
Call-ID: {stable for the registration lifetime}
CSeq: {n} REGISTER
Contact: <sip:1002@192.168.15.10:5065>
Expires: 300
Allow: INVITE, ACK, BYE, CANCEL, OPTIONS
User-Agent: siptest/{version}
Content-Length: 0
```

**Constraints from `sip/server/auth.rs`:**

- `MD5` and `qop=auth` only. `MD5-sess`, `SHA-256` and `auth-int` are refused.
- Nonce-count is replay-tracked **per account**, so `nc` must increase
  monotonically per nonce. Pre-authorise every refresh with the cached nonce
  and `nc+1` rather than provoking a fresh `401` each time.
- `401` with `stale=true` → adopt the new nonce, reset `nc` to 1.
- A second `401` on an already-authorised REGISTER is a hard failure, never a
  retry loop.
- `423 Interval Too Brief` → adopt `Min-Expires` and retry.

Refresh at `min(expires/2, expires - 30s)`, floor 30 s. `Expires: 0` on clean
shutdown. Backoff on failure 2/4/8/16/30 s, capped.

**Do not reuse `ims::sip_client::build_register`** — it hardcodes IMS ICSI
feature tags, `+g.3gpp.smsip`, an IMEI `+sip.instance` URN and a spoofed
Motorola `User-Agent` (`sip_client.rs:569-596`). A handset REGISTER is plain
RFC 3261.

## C-2. Outbound call — the redirect

```
siptest ──INVITE sip:+919000000000@192.168.15.10:5060 (+SDP)──► registrar
siptest ◄─302 Moved Temporarily
          Contact: <sip:+919000000000@192.168.15.10:5072> ─────
siptest ──ACK  (same branch, same CSeq number, To incl. tag)──► :5060
siptest ──INVITE sip:+919000000000@192.168.15.10:5072 (+SDP)──► agent
          (new branch, CSeq n+1, same Call-ID and From tag, same RTP ports)
siptest ◄─100 Trying ────────────────────────────────────────
siptest ◄─180 Ringing ───────────────────────────────────────
siptest ◄─200 OK (+SDP answer) ──────────────────────────────
siptest ──ACK (to the 200's Contact URI, new branch, same CSeq)─►
                                                    ─── media starts ───
```

Rules:

- **The redirect target comes from the `302`'s `Contact`, never from config.**
  That port is 5072 only because VoWiFi is enabled; it is 5062 for
  circuit-switched and 5073 for VoLTE. Config carries an *expected* port used
  solely to log a warning on mismatch.
- Follow at most **one** redirect, and only to the configured bridge host.
- The `302` must be ACKed. Non-2xx ACK is a hop-by-hop transaction ACK: same
  Call-ID, same CSeq *number* with method `ACK`, **the same `Via` branch as the
  INVITE**. Skipping it makes the registrar retransmit for 32 s.
- The 2xx ACK differs: new branch, sent to the URI in the 200's `Contact`.
- No `Proxy-Authorization` — the INVITE path authorises by source address, not
  by a second digest exchange.
- **Do not advertise `Supported: 100rel, timer`.** A pjsua that sees `100rel`
  may send `Require: 100rel` on a 183 and abandon the call when siptest never
  PRACKs. Answer any unknown `Require:` with `420 Bad Extension` + `Unsupported`.

Refusals, each mapped to a distinct named error (FR-009):

| Status | Meaning |
|---|---|
| `403` | Source not a live binding — our registration lapsed |
| `484` | Destination failed the `[0-9*#+]+` check |
| `503` | No idle line |
| `400` | No user part in the Request-URI |

Ring timeout → `CANCEL` reusing the INVITE's exact branch; expect `200` for the
CANCEL and `487` for the INVITE, then ACK the `487`. **`build_cancel` does not
exist anywhere in the repo** and must be written.

## C-3. Inbound call

```
agent   ──INVITE sip:1002@192.168.15.10:5065 (+SDP)──► siptest   [from :5072]
siptest ──100 Trying ────────────────────────────────►
siptest ──180 Ringing ───────────────────────────────►
                              ... answer_delay_ms ...
siptest ──200 OK (+SDP answer) ──────────────────────►  [retransmit on T1 ladder]
agent   ──ACK ───────────────────────────────────────►
                                          ─── media starts ───
```

Rules:

- **Accept regardless of source port.** The dialog is keyed on `Call-ID`, and
  responses go back to whatever `recv_from` reported (RFC 3261 §18.2.2).
  Validating against the registrar's address would make every inbound call
  invisible — the same trap that makes real handsets need "Accept SIP Trust
  Server Only" disabled (`docs/operations.md:702`).
- **Retransmit the 200 OK** at T1 backoff (500 ms doubling, cap 4 s, abandon at
  64×T1) until the ACK arrives. On UDP this is not optional; it works right up
  until the first lost ACK, then the call establishes on one side only.
- Capture `From`, `P-Asserted-Identity` and `X-GSM-Caller-ID` and report all
  three **separately** — an agent testing caller-ID propagation needs to see
  them disagree.
- `CANCEL` before answer → `200` for the CANCEL, `487` for the INVITE; record
  the outcome as `caller_cancelled`, which is **not** a fault.
- `OPTIONS` → `200` with `Allow`. Anything else → `405` with `Allow`.
- A second concurrent call → `486` plus a `busy` event.

## C-4. SDP

Offer (PCMU only until the G.722 slice; `c=` carries the routable LAN address):

```
v=0
o=- {session_id} {version} IN IP4 192.168.15.10
s=siptest
c=IN IP4 192.168.15.10
t=0 0
m=audio 40002 RTP/AVP 0 101
a=rtpmap:0 PCMU/8000
a=rtpmap:101 telephone-event/8000
a=fmtp:101 0-15
a=ptime:20
a=sendrecv
```

`ims::sdp` cannot be reused: `build_offer` emits only PCMU/AMR-NB/AMR-WB/L16,
and `parse_answer` (`sdp.rs:221-231`) rejects every payload type except 0 and
96 — G.722's PT 9 errors out.

Answer handling: take `c=` and the `m=audio` port for the RTP destination, and
the first payload type as the selected codec. Reject with a message naming the
payload type when the answer selects something unoffered. Treat `c=0.0.0.0`,
`m=audio 0` and `a=inactive` as explicit failures, not as silence.

**The G.722 rate trap** — `a=rtpmap` says `G722/8000`, the audio is 16 kHz, and
the RTP timestamp still advances at 8 kHz (160 per 20 ms frame, while the frame
holds 320 samples). Three consumers need different values: jitter maths takes
the RTP clock, the WAV writer and the tone detector take the audio rate.

## C-5. RTP

- 20 ms packets, scheduled against **absolute deadlines**
  (`start + n * ptime`), not `sleep(20ms)` after the work. The latter drifts by
  the per-packet work time — at 50 pps over 30 s that is cumulative and would
  corrupt the RTT measurement. (`ims/call.rs:609` has this bug; do not copy it.)
- Even local port from the configured range; bind port+1 for RTCP so nothing
  else claims it, but send no RTCP.
- Received packets → `ims::media_stats::ReceiveTracker::on_packet(seq, ts,
  arrival, rtp_clock_hz)` for loss, reordering and RFC 3550 jitter.
- **No channel from the media receiver to the media transmitter.** The transmit
  stream is generated purely from an absolute sample counter, so total receive
  failure cannot change what is sent — which is what keeps `SendOnly` and
  `Neither` genuinely distinguishable (`ims/echo.rs:12-25`). Asserted by a test
  named for the invariant: *the transmit sample stream is byte-identical
  whether or not any RTP arrives.*
