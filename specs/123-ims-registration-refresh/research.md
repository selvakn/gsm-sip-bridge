# Research: IMS Registration Refresh Without Call Interruption

Resolves the three Open Questions in the spec and records the design decisions behind [plan.md](plan.md).

**Source quality note.** The clause text below was read from the iTecSpec mirror of 3GPP TS 24.229 (clauses 5.1.1.4.1, 5.1.1.4.2, 5.2.2.2). The ETSI PDF could not be text-extracted in this environment. The mirror may carry an older release than the Rel-18 PDF cited in issue #163, so each decision below is also backed by the live-network evidence in the issue where it exists. Re-check the three decisions against the Rel-18 PDF during review of Slice B; none is expected to change.

## R1. Must the protected re-REGISTER carry `integrity-protected="yes"`?

- **Decision**: No. The UE does not send it.
- **Rationale**: TS 24.229 §5.1.1.4.2 a) lists the UE's re-registration Authorization header as `username`, `realm`, `uri`, `nonce` (last received) and `response` (last calculated) and does not mention `integrity-protected`. §5.2.2.2 item 1) makes the **P-CSCF** insert `integrity-protected` into the Authorization header: "yes" when the request arrived protected by the ongoing or last successful authentication's SA, "no" otherwise. The S-CSCF therefore sees "yes" for a refresh arriving over the SA, which is why it can accept it without a new challenge (TS 33.203 §7.4). This matches the live trace in the issue: 14 refreshes accepted with no UE-side parameter.
- **Alternatives considered**: sending `integrity-protected="yes"` from the UE: harmless on a compliant P-CSCF (it overwrites it) but unnecessary, and it would put a field in the header that the standard assigns to the network.

## R2. Is replaying the cached `nc`/`cnonce` acceptable?

- **Decision**: Replay the cached Authorization header verbatim, including whatever `qop`, `nc` and `cnonce` it was computed with. Do not advance `nc`.
- **Rationale**: §5.1.1.4.2 a) requires the *last calculated response* to be sent. A digest response is bound to its `nc` and `cnonce`, so changing either would invalidate the response, and recomputing would need the AKA result (RES), which is deliberately not kept. For IMS-AKA the S-CSCF accepts a protected re-REGISTER on the strength of the SA, not on a fresh digest. Live evidence: 14/14 accepted on Jio.
- **Alternatives considered**: advancing `nc` and recomputing: requires retaining RES and gains nothing for a P-CSCF-vouched request; rejected as extra secret retention (Principle V).
- **Risk**: a stricter registrar could treat a repeated `nc=00000001` as a replay. Mitigation is FR-018a: two consecutive rejections move the next cycle to a full re-registration.

## R3. Which statuses force a full re-registration?

- **Decision** (TS 24.229 §5.1.1.4.1, re-registration):

  | Answer to the refresh | Action |
  |---|---|
  | 408, 500, 504, 403 | Full re-registration (initial registration procedure). Deferred until idle if a call is active. |
  | 503 with Retry-After **longer than Timer F** (128 s) | Mark this P-CSCF unavailable for the Retry-After; fail over to another P-CSCF with an initial registration. |
  | 503 with Retry-After **shorter than Timer F** | Retry the refresh on the same P-CSCF after the Retry-After (an initial registration if the registration expired meanwhile). |
  | 423 Interval Too Brief | One retry with an expiry at least the `Min-Expires` value. |
  | 401 | Network-requested AKA (Slice C). Until Slice C ships, treated as "needs full re-registration", deferred during a call. |
  | transaction timeout (Timer F) | Failure on backoff; not a "rejection" for FR-018a. |
  | other final errors | Count as a rejection; on backoff. |

- **Rationale**: this is what the clause says; it also matches the set the reporter chose for 403/408/500/504, so that part of PR #168 is sound. The part that was missing there is the 503 split, which is what Greptile finding 3 is about. One correction to the earlier triage: a 503 whose Retry-After is *shorter* than Timer F is correctly retried on the same P-CSCF; only the longer case fails over.
- **Alternatives considered**: treating every non-2xx as "go to full re-registration": simpler, but it loses the 503/423 behaviour and reintroduces call disruption on benign answers.

## R4. SA lifetime rule in §5.1.1.4.2

- **Decision**: no kernel change in Slice B. §5.1.1.4.2 says that on a 200 OK to a re-registration the UE sets the SA lifetime to the longer of the existing lifetime and the new registration lifetime plus 30 s. The XFRM states installed by `gm_ipsec.rs` carry no lifetime limit, so there is nothing to extend. Slice C, which creates new SAs, must not introduce a limit shorter than the registration lifetime plus 30 s.

## R5. Why a dedicated REGISTER CSeq

- **Decision**: add a registration-only CSeq counter. On `main`, `RegisteredSession.cseq` is a single counter consumed by OPTIONS probes, SUBSCRIBE and call requests, while all of them share the registration Call-ID. RFC 3261 §10.2 requires a REGISTER with a reused Call-ID to have a CSeq one higher than the previous REGISTER; a shared counter would skip numbers and let an OPTIONS answer and a REGISTER answer collide.
- **Alternatives considered**: a separate Call-ID for REGISTER (loses the continuity the issue asks for); matching on CSeq number alone (the exact collision the PR had to patch).

## R6. Where responses are held while a call rings

- **Decision**: treat as an audit task (first task of the US4 phase in tasks.md). Greptile's finding says the inbound-INVITE path holds REGISTER replies until the PBX answers and replays them after the deadline check. The behaviour to implement is independent of the mechanism: the pending refresh's deadline is only declared expired if no answer arrived before it, and a held answer carries the time it arrived.
- **Alternative considered**: move registration handling to its own reader thread. Rejected: contradicts the single-reader design the issue and PR both preserve, and is more moving parts (Principle V).

## R7. Interop and rollout

- **Decision**: no config switch (Clarification Q1); automatic fallback after 2 consecutive rejections (FR-018a); live evidence on Jio and Vodafone VoWiFi before Slice B is called done; VoLTE live run is a follow-up (Clarification Q4).
- **Known carrier quirks to keep in mind**: Vodafone's reginfo carries two AORs and an IMEI echo; Vodafone host-side VoLTE REGISTER can fail pre-auth with 403-6037 independent of this change; Jio inbound 503 "SDP Protocol Error" is boilerplate. None of these should be mistaken for a refresh failure during live verification.

## R8. Credit and reuse of PR #168

- Reuse candidates (clean, tested in the PR): the schedule helper, the `Maintenance` split and its tests, Via-branch response matching, Contact-lifetime parsing (`granted_lifetime`, `contact_expiry`), the UDP retransmit/T1/T2/Timer F logic, and the test fixtures. Re-create or cherry-pick with `Co-authored-by: Kirill Mironov <kmironov.eu@gmail.com>` (the reporter's commit email), keeping their authorship where a hunk is taken unchanged.
- Rewrite: failure handling (R3), ring-wait ordering (R6), and, in Slice C, UDP reply reading.
- **Outcome (T002).** What was actually taken from `3e914c3`: the Contact-lifetime parsing (`granted_lifetime`, `contact_expiry`, `delta_seconds`) in `ims/agent/registration_refresh.rs`, adapted rather than copied, and credited in that file's module header. The schedule helper, the `Maintenance` split, Via-branch matching and the UDP timers were re-written against this codebase's own structure, as were the ring-wait handling, the failure routing and everything in Slice C (the reporter's worker owned sockets and kernel state; this design keeps the SIM alone on the thread). No hunk was applied unchanged, so no commit carries a `Co-authored-by` trailer; the reporter is credited in the release notes of both slices and in the module header.
