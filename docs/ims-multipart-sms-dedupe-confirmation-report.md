# Multipart IMS SMS leaves earlier segment deduplication claims unconfirmed

**Date:** 2026-11-15

**Tracking issue:** #206

**Reviewed source revision:** `5d2f7ef4f24980886dab44a9bcff5c0e5cd976a4`

**Status:** Historical diagnosis of the reviewed revision. The fix and completed verification are described in the resolution section below. The running binary's revision during the original investigation was not independently verified.

## Summary

When a multipart SMS is received through IMS, each segment gets its own deduplication claim. After the complete message is successfully relayed, the handler confirms only the claim belonging to the segment that completed reassembly. Claims for previously buffered segments remain admitted but unconfirmed, even though their content has already been included in the relayed message.

The completed reassembly buffer is then removed, leaving no completion path that will settle those earlier claims. While those claims remain in the deduplication cache, corresponding copies in modem storage cannot be cleared. The modem reader repeatedly waits for confirmation that will never arrive through the completed assembly.

Network acknowledgments and internal confirmation are separate operations. The bridge can send SIP `200 OK` and an SMS `RP-ACK` for every segment while still leaving earlier internal claims unconfirmed.

## Conditions

The defect is in the shared IMS SMS handler used by the VoWiFi and VoLTE paths. It requires:

1. An SMS with at least two valid concatenated segments.
2. The segments being admitted and assembled through the IMS handler.
3. Successful relay of the joined message over the local control channel.
4. Earlier segment keys still being present in the deduplication cache.

To observe retained modem copies, the same segments must also exist in modem storage and the associated modem reader must be running. The internal state defect does not require duplicate delivery through the modem.

“The completing segment” means the last segment to arrive. With out-of-order delivery, it need not have the highest sequence number.

## Expected and actual behavior

For a two-part SMS received in order:

| Event | Expected internal state | Actual internal state |
|---|---|---|
| Part 1 is buffered | Part 1 admitted, unconfirmed | Matches expectation |
| Part 2 completes the message, before relay | Both parts admitted, unconfirmed | Matches expectation |
| Joined message is successfully relayed | Both segment claims confirmed | Only part 2 confirmed |
| Completed assembly is removed | No unfinished claims belonging to the delivered assembly | Part 1 still admitted, unconfirmed |
| Modem reader encounters a copy of part 1 | Recognize completed delivery and clear the copy | Wait up to 8 seconds, then leave the copy in storage |

The same pattern applies to an N-part message: under these conditions, N−1 segment claims remain unconfirmed after successful completion.

## Root cause

### Segment identity is computed before reassembly

The handler formats a multipart segment as `[sequence/total] text`, builds an `InboundMessage`, and calculates its deduplication key before calling the assembler. The key is:

```text
sender + U+0001 + labelled_segment_body
```

The route and modem storage index are excluded, allowing matching IMS and modem segments to share a claim. Reassembly later replaces the outgoing body with the joined text, but the local `key` variable still identifies only the current segment.

Sources: [segment body and key construction](/home/ubuntu/gsm-sip-bridge/gsm-sip-bridge/src/ims/agent/mod.rs:1464), [deduplication key definition](/home/ubuntu/gsm-sip-bridge/gsm-sip-bridge/src/volte/sms.rs:85).

### Buffered segments correctly remain unconfirmed

The `PartOutcome::Pending` branch stores the segment in RAM and acknowledges it to the network without calling `Dedupe::confirm`. This is intentional: buffering alone must not authorize deletion of a backup copy from modem storage.

Source: [pending segment handling](/home/ubuntu/gsm-sip-bridge/gsm-sip-bridge/src/ims/agent/mod.rs:1589).

### Successful completion confirms only one key

After a successful control-channel relay, the handler performs the equivalent of:

```rust
dedupe.confirm(&key); // The current segment's key only.
reassembly.mark_delivered(&sender, &part);
```

`mark_delivered` removes the whole assembly from `buffers` and `order`. It does not settle the other segment claims. Since the assembly has been removed, the reassembly expiry path cannot subsequently flush those segments and confirm their keys either.

Sources: [confirmation after relay](/home/ubuntu/gsm-sip-bridge/gsm-sip-bridge/src/ims/agent/mod.rs:1650), [assembly removal](/home/ubuntu/gsm-sip-bridge/gsm-sip-bridge/src/volte/sms.rs:448).

### The modem reader requires explicit confirmation

For an existing claim, `sweep_modem_storage` calls `wait_for_resolution`. A confirmed claim allows deletion. A claim that remains admitted but unconfirmed causes an 8-second wait followed by `TimedOut`; the record is left in modem storage.

The cache holds 64 keys by default, has no time-based expiry, and is not persisted. Consequently, an orphaned claim can remain indefinitely on a quiet line. The 180-second reassembly timeout does not resolve it after the completed assembly has been removed.

