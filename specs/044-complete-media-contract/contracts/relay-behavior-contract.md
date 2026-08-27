# Contract: relay/answer behavior by condition

Each row is independently verifiable by observing the wire (packets on
the relay's two sockets) or the SDP answer, with no knowledge of internal
implementation required.

| Condition | Behavior (after this feature) | Behavior (before — for contrast) |
|---|---|---|
| Pass-through call, legs negotiated *different* DTMF payload types, a keypress arrives | Forwarded with the payload-type byte rewritten to the **receiving** leg's own negotiated DTMF PT | Forwarded with the **sending** leg's PT unchanged — may not match what the receiver expects (RTP-03 bug) |
| Pass-through call, legs negotiated the *same* DTMF payload type | Forwarded unchanged (no rewrite needed) | Same (unchanged) |
| Pass-through or transcoding call, an ordinary audio packet | Forwarded/transcoded exactly as today | Same (unchanged) |
| Any relayed stream's SSRC changes mid-call | Logged, identifying the direction; packet still forwarded | Nothing logged; packet still forwarded (unchanged forwarding, new visibility) |
| A stream's very first packet | Not logged as a change (nothing to compare against) | N/A (no logging existed) |
| Offer states a `ptime` value (any value, including this bridge's own default) | Answer states this bridge's own true, fixed value, unaffected by the offer's | Same — confirmed correct, not a bug (SDP-06's `ptime` half; see research.md Decision 4) |
| Offer states no `ptime` | Answer states the existing fixed value | Same (unchanged) |
| RTCP (any condition) | Unchanged — no RTCP sent or received, `b=RS:`/`b=RR:` still declared | Same — explicitly deferred, not fixed by this feature |

Rows describing "before" behavior are drawn from
`docs/plans/mt-conformance-findings.md` (RTP-03, RTP-04, SDP-06) and the
verified current source (`src/ims/agent/veth.rs`, `src/ims/transcode.rs`,
`src/ims/sdp.rs`) — see `research.md` for exact call sites.
