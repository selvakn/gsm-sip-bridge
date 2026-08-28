# Quickstart: verifying MT-06 (SDP QoS preconditions)

## Unit tests

All of this feature's logic is pure (`sdp.rs`'s new parsing/verdict/answer
functions, plus `agent/mod.rs`'s `SUPPORTED_EXTENSIONS` change and one new
`agent/inbound.rs` decline path) — no live socket/session harness needed,
same posture as batch 4. `make test` (whole workspace) exercises every new
function:

- `unsupported_required_extensions` no longer lists `precondition` as
  unsupported on its own (a direct test alongside the existing MT-03/MT-04
  confirmation tests in `agent/mod.rs`), while an unrelated `Require` tag
  combined with `precondition` still gets `420` with only the other tag in
  `Unsupported`.
- `sdp.rs`: a new fixture family (mirroring `PJSIP_REAL_VETH_OFFER`'s
  const-fixture-plus-`sdp.contains(...)` style) covering every row of
  `contracts/precondition-answer-contract.md` — `remote`/`e2e`/`local`
  status types, `mandatory`/`optional` strengths, absent `a=des:qos`
  entirely, and the two-line (`remote` + `e2e`) combined case.
- A dedicated test proves the offer's `local`-status-type `a=curr:qos`
  line is mirrored through inverted to `remote` in the answer byte-for-byte
  (User Story 3) — never a value the bridge computed itself.
- `agent/inbound.rs`: a test that an offer combining `mandatory e2e` with
  an otherwise-perfect codec/transport still reaches `580` rather than
  falling through to the codec/transport declines, confirming ordering
  (contract row 9).
- A test that an offerless INVITE (SDP-04's empty-body branch) with
  `Require: precondition` still proceeds through `handle_offerless_invite`
  unaffected (Decision 5).

## Hardware round

Per this spec's Clarifications: **regression-only**. No carrier reachable
here (Jio, or the `test/` rig) has ever sent `Require: precondition`, and
only the carrier itself can reach `agent::inbound::handle_invite` at all —
so the new precondition-handling code cannot be exercised by a live call
regardless of effort spent trying.

Rebuild and retag the image, redeploy to the real Pi, re-register the real
line, drive one ordinary real inbound call via the user's phone (no
preconditions involved — an ordinary handset offer has none) and confirm
no regression: call answers, audio both ways, clean hangup, exactly as
every prior batch's hardware round already establishes. That call's offer
has no `a=des:qos` lines, so per FR-007 it takes the exact same path as
before this feature — this round proves the *existing* paths (SDP-01/02/03
media handling, codec selection, offerless-INVITE) still work with the new
code compiled in, not that the new precondition logic itself works live.
