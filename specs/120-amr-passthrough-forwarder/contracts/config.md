# Contract: `[vowifi].amr_passthrough`

```toml
[vowifi]
# Forward AMR audio between the carrier and an AMR-capable client without
# decoding it. Inbound calls only. Off by default.
amr_passthrough = false
```

- Type: boolean. Default `false`. Absent = `false`.
- Anything other than a TOML boolean stops the bridge at startup (strict parsing, FR-015).
- Global, not per line. Read at startup; changing it needs the container recreated.
- Applies to VoWiFi and VoLTE alike.
- Shell export: `AMR_PASSTHROUGH=true|false` from `gsm-sip-bridge config` (for confirmation; no script reads it).
- No effect on the circuit-switched GSM path.
- Documented in `docs/configuration.md` and `config.toml.example`, with the PBX caveat (stock Asterisk does not pass AMR through) and how to read the path from logs.
