# Contract: SIP responses by request/identity-match combination

This bridge's external interface is the SIP responses it sends to the
carrier/PBX on the wire. This table is the contract this feature changes —
each row is independently verifiable by sending the described request and
observing the response, with no knowledge of internal implementation
required. "Active call" = the one call, if any, `LoopState` currently holds.

| Inbound request | Condition | Response (after this feature) | Response (before — for contrast) |
|---|---|---|---|
| `BYE` | Names the active call | `200 OK`, call ends, reported caller-ended | `200 OK`, call ends (unchanged) |
| `BYE` | Names a different/unknown call | `481 Call/Transaction Does Not Exist`; active call untouched | Active call ended regardless (MT-08 bug) |
| `BYE` | No call is active | `481 Call/Transaction Does Not Exist` | `200 OK` (falsely implied a dialog existed) |
| `INVITE` | Names the active call; CSeq matches the INVITE already answered | `200 OK` with the same `Contact`/SDP already given | Refused `486 Busy Here` (MT-02 bug) |
| `INVITE` | Names the active call; CSeq matches the INVITE already rung on (not yet answered) | `180 Ringing`, same as already given; ringing not restarted | Silently dropped |
| `INVITE` | Names the active call; CSeq is new (genuine re-INVITE) | `488 Not Acceptable Here` with `Warning: 304` | Refused `486 Busy Here` (MT-02 bug) |
| `INVITE` | Names an unrelated call (line occupied) | `486 Busy Here` (unchanged) | `486 Busy Here` |
| `CANCEL` | Names the active call (already given a final response) | `200 OK` on that call's own `To` tag | `481` (incorrect per RFC 3261 §9.2) |
| `CANCEL` | Names a call the bridge has no record of | `481 Call/Transaction Does Not Exist` (unchanged) | `481` |
| `ACK` | Names the active call | Confirms the dialog (unchanged, no response is ever sent for ACK) | Confirms unconditionally |
| `ACK` | Names a different/unknown call | Not treated as confirming the active call (diagnostic log only — still no response, per RFC 3261) | Confirmed unconditionally, indistinguishably |

Rows describing "before" behavior are drawn from `docs/plans/mt-conformance-findings.md`
(MT-01, MT-02, MT-08) and the verified current source
(`src/ims/agent/mod.rs`, `src/ims/agent/inbound.rs`) — see `research.md` for
the exact call sites.
