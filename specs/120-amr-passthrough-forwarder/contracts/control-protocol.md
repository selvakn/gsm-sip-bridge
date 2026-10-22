# Contract: control-channel additions (Agent A ↔ Agent B)

Line-delimited JSON as today (`#[serde(tag = "event", rename_all = "snake_case")]`). Every addition is optional on the wire so a mixed-version pair degrades to transcoding.

## IncomingCall (A → B), extended

```json
{"event":"incoming_call","call_id":"…","caller":"…","caller_name":null,
 "carrier_amr":{"flavour":"wb","octet_aligned":true,"mode_set":null,"frames_per_packet":1,
                "crc":false,"robust_sorting":false,"interleaving":false,
                "payload_type":104,"dtmf_payload_type":105}}
```

`carrier_amr` is present only when Agent A has `amr_passthrough` on and the carrier codec it chose is AMR. Absent → Agent B never attempts an upgrade (`reason=disabled` or `carrier_not_amr`). An older Agent A omits it; an older Agent B ignores it.

## ForwardMedia (B → A)

```json
{"event":"forward_media","call_id":"…","rtp_port":40010,
 "client":{ …AmrLegParams… }}
```

Sent after the client answered and Agent B judged the call compatible. Agent B has bound the veth-side forwarder socket at `rtp_port` on its veth address. Agent A binds its own socket, connects it to `<B veth addr>:rtp_port`, re-checks compatibility with `client`, and replies.

## ForwardAck (A → B)

```json
{"event":"forward_ack","call_id":"…","ok":true,"reason":null}
```

`ok:false` carries a `Reason` string; Agent B then leaves the call transcoded. No reply within 2 s counts as `ack_timeout`.

## ForwardCancel (B → A)

```json
{"event":"forward_cancel","call_id":"…"}
```

Sent when Agent B abandons the upgrade after a successful `ForwardAck` (client re-INVITE failed, post-reinvite recheck failed). Agent A discards the pending forwarding socket and uses the normal transcoding relay.

## Ordering

`IncomingCall` → `BridgeReady` → *(client answers)* → [`ForwardMedia` → `ForwardAck`] → *(client re-INVITE)* → `CallAnswered`. Agent A starts the forwarding relay only on `CallAnswered` with an acked forward pending; otherwise `spawn_transcoding_relay` exactly as today. `CallEnded` and `HangupAck` are unchanged.

## CallRecord (status reply), extended

`media_path: Option<String>` with `serde(default)`; format in [logs.md](logs.md).
