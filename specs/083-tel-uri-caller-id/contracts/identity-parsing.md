# Contract: identity header parsing

These are the wire inputs the bridge accepts and what it must derive from
them. Every row becomes a test, all with synthetic numbers.

## C1. `uri_number(uri)`

| # | URI | Result | Rule |
|---|---|---|---|
| 1 | `tel:+919000000000;noa=international;srvattri=national` | `+919000000000` | FR-006, issue #104 |
| 2 | `tel:+919000000000` | `+919000000000` | FR-006 |
| 3 | `TEL:+919000000000` | `+919000000000` | FR-004 |
| 4 | `tel:+91-900-000-0000` | `+919000000000` | FR-007, RFC 3966 §5.1.1 |
| 5 | `tel:+91(900)000.0000` | `+919000000000` | FR-007 |
| 6 | `tel:9000000000;phone-context=+91` | `9000000000` | FR-006, R4 |
| 7 | `tel:+919000000000;ext=12` | `+919000000000` | FR-006 |
| 8 | `sip:+919000000000@ims.example` | `+919000000000` | FR-005 |
| 9 | `Sip:+919000000000@ims.example;user=phone` | `+919000000000` | FR-004, FR-005 |
| 10 | `sips:+919000000000@ims.example` | `+919000000000` | FR-004 |
| 11 | `sip:+919000000000;npdi;rn=+919000000099@ims.example;user=phone` | `+919000000000` | FR-005, R3 |
| 12 | `sip:+91-900-000-0000@ims.example;user=phone` | `+919000000000` | FR-007 |
| 13 | `sip:%2B919000000000@ims.example` | `+919000000000` | FR-007 |
| 14 | `sip:alice:secret@ims.example` | `alice` | FR-005 |
| 15 | `sip:A2P@203.0.113.7;transport=udp` | `A2P` | FR-005 |
| 16 | `sip:gateway.ims.example` | none | FR-005, clarification |
| 17 | `sip:gateway.ims.example;transport=udp` | none | FR-005 |
| 18 | `sip:a%3E%0D%0Ab@ims.example` | none | FR-008 |
| 19 | `sip:a%22b@ims.example` | none | FR-008 |
| 20 | `sip:%zz@ims.example` | none | FR-008 (invalid escape) |
| 21 | `tel:` | none | FR-008 (empty) |
| 22 | `mailto:a@example.com` | none | FR-004 |
| 23 | `urn:service:sos` | none | FR-004 |

## C2. `parse_name_addr(value, params)`

| # | Value | Params | display | uri |
|---|---|---|---|---|
| 1 | `<tel:+919000000000>` | None | none | `tel:+919000000000` |
| 2 | `"Asserted Name" <tel:+919000000000;cpc=ordinary>` | None | `Asserted Name` | `tel:+919000000000;cpc=ordinary` |
| 3 | `Asserted Name <sip:+919000000000@ims.example>` | None | `Asserted Name` | `sip:+919000000000@ims.example` |
| 4 | `"Doe <Jr>, sip:x" <tel:+919000000000>` | None | `Doe <Jr>, sip:x` | `tel:+919000000000` |
| 5 | `"Q \"x\"" <tel:+919000000000>` | None | `Q "x"` | `tel:+919000000000` |
| 6 | `sip:ipsmgw.example;lr` | None | none | `sip:ipsmgw.example;lr` |
| 7 | `sip:ipsmgw.example;tag=abc` | Allowed | none | `sip:ipsmgw.example` |
| 8 | `<sip:gw.example>;tag=abc` | Allowed | none | `sip:gw.example` |
| 9 | `"Anonymous"` | None | – | value is none |
| 10 | `"" <tel:+919000000000>` (empty quotes) | None | none | `tel:+919000000000` |
| 11 | `"unterminated <tel:+919000000000>` | None | – | value is none |

## C3. Caller selection (`extract_caller`, `extract_caller_name`)

| # | Headers | caller | name |
|---|---|---|---|
| 1 | `From: <tel:+919000000000;noa=international;srvattri=national>;tag=example` / `P-Asserted-Identity: <tel:+919000000000>` | `+919000000000` | none |
| 2 | `From: "Other Name" <sip:+919000000001@ims.example>;tag=a` / `P-Asserted-Identity: "Asserted Name" <tel:+919000000000>` | `+919000000000` | `Asserted Name` |
| 3 | PAI line 1 `<tel:+919000000000>`, PAI line 2 `"Asserted Name" <sip:+919000000000@ims.example>`, `From: "Other Name" <sip:+919000000001@…>` | `+919000000000` | `Asserted Name` |
| 4 | `P-Asserted-Identity: <tel:+919000000000>, "Asserted Name" <sip:+919000000000@ims.example>` | `+919000000000` | `Asserted Name` |
| 5 | `P-Asserted-Identity: <sip:gw.ims.example>`, `From: "Other Name" <tel:+919000000001>` | `+919000000001` | `Other Name` |
| 6 | `From: <sip:gateway.ims.example>;tag=a`, no PAI | `unknown` | none |
| 7 | `From: "sip:+919000000099" <tel:+919000000001>;tag=a` | `+919000000001` | `sip:+919000000099` |
| 8 | `f: <tel:+919000000001>;tag=a` (compact) | `+919000000001` | none |

## C4. Delivery-report address (`header_uri`)

| # | Headers | PAI result | From result |
|---|---|---|---|
| 1 | `P-Asserted-Identity: <tel:+919000000000>, <sip:ipsmgw.example;transport=udp>` | `sip:ipsmgw.example;transport=udp` | – |
| 2 | PAI line 1 `<tel:+919000000000>`, PAI line 2 `<sip:ipsmgw.example>` | `sip:ipsmgw.example` | – |
| 3 | `P-Asserted-Identity: <tel:+919000000000>` | `tel:+919000000000` | – |
| 4 | `From: sip:ipsmgw.example;tag=abc` | – | `sip:ipsmgw.example` |
| 5 | `P-Asserted-Identity: sip:ipsmgw.example;lr` | `sip:ipsmgw.example;lr` | – |
| 6 | `P-Asserted-Identity: <sip:A2P@203.0.113.7;transport=udp>` / `From: <sip:gateway.ims.example>;tag=abc` | `sip:A2P@203.0.113.7;transport=udp` | `sip:gateway.ims.example` |
| 7 | `P-Asserted-Identity: "Anonymous"` | none | – |

Row 6 is the live Jio form and must be byte-identical to today (FR-014).

## C5. Unresolved-caller diagnostic (FR-015)

| # | Headers | Result |
|---|---|---|
| 1 | any row of C3 with a resolved caller | none |
| 2 | `From: <sip:gateway.ims.example>;tag=a`, no PAI | some, containing `P-Asserted-Identity` reported as absent and the raw `From` value |
| 3 | `P-Asserted-Identity: <sip:a%0D%0Ab@x>`, `From: garbage` | some, both raw values present, no literal CR or LF in the text |
