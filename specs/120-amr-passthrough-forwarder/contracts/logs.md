# Contract: how an operator reads the path a call took

Agent B, one INFO line per inbound call, emitted before `CallAnswered` is sent:

```
INFO media path decided call_id=… path=forwarded flavour=amr-wb carrier_packing=octet-aligned client_packing=bandwidth-efficient
INFO media path decided call_id=… path=transcoded reason=mode_set
```

Agent A's existing `call answered and bridged` line gains `forwarding=true|false`; `transcoding` stays and is `false` on a forwarded call.

`CallRecord.media_path` (visible in `vowifi-status` recent calls):

| Value | Meaning |
|---|---|
| `forwarded:amr-wb:oa/be` | forwarded; carrier octet-aligned, client bandwidth-efficient |
| `forwarded:amr-nb:oa/oa` | forwarded, same packing |
| `transcoded:<reason>` | not forwarded; reason from the `Reason` list in data-model.md |

When the setting is off, Agent B still records `transcoded:disabled` only for calls whose carrier leg was AMR (so non-AMR calls and the GSM path produce no new output).
