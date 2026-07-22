# Contract: `ImsTransport`

**Feature**: `015-volte-host-ims` | **Satisfies**: FR-017, FR-018, SC-007

The seam that lets one registration implementation serve two access networks.
This is the only new abstraction in the feature; its justification is recorded
in `plan.md` → Complexity Tracking.

## Purpose

`ims::register_session` currently reaches the P-CSCF over whatever network the
process happens to sit in, with the ePDG tunnel established out-of-band. This
contract makes that dependency explicit and substitutable, so the same
registration code runs unchanged over either access network.

## The contract

An implementor is responsible for **producing a network position from which
IMS signalling can reach the carrier's P-CSCF**, and for tearing it down.

```
prepare()  -> ImsTransportHandle | Error
teardown() -> ()
```

### `prepare()`

Must, on success, guarantee all of:

| Guarantee | Why |
|---|---|
| A usable network attachment exists and is bound to the host | FR-002 |
| A `PcscfEndpoint` is known — discovered or configured | FR-007, FR-010 |
| The local source address to use for signalling is known | Via/Contact headers must not be unspecified |
| The attachment is idempotent — calling `prepare()` twice does not duplicate it | FR-004 |

Returns an `ImsTransportHandle` carrying:

| Field | Notes |
|---|---|
| `pcscf` | Address + port of the entry point |
| `local_addr` | Source address for SIP signalling. **May be IPv6** |
| `discovery_report` | How `pcscf` was found (FR-009, FR-011); `None` for transports where it is handed over rather than discovered |
| `descriptor` | Human-readable identification of the transport, for diagnostics |

### `teardown()`

Must revert every piece of host configuration the implementor applied,
including restoring any displaced prior state (FR-005). Must be safe to call
when `prepare()` failed part-way, and safe to call twice.

## Error reporting

Errors must carry the `RegistrationStage` at which they occurred
(`AttachingPdn` or `DiscoveringPcscf`) so FR-015's required distinctions
survive the abstraction boundary. A transport error that surfaces as an
undifferentiated failure violates this contract.

## Implementors

### `EpdgTransport` (existing VoWiFi path)

- **Attachment**: the strongSwan IPsec tunnel and its XFRM interface, already
  established by `docker/entrypoint.sh` before the agent starts.
- **P-CSCF**: handed over during tunnel setup; `discovery_report` is `None`.
- **Constraint**: adopting this contract **must not change VoWiFi behaviour in
  any observable way** (FR-019). This is an adapter over existing behaviour,
  not a rewrite. Its correctness bar is that the existing VoWiFi test suite
  passes unmodified.

### `LteImsPdnTransport` (new)

- **Attachment**: the IMS PDN, established over AT and bound to the host
  interface.
- **P-CSCF**: the ordered discovery chain, or the configuration override.
- **Constraint**: must tolerate an IPv6-only attachment.

## Contract tests

Both implementors must pass the same behavioural suite (`test_ims_transport.rs`):

| Test | Assertion |
|---|---|
| Idempotent prepare | Two `prepare()` calls yield one attachment, not two |
| Teardown restores | Host configuration after `teardown()` matches the pre-`prepare()` state |
| Teardown after partial failure | A `prepare()` that fails mid-way still leaves `teardown()` safe and complete |
| Double teardown | Second call is a no-op, not an error |
| Handle completeness | A successful `prepare()` always yields a routable `local_addr` and a `pcscf` |
| Staged errors | Failures carry the correct `RegistrationStage` |

Per Constitution Principle I these run against real implementations, with the
external peer (modem, tunnel) simulated at the wire level rather than the
component under test being mocked.

## Non-goals

- **Media transport.** This contract covers signalling reachability only.
  Calls are out of scope for this feature; extending the handle to carry media
  parameters is a follow-up concern.
- **Transport selection policy.** Choosing which transport to use is the
  caller's business, not the contract's.