Sources: [modem disposition handling](/home/ubuntu/gsm-sip-bridge/gsm-sip-bridge/src/volte/sms.rs:1016), [wait timeout and resolution](/home/ubuntu/gsm-sip-bridge/gsm-sip-bridge/src/volte/sms.rs:1092), [cache admission and capacity](/home/ubuntu/gsm-sip-bridge/gsm-sip-bridge/src/volte/sms.rs:120).

## Impact

- **Retained modem records:** Copies of earlier segments remain in storage despite their content having been relayed in the complete IMS message.
- **Repeated delays and possible starvation:** Each unresolved segment can consume up to 8 seconds per sweep. A backlog can repeatedly exhaust the sweep budget before later records are processed or the deletion phase is reached.
- **Storage pressure:** Retained records consume finite SMS storage. If this accumulates, new SMS reception may be affected. Storage exhaustion was not demonstrated during this investigation.
- **Possible duplicate fragments later:** Once a stale key is evicted or the process restarts, a retained modem copy can be admitted again and forwarded as a separate `[sequence/total]` message. The memory-only cache already permits duplicates across restarts independently of this defect; this defect leaves additional copies available for that outcome.
- **Incomplete acknowledgment of IMS retransmissions:** A retransmission of an earlier segment encounters an unconfirmed claim. The duplicate branch sends SIP `200 OK`, but suppresses `RP-ACK` while the claim remains unconfirmed. This can interfere with the network's delivery completion and potentially cause further retries or fallback delivery.

The joined message can still be relayed successfully. The immediate defect is unfinished per-segment bookkeeping after that relay, rather than failed concatenation.

Source for retransmission behavior: [duplicate acknowledgment branch](/home/ubuntu/gsm-sip-bridge/gsm-sip-bridge/src/ims/agent/mod.rs:1536).

## Runtime evidence

The original inspected log was [the IMS agent log](/tmp/ims-agent-0.out), which has since been overwritten by restarts. The excerpts below preserve the historical observations. All times are UTC on 2026-11-15. Subscriber identifiers and message contents are omitted.

### Every original segment received an outgoing acknowledgment

The available trace contains two multipart SMS messages, with three and eight segments:

| Message | Segment arrival interval | Segments seen | Outgoing acknowledgments |
|---|---|---|---|
| A | 14:48:40–14:49:07 | 1/3, 2/3, 3/3 | SIP `200 OK` and `RP-ACK` for each segment |
| B | 14:50:08–14:51:42 | 1/8 through 8/8 | SIP `200 OK` and `RP-ACK` for each segment |

For example, the three-part message produced these report records:

```text
14:48:40.635533Z sent the SMS delivery report report_call_id=c549158d4dd9ae3d rp_ack=021141020000
14:48:53.706733Z sent the SMS delivery report report_call_id=095f000e7e2a7d64 rp_ack=021241020000
14:49:07.267414Z sent the SMS delivery report report_call_id=1caf10a977c615e8 rp_ack=021341020000
```

These are shortened excerpts preserving the timestamps and correlation fields. Sending these reports does not invoke `Dedupe::confirm`: [the acknowledgment helper](/home/ubuntu/gsm-sip-bridge/gsm-sip-bridge/src/ims/agent/mod.rs:1825) operates independently of the deduplication cache.

### The modem backlog persisted across repeated sweeps

After the three-part message, the reader reported three stored records at 14:49:20 and two at 14:50:00. Later, it repeatedly reported ten records and exhausted its pass budget:

```text
2026-11-15T14:52:56.015484Z INFO found messages in modem storage; relaying and clearing them count=10
2026-11-15T14:53:36.022380Z WARN sweep pass out of budget; leaving the remaining messages unrelayed for the next pass from_index=5
2026-11-15T14:54:06.462087Z INFO found messages in modem storage; relaying and clearing them count=10
2026-11-15T14:54:46.473339Z WARN sweep pass out of budget; leaving the remaining messages unrelayed for the next pass from_index=5
2026-11-15T15:03:28.934545Z INFO found messages in modem storage; relaying and clearing them count=10
2026-11-15T15:04:08.941874Z WARN sweep pass out of budget; leaving the remaining messages unrelayed for the next pass from_index=5
```

The unchanged backlog and repeated budget exhaustion are consistent with the stale-claim failure path. The log does not print individual confirmation states or timeout outcomes, so it does not independently establish the identity or cause of every retained record. The root cause above is established by source inspection.

### Separate observation: the network rejected the reports

All eleven outgoing `RP-ACK` reports received SIP `400 Bad Request`, correlated by `report_call_id`, with this warning text:

```text
SMS: Failed to find user scb list.
```

This is a separate observed problem. The trace establishes that the bridge sent the acknowledgments and that the network rejected them. It does not establish that stale internal claims caused those rejections. Repairing deduplication confirmation alone should not be described as a fix for these `400` responses.

## Reproduction

A deterministic local harness is preferable to relying on a carrier to deliver the same SMS over both routes.

