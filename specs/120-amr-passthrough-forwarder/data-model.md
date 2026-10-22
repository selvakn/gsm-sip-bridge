# Data model: AMR passthrough forwarder

All types are in-memory. Nothing is stored in SQLite.

## AmrLegParams (`amr_forward/params.rs`)

What one leg negotiated. Built once from the carrier's SDP (Agent A) and once from the client's answered call (Agent B); serialised in the control protocol.

| Field | Type | Notes |
|---|---|---|
| `flavour` | `Nb` \| `Wb` | reuses `ims::amr_rtp::AmrKind` |
| `octet_aligned` | bool | packing; may differ between legs |
| `mode_set` | `u16` bitmask, `None` = all | bits 0..7 (NB) or 0..8 (WB) |
| `frames_per_packet` | u8 | from `ptime`/`maxptime`, 1 for the usual 20 ms |
| `crc`, `robust_sorting`, `interleaving` | bool | any `true` fails compatibility |
| `payload_type` | u8 | the leg's audio PT |
| `dtmf_payload_type` | `Option<u8>` | the leg's telephone-event PT |
| `clock_rate` | u32 | 8000 / 16000, derived from `flavour` |

Validation: `mode_set` bits outside the flavour's range → parse error → `reason=feature`. Unknown fmtp keys are ignored unless they are one of the three feature flags.

## MediaPath (`amr_forward/params.rs`)

```
Forwarded { flavour, carrier_oa: bool, client_oa: bool }
Transcoded { reason: Reason }
```

`Reason` ∈ {`Disabled`, `Outbound`, `CarrierNotAmr`, `ClientNotAmr`, `FlavourDiffers`, `ModeSet`, `Feature`, `Ptime`, `Dtmf`, `AckTimeout`, `ReinviteFailed`}, each with a stable `as_str()` used verbatim in logs and the call record. Decided once per call; the type has no transition from `Forwarded` back to `Transcoded` after `CallAnswered`.

## ForwardSession (Agent B, `vowifi/mod.rs`) and ForwardLeg (Agent A)

| Field | Notes |
|---|---|
| `client_socket` (B) | forwarder RTP/RTCP pair advertised to the client, even port, RTCP = port+1 |
| `veth_socket` (B) / `fwd_socket` (A) | plain UDP pair on the veth, connected to each other |
| `stop` | `Arc<AtomicBool>`, same shape as `relay_rtp`'s |
| `direction` | `AtomicU8` per direction: `active` / `held` |
| `counters` | per-direction received packets |
| `path` | the `MediaPath` for the call record |

Lifecycle: created at upgrade start → `ForwardAck` → client re-INVITE → `CallAnswered` → relay running → dropped at call end or on any setup failure. `Drop` closes the sockets and joins the threads (FR-017).

## Per-call registry (`pjsua-safe`)

`HashMap<pjsua_call_id, ForwarderAddr>` behind a `Mutex`, read by `on_create_media_transport` and `on_stream_created2`, written by `register_forward` just before the re-INVITE, cleared when the call disconnects. An absent entry means "today's path".

## Control protocol additions

See [contracts/control-protocol.md](contracts/control-protocol.md). `CallRecord.media_path: Option<String>` is the only persisted-in-memory addition to existing types.
