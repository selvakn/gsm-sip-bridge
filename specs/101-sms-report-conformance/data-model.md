# Data Model: SMS Delivery Report Conformance

No persistent data. In-memory shapes only.

## Delivery report (wire message)

| Field | Source | Change |
|---|---|---|
| Request-URI, `To` | inbound `P-Asserted-Identity` (fallback `From`) | none |
| `From`, `P-Preferred-Identity` | registered public identity | none |
| `Route` (first) | P-CSCF address + protected server port, `lr` | **new**, only when the setting is on |
| `Route` (rest) | registrar `Service-Route` entries, in order | none |
| `In-Reply-To` | inbound message `Call-ID` | **new** |
| `Call-ID`, `CSeq`, `Via`, `Max-Forwards`, security headers, `Content-Type`, body | as today | none |

## In-flight report set

- Ordered, bounded (32) collection of Call-IDs of delivery reports sent and not yet answered finally.
- Insert on a successful send. Remove on a final response. Evict the oldest when full.
- Owned by the dispatch loop (one per line/agent); not shared across threads.

## Setting

`sms_report_pcscf_route`: bool, default `true`. `[vowifi]` value; `[volte]` inherits it when its own key is absent.
