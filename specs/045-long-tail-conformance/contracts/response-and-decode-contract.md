# Contract: responses and SMS decoding by condition

| Condition | Behavior (after this feature) | Behavior (before — for contrast) |
|---|---|---|
| Inbound request carries `P-Asserted-Identity` differing from `From` | Caller identity (logs/CDR/SMS sender) uses the asserted identity | Used `From` alone (MT-12 bug) |
| Inbound request carries only `From` | Unchanged | Unchanged |
| VoLTE line's SUBSCRIBE | States its real access-network type | Hardcoded `3GPP-WLAN` regardless of line type (MT-11 bug) |
| `200 OK` to an answered inbound INVITE | States the line's real access-network type | No `P-Access-Network-Info` at all (MT-11 bug) |
| Response to a request whose `Via` sent-by disagrees with its real source | Echoed `Via` gains `received=<real-ip>` | No `received` ever added (MT-13 bug) |
| Response to a request whose `Via` carries bare `rport` | Echoed `Via` states `rport=<real-port>` | Bare `rport` echoed unfilled (MT-13 bug) |
| TPDU whose TP-MTI isn't SMS-DELIVER | Recognized (`DecodedRp::UnsupportedTpdu`), plain `200 OK`, never relayed as text | Walked with the SMS-DELIVER layout regardless — plausible but wrong sender/text possible (SMS-02 bug) |
| TPDU claiming SMS-DELIVER but malformed | RP-ERROR sent | Silently relayed as if `req.body` were plain text (SMS-03 bug) |
| Message-waiting-indication DCS, UCS2 group (`0xE0`-`0xEF`) | Decodes as UCS2 | Decoded as GSM7, garbled (SMS-04 bug) |
| National-language single/locking-shift table in use | Decodes correctly | Falls back to default table / literal space on unknown escapes (SMS-07 bug) |
| VoLTE modem-storage sweep starts | Explicitly sets `AT+CNMI=2,1,0,0,0` | Relies on modem's power-on default (CS-03 gap) |
| `+CMGR` response with a quoted field containing a comma | Fields after it still correctly attributed | Naive `split(',')` — correct today only by field-order coincidence (CS-04 latent bug) |
| Inbound INVITE body's `Content-Type` isn't SDP | Declined | Scanned as SDP text regardless (SDP-05 bug) |
| Inbound INVITE body's `Content-Type` is SDP or absent | Unchanged | Unchanged |
| `100rel` (MT-04) | Never advertised, `Require: 100rel` still declined by MT-03 | Same — confirmed already correct, test added |

Rows describing "before" behavior are drawn from
`docs/plans/mt-conformance-findings.md` (batch 6) and the verified current
source — see `research.md` for exact file/line references.
