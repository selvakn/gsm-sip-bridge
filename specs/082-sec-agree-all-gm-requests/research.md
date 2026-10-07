# Research: Security-agreement headers on every Gm request

## R1. What is the exact current state?

`Security-Verify` and the option tags appear in `build_invite`
(`ims/call.rs`), `build_message` (`ims/sip_client.rs`) and, set by hand, in the
post-IPsec REGISTER (`ims/mod.rs`). BYE, UPDATE, PRACK, ACK, CANCEL,
SUBSCRIBE, OPTIONS and the un-REGISTER carry none. `security_verify` is a
standalone `Option<String>` on `RegisteredSession` next to
`gm_state: Option<(GmEndpoints, SaProposal, SecurityServerParams)>`.

## R2. Does the echo need to follow a mid-call renewal?

**Decision**: capture it in `DialogInfo` at call setup.
**Rationale**: `MaintenancePolicy` defers renewal while a call is active
(`lifecycle.rs`, tests `renewal_due_during_a_call_is_deferred_until_it_ends`
and `a_call_may_outlive_its_registration`), and a renewal replaces the whole
session. A call therefore never sees a second SA. `DialogInfo` already
snapshots `local_addr` and `use_tcp` from the session the same way.
**Alternatives**: passing the session into every in-dialog builder (more
plumbing, no behavioural difference); a shared `Arc<Mutex<..>>` echo (new
concurrency for no benefit).

## R3. What do CANCEL and the non-2xx ACK carry?

**Decision**: the full three-header block on both, whenever the INVITE had it.
**Rationale**: a CANCEL and a non-2xx ACK mirror the INVITE of the same
transaction. Since the INVITE carries the block when a Gm SA exists, mirroring
it means carrying the block, so FR-002 and FR-003 hold without special cases.
RFC 3329 §2.3.1 text on this is not in the repo; the live runs (FR-009) are the
real check. If a carrier rejects it, the fallback in the spec's Assumptions
applies.
**Alternatives**: `Security-Verify` only on CANCEL (needs a second block shape,
two code paths).

## R4. How is the helper's output shaped?

**Decision**: `fn sec_agree_headers(verify: Option<&str>) -> String` returning
either `""` or three CRLF-terminated lines, in the order
`Require`, `Proxy-Require`, `Security-Verify`. Builders push it as one string;
the un-REGISTER, which takes a `Vec<String>`, splits it on `\r\n`.
**Rationale**: the existing builders already format header text this way, and
the order matches what PR #99 live-verified.

## R5. How is the wiring tested without a SIM card?

**Decision**: `register_session` needs a USIM, so it stays untested at that
level. Test instead (a) the echo derivation from a synthetic multi-entry
`Security-Server` list as a pure function, (b) `RegisteredSession` fixtures with
a `GmSa` and every session-sent or dialog-built request asserted for the full
list, and (c) OPTIONS and un-REGISTER through a loopback listener.
**Rationale**: these are the points where a partial echo or a dropped
echo can appear.
**Alternatives**: a fake USIM and P-CSCF (large, and a mock of exactly the
kind the constitution discourages).

## R6. Live verification

Vodafone: local rig. Jio: remote Pi (`pi@192.168.100.2`, arm64 build). Both
need a placed call, a hang-up, and a look at keepalive and subscription
responses. See [quickstart.md](./quickstart.md).