1. Create shared `Dedupe` and `Reassembly` instances and a functioning local control-channel receiver.
2. Feed a valid two-part IMS SMS through the production handler, using a unique sender and segment bodies to avoid unrelated key collisions.
3. After part 1, verify that its claim is admitted but unconfirmed.
4. Feed part 2 and allow the joined message relay to succeed.
5. Verify that the receiver obtains one joined message.
6. Inspect both original segment keys. Currently, part 1 remains unconfirmed and part 2 is confirmed.
7. Present matching segment records to the modem sweep through an AT test fixture. Verify that part 1 times out and remains stored, while part 2 is eligible for deletion, subject to the pass budget.
8. Optionally, evict part 1's key and sweep again. The retained record can then be relayed as an individual fragment.

Repeat with arrival order `2/2`, then `1/2`: the claim left unconfirmed belongs to the earlier arrival, demonstrating that the defect follows arrival order rather than the segment number.

These are proposed reproduction steps; a new executable harness was not implemented or run as part of this report.

## Recommended correction

Preserve the exact deduplication keys for all segments belonging to a completed assembly. Once the joined message relay succeeds, confirm every constituent key still resident in the cache before removing the matching assembly state.

The assembler can retain the original key alongside each segment or expose enough metadata to reproduce it through a shared key-construction helper. The completion result should provide the joined text and a snapshot of the member keys. Confirming the joined message's own text key would not settle the per-segment claims.

Maintain the existing failure behavior:

- Keep pending segments unconfirmed while they exist only in RAM.
- Do not confirm any segment merely because all parts have arrived.
- On failed relay, retain the completed buffer and preserve the retry path for the triggering segment.
- On a successful retry, confirm all constituent claims, including those from earlier arrivals.
- Use a short deduplication critical section for the group confirmation; do not hold its lock during network or AT operations.

The current success boundary is a successful TCP `write_msg` to the control channel, not an explicit acknowledgment of a committed SQLite transaction. This report uses “successfully relayed” in that precise sense. Stronger persistence guarantees would require separate work.

## Regression coverage

Add tests covering:

1. Two-part successful relay: all constituent claims still resident in the cache become confirmed and the assembly is removed.
2. Out-of-order and larger multipart messages within cache capacity: every retained member claim is confirmed, regardless of which segment completes the set.
3. Pending state: no confirmation before relay success.
4. Relay failure and retry: no premature confirmation or buffer removal; successful retry settles all retained member claims and emits the joined message once.
5. Modem copies after successful IMS delivery: resident confirmed keys permit clearing without duplicate relay or the 8-second claim wait.
6. IMS retransmission after completion: an earlier segment receives the configured `RP-ACK` without another user-visible delivery.
7. Existing cache bounds and incomplete-message expiry behavior remain intact; confirmation of an already-evicted key remains a no-op.
8. Completing one assembly leaves unrelated assemblies and their claims untouched.

The existing [dedupe/reassembly composition test](/home/ubuntu/gsm-sip-bridge/gsm-sip-bridge/src/volte/sms.rs:1818) checks segment suppression and joined text, but does not exercise the handler's relay-success transition or assert that every constituent claim is confirmed. Regression coverage needs to exercise that production transition through a handler harness or a shared completion helper used by the handler.

## Resolution and completed verification

The fix for #206 includes two related corrections:

- After successfully relaying an assembled SMS, confirm every constituent claim still resident in the cache before removing the assembly.
- Preserve the original seven TP-SCTS bytes on both IMS and modem reception and include them in the deduplication key. Without this, a later legitimate SMS with identical text could have its earlier segments suppressed by old modem-delivery claims.

Each buffered segment retains its exact original deduplication key, including its own timestamp. Completion, failed-relay retries, expiry, capacity eviction and concatenation-reference reuse preserve those keys. Different timestamps on individual segments do not split one multipart assembly into separate groups. The cache remains bounded and in memory; this change does not introduce persistent deduplication across restarts.

Previously completed automated validation:

- Workspace tests: 3,227 passed, zero failed; one test requiring `CAP_NET_ADMIN` was ignored.
- Formatting, Clippy across all targets, and the zero-unsafe audit passed. Optional `cargo-deny` and `shellcheck` checks were unavailable in the environment.
- The four shell integration suites passed during preparation of the fix.
- Production-handler regressions cover identical text sent again with a new timestamp, old modem claims for the first two parts, two-, three- and eight-part assemblies, out-of-order delivery, independent assemblies, failed-relay retries, expiry retries and cache eviction. Parser and modem tests cover timestamp preservation and matching identities across routes.

Live verification on 2026-11-15 found two separately accepted, complete copies of the same 175-character SMS in database records 133 and 134, received at 16:27:46 and 16:43:16. Following the subsequent restart, the agent registered at 16:44:14. Through the check at 16:46:20, no modem fragments were replayed, no new SMS records appeared, and the new agent log contained no modem-sweep errors or budget exhaustion.

Because restart clears the deduplication cache, remaining readable modem copies would have been eligible for relay again. Their absence after restart provides strong indirect evidence that the copies were cleared. Successful `CMGD` commands and empty `CMGL` results are not logged, so the trace does not directly prove each hardware deletion.

The previously observed SIP `400 Bad Request` responses to RP-ACK reports are a separate unresolved issue; this fix does not claim to correct those network responses.
