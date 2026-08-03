# Contract: the cross-process line-command protocol

**Feature**: 025-outbound-calling
**Rescoped 2026-08-03** (research.md R-003, revised): this channel is
needed **only** for the genuinely cross-process case — reaching a
VoWiFi/VoLTE line's agent process from whichever process owns the SIP side.
Circuit-switched modems never need it: they always live in the daemon's
`CardPool`, and the daemon-internal case is instead `ControlCmd::Dial`
(`control-cmd-dial.md`), which reuses the existing `ControlCmd`/`ModuleCmd`
mechanism with no new socket. Deferred to plan.md Step 4, after the CS-only
MVP (Step 3) is verified.

A new, synchronous request/response channel — distinct from the existing
`control::protocol` socket, which stays exactly as it is (agent→daemon
`Observe` reports, CLI→daemon `ControlCmd`s, now including `Dial` for the
same-process case). See research.md R-003 for why the cross-process case
still needs its own channel rather than an extension of the existing one.

## Who listens, who connects

- **Listener**: each VoWiFi/VoLTE line agent process runs
  `control::line_server` for its own line. (The daemon does **not** need to
  run this listener for CS modems — see the rescoping note above.)
- **Client**: whichever process currently owns the SIP side
  (`sip::SipBridge::owns_sip_side`, the same arbitration spec 024 reuses for
  the registrar) — the one that received the INVITE and is running
  `sip::outbound`'s line selection, when it needs to reach a line hosted in
  a *different* process.

Both roles can be the same process (a single-line, all-in-one-process
deployment): the client MAY take a local fast path and call the line's dial
function directly rather than round-tripping through a loopback socket, but
the wire contract below MUST still hold for the cross-process case, and is
what integration tests exercise (constitution Principle I — real sockets,
not mocks).

## Transport

Unix domain socket, one per line, path derived from the line's existing
identity (mirrors how `control::client::send_cmd`'s socket path is derived
today) — e.g. `/run/gsm-sip-bridge/line-<id>.sock`. Newline-delimited JSON,
same framing as `control::protocol::{read_cmd,write_resp}`.

## Request

```json
{"cmd": "place_call", "destination": "+15551234567"}
```

| Field | Type | Notes |
|---|---|---|
| `destination` | string | Verbatim from the originating INVITE's Request-URI user part (FR-010) — the line-command layer does not validate or transform it further; FR-014 validation already happened before line selection. |

## Response

Exactly one of:

```json
{"outcome": "placed"}
{"outcome": "busy"}
{"outcome": "failed", "reason": "no network registration"}
```

| Outcome | Meaning | Caller action |
|---|---|---|
| `placed` | The dial-out leg was established (ATD accepted / IMS INVITE sent and progressing); media bridging proceeds on the existing per-call path. | Relay progress to the SIP-side caller (FR-012). |
| `busy` | This line was not actually idle when the command arrived (lost a local race, or its state changed between the last `AgentReport` and now). | Treat as if no line had been idle — do **not** try a different line automatically (FR-008/FR-009a); the whole `OutboundCallRequest` is refused. |
| `failed` | The line was idle but the dial attempt itself failed (no registration, modem error, IMS rejection before the call reached the network). | Count as `refused_network_failure` (FR-009a); no automatic retry on a different line. |

## Timeouts

The client MUST apply a request timeout well under a SIP INVITE's own
retransmit/give-up timers (RFC 3261 Timer B, 32 s) — a connect-or-first-byte
timeout in the low single-digit seconds is sufficient, since `PlaceCall` only
needs to confirm the dial *attempt* started, not that it was answered; actual
ringing/answer progress is relayed on the existing SIP dialog, not through
this channel. A timed-out request is treated as `failed`.

## Compatibility

With `[outbound].enabled = false` (or absent), no process starts a
`line_server` listener and no client ever connects — this channel does not
exist on a deployment that has not opted in (FR-017).
