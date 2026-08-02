# Quickstart: Outbound Calling

**Feature**: 025-outbound-calling

Place a call out over the mobile network from the SIP side — a PBX-sent
call, or a phone registered directly to the bridge's own SIP server mode.

---

## 1. Configure

Add to `config.toml`:

```toml
[outbound]
enabled = true
```

That's it — no allow-list, no card selection, no per-path priority (spec.md
Clarifications, 2026-08-02). The bridge dials whatever destination it is
given, on whichever configured line is idle.

Works alongside either SIP-side topology already configured:

- **With a PBX** (`[bridge].sip_destination`/`[sip].server` set as usual):
  configure the PBX to route an outbound call to the bridge's existing trunk
  registration — no new address to give it.
- **With SIP server mode** (`[sip_server]`, spec 024): any phone that
  successfully registers may now also dial out, not only the `ring_aor`
  account.

## 2. Start

```bash
make run          # or: docker compose up -d
```

## 3. Place a call

From the PBX side: dial a route configured to reach the bridge, with the
destination number as the dialed digits.

From a phone in SIP server mode: dial the destination number directly, as
you would dial any extension.

Expected: the number is dialed out over the mobile network on an idle SIM;
once answered, audio flows both ways.

## 4. If nothing happens

- **No SIM idle**: the caller hears an immediate reject (`503`), not a long
  ring-out. Check `gsm_sip_bridge_outbound_attempts_total{outcome="refused_no_idle_line"}`.
- **Wrong number reaches the carrier**: the bridge dials exactly what it was
  given — verify the PBX dial plan or phone isn't prefixing/stripping digits
  before it reaches the bridge (this feature does neither).
- **Phone in SIP server mode gets `403` instead of being redirected**: it is
  not currently registered, or `[outbound].enabled` is not set — a phone
  must authenticate (spec 024) before it is eligible to dial out.

## 5. Diagnose an outcome

```bash
curl -s localhost:9100/metrics | grep gsm_sip_bridge_outbound
```

`gsm_sip_bridge_outbound_attempts_total` is labelled by `outcome`:
`placed`, `refused_no_idle_line`, `refused_invalid_destination`,
`refused_network_failure`, `unanswered` — matching the granularity already
available for inbound calls (FR-015).
