# Data Model: Q.850 Reason-based PBX reject detection

## `Disconnect` (pjsua-safe, in-memory, per call token)

| Field | Type | Meaning |
|---|---|---|
| `status` | `Option<u16>` | PJSIP's final status; `None` if PJSIP recorded none |
| `q850` | `Option<u16>` | cause from `Reason: Q.850;cause=N` on the received failure final response; `None` if absent, unparsable, not Q.850, or no response in the event |
| `response_seen` | `bool` | the event carried a received response (for FR-011a logging) |

Stored in the existing bounded log (64 entries, keyed by call token, oldest
dropped). Presence of the entry still means "the disconnect callback ran".

## `RejectStatuses` (gsm-sip-bridge, built once at start)

`{486, 600, 603} ∪ [sip].reject_statuses`. `contains(u16) -> bool`.
Invariants: every element in 400..=699; defaults always present.

## `PbxOutcome::Rejected`

`Rejected { status: u16, q850: Option<u16> }`. `status()` is unchanged;
a new `q850()` accessor feeds the record string.

## Call record string (existing control-message field)

`declined:pbx_rejected:<status>` with `:q850=<cause>` appended whenever a
cause was present. Example: `declined:pbx_rejected:403:q850=21`. A reject
with no status (cause only, status unreadable) is not possible: causes are
read only from a failure final response, which has a status.

## State transitions

None new. Classification order is in research.md R4.
