# Contract: response to an inbound INVITE by precondition shape

This bridge's external interface is the SIP/SDP response it sends to an
inbound carrier `INVITE`. Each row is independently verifiable by sending
the described `Require`/offer and inspecting the response, with no
knowledge of internal implementation required. "Remote"/"local" below are
offer-relative (as the offerer itself would write them) — see
`research.md` Decision 1 for why that inverts in this bridge's own answer.

| Offer condition | Response (after this feature) | Response (before — for contrast) |
|---|---|---|
| No `Require: precondition`, no `a=des:qos` lines at all | `200 OK`, unchanged | Unchanged — identical to today |
| `Require: precondition`, offer has no `a=des:qos` lines | `200 OK`, unchanged — treated as no precondition requested (FR-007) | `420 Bad Extension` (blanket gate fired on the header alone) |
| `Require: precondition`, `a=des:qos mandatory remote sendrecv` | `200 OK`, SDP includes `a=curr:qos local sendrecv` + `a=conf:qos local sendrecv` | `420 Bad Extension` |
| `Require: precondition`, `a=des:qos optional remote sendrecv` | `200 OK`, SDP includes `a=curr:qos local sendrecv` + `a=conf:qos local sendrecv` | `420 Bad Extension` |
| `Require: precondition`, `a=des:qos mandatory e2e sendrecv` | `580 Precondition Failure`; no answer body | `420 Bad Extension` |
| `Require: precondition`, `a=des:qos optional e2e sendrecv` | `200 OK`, SDP includes `a=curr:qos e2e <local-only status>` — never claims the far side's contribution | `420 Bad Extension` |
| `Require: precondition`, `a=des:qos mandatory local sendrecv` (offerer's own segment) plus a matching `a=curr:qos local none` | `200 OK`, SDP mirrors `a=curr:qos remote none` (inverted, not asserted — User Story 3); call proceeds | `420 Bad Extension` |
| `Require: precondition` + one `mandatory remote` line and one `mandatory e2e` line together | `580 Precondition Failure` — the unconfirmable `e2e` line governs the whole offer | `420 Bad Extension` |
| Precondition lines present, but the audio section itself is otherwise malformed (bad transport profile, no usable codec) | Existing decline for that reason fires first (`488`/Warning 305 or 304) — precondition handling never masks an existing decline | Same existing decline (this row exists only to show ordering is unchanged) |
| `Require:` names an extension other than `precondition` (e.g. `Require: 100rel, precondition`) | `420 Bad Extension`, `Unsupported: 100rel` — `precondition` no longer listed as unsupported, but the other tag still is | `420 Bad Extension`, `Unsupported: 100rel, precondition` |
| Offerless INVITE (SDP-04) with `Require: precondition` | Unaffected — no body means no `a=des:qos` lines, same as the "no lines" row above; SDP-04's existing offerless flow runs unchanged | `420 Bad Extension` (blanket gate fired before the empty-body branch was ever reached) |

Rows describing "before" behavior are drawn from
`docs/plans/mt-conformance-findings.md` (MT-06) and the verified current
source (`src/ims/agent/mod.rs`'s `unsupported_required_extensions`,
`src/ims/agent/inbound.rs`'s `handle_invite`) — see `research.md` for exact
decisions and RFC citations.
