# Quickstart: verifying dialog/transaction identity matching

## Unit tests (fast, no hardware)

```bash
cd gsm-sip-bridge
cargo test --lib ims::agent::call::tests
cargo test --lib ims::agent::mod::tests
make format && make lint && make test   # mandatory before any commit, whole workspace
```

Expect new tests: `classify_in_dialog_invite_*` (`call.rs`),
`names_active_call_*`, `cancel_response_*`, `bye_response_if_unmatched_*`
(`mod.rs`) — see `plan.md` Project Structure for the exact list.

## Hardware verification (real line, same rig as batches 1/2)

Uses the pinned-image test rig at `test/` (see its own docs) against the
on-host EC20 line and the `siptest` companion softphone standing in for the
PBX extension — the same pattern already used for batches 1 and 2.

1. Rebuild and retag: bump `test/docker-compose.yaml`'s `image:` tag (e.g.
   `gsm-sip-bridge:dialog-identity`), `docker compose up -d`, confirm the
   line re-registers.
2. **BYE mismatch (User Story 1)**: harder than it looks — Agent A's carrier-facing
   SIP socket binds inside the `ims` network namespace behind the IPsec
   tunnel (`ip netns exec ims<line> ...`), not on a plain host-reachable
   port, so "send a raw BYE at it" means crafting a well-formed TCP/UDP SIP
   message from *inside* that namespace, not a script run from the host.
   Given the real live line, this was judged not worth attempting against a
   call actually in progress; the no-active-call branch
   (`bye_response_if_unmatched_refuses_481_with_no_active_call`) could be
   tried safely between calls if desired. Otherwise this story's live
   coverage is the regression check below (a real BYE-driven teardown, from
   the PBX side via `hangup_carrier`, completed with no errors) plus its
   three unit tests.
3. **Retransmission (User Story 2)**: place a real inbound call from a
   handset; if the network/handset retransmits the INVITE (not always
   reproducible on demand), confirm no double-ring or duplicate bridge
   attempt appears in the logs. This is the one row of the contract that may
   end up verified by unit test alone if it can't be provoked live.
4. **CANCEL-after-answer**: harder to provoke from a real handset (requires
   racing a CANCEL against the answer) — acceptable to leave unit-test-only
   if not reliably reproducible live, same as the existing CANCEL-during-ring
   test coverage.
5. **Re-INVITE decline (User Story 3)**: no carrier in current use has been
   observed sending one (see `spec.md` Assumptions) — unit-test coverage via
   `classify_in_dialog_invite`/`InDialogInvite::ReInvite` stands in; note this
   explicitly in the hardware-verification log rather than claiming it was
   exercised live.
6. **Regression check**: an ordinary call (ring → answer → real hangup) and
   `OPTIONS` keepalives must behave exactly as in the batch 2 hardware round.

Use `/discord-notify` to ask the user to place/receive calls at the right
points, matching the pattern already used for batches 1 and 2.
