# Research: Q.850 Reason-based PBX reject detection

Checked against the code on `main` (PR #110 merged). Nothing below has been
run against a live PBX yet; R1 is the one the plan stakes the most on and is
verified first (task T001).

## R1. Can the Reason header be read on the final response?

**Decision**: Read it inside `on_call_state_cb` (`pjsua-safe/src/endpoint.rs`)
when the call reaches `DISCONNECTED`, from the `pjsip_event` it is handed.

**Evidence (static)**:
- `disconnect_trigger()` already extracts the received message from the same
  event: for `PJSIP_EVENT_TSX_STATE` it takes `tsx_state.src.rdata`, and when
  the inner kind is `RX_MSG` it reads `(*rdata).msg_info.msg`.
- `pjsip_msg_find_hdr_by_name(msg, name, start)` is in the generated
  bindings. PJSIP has no typed parser for `Reason`, so it comes back as a
  `pjsip_generic_string_hdr` whose `hvalue` is the raw text.

**Not yet proven**: that a rejected outgoing INVITE always delivers its final
response in this event. If a disconnect is driven by a timer or transport
error, `rdata` is null. That case falls back to the status code (spec
FR-011a), so the worst outcome is today's behaviour.

**Alternatives considered**:
- Add a `pjsua_callback` hook such as `on_call_tsx_state`: more code, same
  data.
- Read `info.last_status_text`: PJSIP's own reason phrase, not the header.

## R2. Where the cause is stored

**Decision**: widen the per-call disconnect log entry from `Option<u16>` to a
small `Disconnect { status: Option<u16>, q850: Option<u16> }`. Keep keying by
call token, the 64-entry bound and the "entry present = disconnect seen" rule
(these exist for the races documented on `DISCONNECT_STATUS`).

**Rationale**: the cause is captured in the same callback at the same instant
as the status, so it inherits the same race protection for free. A second
store would need its own ordering guarantee.

**Alternative**: a second `DISCONNECT_CAUSE` map. Rejected: two locks, two
eviction policies, and a window where status exists without cause.

`Call::last_status()` stays as is. A new `Call::last_disconnect()` returns
both; `last_status()` becomes a thin wrapper so existing callers and tests
need no change.

## R3. Parsing the header

**Decision**: a pure function `parse_q850_cause(&str) -> Option<u16>` in
`pjsua-safe`, unit-tested without PJSIP.

Rules (spec FR-009):
- Header name matched case-insensitively (PJSIP's lookup already is).
- A value is `protocol ; param ; param`. Take the first token before `;`,
  trimmed, case-insensitive compare to `Q.850`. Other protocols (`SIP`) are
  ignored.
- Find `cause=N` among the parameters; `N` must parse as `u16` and be in
  0..=127 (Q.850 causes are 7 bits). Anything else is `None`.
- A message may carry several Reason headers. Iterate with the `start`
  cursor of `pjsip_msg_find_hdr_by_name` and take the first Q.850 one.
- Quoted `text="..."` may contain `;` or `cause=`; split with quote
  awareness, so `text="cause=9"` is not read as a cause.

## R4. Classification

**Decision**: replace `classify_pbx_disconnect(status)` with
`classify_pbx_disconnect(&Disconnect, &RejectStatuses)`.

Order (spec FR-001..FR-005):
1. `q850` is 21 or 17 and the status is a failure (400..=699): `Rejected`.
2. status in the reject set (defaults 486/600/603 plus config): `Rejected`.
3. status 480 or 408: `NoAnswer`.
4. else `Failed`.

Cause beats status, so `408` + cause 21 is `Rejected`. A cause on a status
outside 400..=699, or a missing status, is ignored (FR-008a).

`PbxOutcome::Rejected` gains `q850: Option<u16>` so the record can carry it.
The cause is attached whenever present (FR-010), including when the status
alone would have decided.

## R5. Reject set and config

**Decision**: `[sip].reject_statuses`, an optional list of integers.
`RawSip.reject_statuses: Option<Vec<u16>>`; `build.rs` validates each is in
400..=699 and returns `BridgeError::Config("[sip].reject_statuses: ...")`
naming the value, as `parse_phone_codecs` does for `codecs`. The effective
set is `{486, 600, 603} ∪ configured`, built once and passed down; the
defaults cannot be removed (FR-006). 403 is not a default (FR-005).

**Alternative**: a `Vec<u16>` of `u16` in the domain type. Chosen: a small
`RejectStatuses` newtype with `contains()` so the defaults live in one place
and the classifier cannot be handed a bare slice that forgets them.

## R6. Plumbing

`wait_for_pbx_answer(pbx_call, ctrl_rx)` is called from
`vowifi/mod.rs` (agent B) and its tests. It gains a `&RejectStatuses`
parameter, sourced from `config.sip` where agent B is started (the same
`config.sip` already used for `amr_octet_align`, `vowifi/mod.rs:308`).
The control wire format does not change: the reason is still the string
`declined:pbx_rejected:<status>[:q850=<cause>]` built at the existing
`format!("declined:{reason}:{status}")` site.

## R7. Observability

- Call record: `declined:pbx_rejected:403:q850=21` (FR-010).
- When the outcome is not `Rejected` and a header was present or the event
  carried no response, the existing "PBX leg ended" log line gains
  `q850` and `response_available` fields (FR-011, FR-011a).
- Metrics: unchanged labels. A cause-decided reject is still
  `pbx_declined`'s reject counterpart (`pbx_rejected`).

## R8. Live verification

Use the privileged test container (see `test/`) with `siptest` as the
telephone side answering `403` + `Reason: Q.850;cause=21`, then `503` +
cause 21, then bare `403` with and without `reject_statuses = [403]`.
Confirm the carrier-side leg gets `486`/`480` as specified. Capture with
`tcpdump` inside the container.
