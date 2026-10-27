# Review rules for this repository

Distilled from the review history in `docs/greptile-review-learnings.md`. Flag a change that breaks one of these.

## State and transitions
- A field changed by the diff that other structs **copy at construction** (call dialogs, pending originations, status snapshots, retired lists) must be updated at the transition, or the diff must show why not. Flag stale copies, and invariant comments ("cannot change", "deferred while ...") that the diff makes false.
- After a connection, association or registration switch, every in-flight request kind (BYE, UPDATE, CANCEL, PRACK, SUBSCRIBE refresh) must carry a consistent tuple: Via sent-by, Security-Verify, connection and route. A CANCEL repeats its INVITE's Via and goes where the INVITE went.

## Logic that runs in two places
- Logic that must also run inside a blocking wait (ring wait, origination wait) must call the same entry points as the dispatch loop, not a re-implementation. Predicates such as `busy()` must see the new place. Queued replies are read before timers are judged.

## Existing contracts
- Error text matched by a classifier (`contains(...)`), and limits or retry counts defined by an existing path (`MAX_*`), are contracts: reuse them or match their semantics and check point exactly.

## Resources and rollback
- A guard disarmed early (`take`, `into_parts`) must not leave connections, reader threads, sockets or kernel state behind when a later step fails. Every thread or socket clone has a release path on success, error, cancel, rebuild and shutdown.
- Blocking calls on the dispatch loop need a bound and a note on whether existing code already blocks the same way.

## Docs and process
- Claims in `specs/*/live-verification.md`, release notes and docs must not exceed the evidence. Release notes follow `.cursor/rules/release-notes.mdc`.
- No real phone numbers, MSISDNs, IMSIs or ICCIDs anywhere; only synthetic placeholders.
- A requirement narrowed during implementation must be recorded as a deviation, not ticked as done.
