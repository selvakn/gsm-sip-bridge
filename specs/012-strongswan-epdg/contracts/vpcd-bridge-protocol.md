# Contract: vpcd ↔ `vowifi-usim-bridge` (virtual USIM over the modem)

The chain: `charon (eap-sim-pcsc) → libpcsclite → pcscd → vpcd IFD handler → TCP →
vowifi-usim-bridge → AT+CSIM → EC200U-resident SIM`.

The bridge implements the **virtual card** side of vsmartcard's vpcd protocol. Everything in
this contract is unit/integration-testable without hardware (real TCP sockets + APDU byte
fixtures; scripted modem transport per the existing `at_commander.rs` test precedent).

> **Verify at implementation** (research.md item 2): frame layout and control bytes below are
> from the vsmartcard project's documented protocol and must be confirmed against the vendored
> vpcd source at build time; the fixture suite encodes whatever the verified truth is.

## Transport & framing

- Bridge connects (TCP client) to vpcd's listener, `VPCD_HOST:VPCD_PORT`
  (default `127.0.0.1:35963`), reconnecting with backoff on loss.
- Every message both directions: 2-byte big-endian length prefix + payload.

## Control messages (payload length 1, vpcd → bridge)

| Byte | Meaning | Bridge response |
|---|---|---|
| `0x00` | Power off | none; release the modem serial port |
| `0x01` | Power on | none; acquire serial port (retry with backoff while busy — shared port), run session prologue |
| `0x02` | Reset | none; re-run session prologue (card state reset) |
| `0x04` | Request ATR | ATR bytes (canned USIM ATR constant — `AT+CSIM` cannot read the real one; assumption: `eap-sim-pcsc` treats ATR as opaque) |

Payload length > 1 ⇒ command APDU; bridge replies with exactly one response APDU
(data ‖ SW1 SW2).

**Session prologue** (on power-on/reset): SELECT MF (3F00), discover the real USIM AID via
EF_DIR (reuse `usim.rs::discover_usim_aid`), so subsequent normalization has the AID available.

## APDU forwarding & normalization (the quirk-adapter role)

Default path: hex-encode the APDU into `AT+CSIM=<len>,"<hex>"`, parse `+CSIM` reply, return
data+SW. Normalizations applied transparently (all derived from the proven fixes in
`docker/patches/0001-ec200u-at-csim-fixes.patch`):

1. **GET RESPONSE emulation.** The EC200U auto-chains internally and returns full data with
   `SW=9000`. Contract with the PC/SC client preserved both ways:
   - If the client's protocol expects `61xx` + `GET RESPONSE` (`00 C0 00 00 xx`): the bridge
     caches the full response, returns `61 <len>` first, and serves the cached data on the
     following GET RESPONSE without touching the modem.
   - If the modem itself returns `61xx` (other firmware), the bridge performs the GET RESPONSE
     against the modem itself and returns assembled data.
   Decision of which face to present is driven by what the client sends (transparent either way).
2. **SELECT P2 tolerance.** If a `SELECT` (INS `A4`) with `P2=0x00` comes back `SW=6B00`, retry
   once with `P2=0x0C` (return-nothing FCI variant) and return that result. (Cards verified in
   this project reject `P2=0x00`.)
3. **AID redirect.** If the client SELECTs a USIM AID (RID `A0 00 00 00 87 10 02`) that is not
   the discovered one, substitute the discovered AID (different operators' SIMs have different
   AIDs — hardcoded generic AIDs are the documented failure mode).
4. **Serial discipline.** One APDU transaction at a time; response wait tolerant of slow
   AUTHENTICATE (seconds — never retransmit inside a pending transaction, per patch item 4);
   hex-validation of `+CSIM` fragments before parsing.

Anything not matching a normalization rule is forwarded verbatim — the bridge must not
understand EAP-AKA; AUTHENTICATE (INS `88`) flows through as opaque APDUs, including AUTS
sync-failure responses (the client/SIM handle resync semantics).

## Error mapping

| Condition | Bridge behavior |
|---|---|
| Serial port busy at power-on | retry/backoff up to a bounded window, then respond to subsequent APDUs with `SW=6F00` (card mute) so charon fails the EAP round cleanly and retries IKE_AUTH |
| Modem returns `ERROR` / garbage | `SW=6F00`, log the raw exchange |
| vpcd connection lost | drop session, release port, reconnect loop |
| Bridge restart (supervised) | safe: vpcd/pcscd tolerate reader "card removed/inserted" transitions |

## CLI surface

```
gsm-sip-bridge vowifi-usim-bridge --modem <path> [--vpcd-host 127.0.0.1] [--vpcd-port 35963]
gsm-sip-bridge vowifi-imsi --modem <path>          # prints IMSI, used by entrypoint templating
```

Both are pre-daemon subcommands dispatched from `main.rs` like `ims-register`/`ims-call`;
`vowifi-usim-bridge` runs supervised by the entrypoint (strongSwan engine only), in the
**default** namespace (pcscd and the modem device live there; charon too).
