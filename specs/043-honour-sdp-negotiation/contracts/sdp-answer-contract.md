# Contract: SDP answers by offer condition

This bridge's external interface is the SDP body it sends back in a `200
OK` (or the decline response) to an inbound `INVITE`. Each row is
independently verifiable by sending the described offer and inspecting the
response, with no knowledge of internal implementation required.

| Offer condition | Response (after this feature) | Response (before — for contrast) |
|---|---|---|
| One audio section, supported transport, two-way (or no) direction, a usable codec | `200 OK`, one `m=audio` line, `a=sendrecv` | Unchanged — identical to today |
| One audio section plus one or more other `m=` sections (extra audio, video, text, anything) | `200 OK`, the negotiated `m=audio` line plus one `m=<kind> 0 <proto> <fmts>` line per extra section, in original relative order | The extra section(s) simply absent from the answer — not accepted, not declined (SDP-01 bug) |
| Two `m=audio` sections | `200 OK` negotiates the **first**; the second appears as a declined `m=audio 0 ...` line | The **second** silently overwrote the first's port/codec list with no trace (SDP-01 bug) |
| Audio section marked `a=sendonly` | `200 OK` states `a=recvonly` | `200 OK` states `a=sendrecv` regardless (SDP-02 bug) |
| Audio section marked `a=recvonly` | `200 OK` states `a=sendonly` | `200 OK` states `a=sendrecv` regardless (SDP-02 bug) |
| Audio section marked `a=inactive` | `200 OK` states `a=inactive` | `200 OK` states `a=sendrecv` regardless (SDP-02 bug) |
| Audio section's `m=` line names an unsupported transport (e.g. `RTP/SAVP`) | `488 Not Acceptable Here`, `Warning: 305 ... "incompatible network protocol used"`; no answer produced | Transport token never read; offer processed as if it said `RTP/AVP` (SDP-03 bug) |
| Audio section names a codec list this bridge can't answer with (unchanged from MT-07) | `488 Not Acceptable Here`, `Warning: 304 ... "media type not available"` | Same (unchanged — this row exists only to show it's distinguishable from the new transport decline) |
| Inbound INVITE carries `Session-Expires` (MT-05) | `200 OK` never echoes `Session-Expires` or claims `Supported: timer` — unchanged from today, now pinned by a test | Same (already correct post-MT-10; not a behavior change) |

Rows describing "before" behavior are drawn from `docs/plans/mt-conformance-findings.md`
(SDP-01, SDP-02, SDP-03) and the verified current source (`src/ims/sdp.rs`,
`src/ims/agent/inbound.rs`) — see `research.md` for exact line references.
