# Research: Caller identity from `tel:` URIs

All primary sources were read directly:

- RFC 3261 §7.3.1, §19.1, §20.10, §20.20, §25.1
- RFC 3325 §9.1
- RFC 3966 §3, §5.1
- RFC 3986 §3.1

There are no open NEEDS CLARIFICATION items.

## R1. How a header value is split and parsed

**Decision**: read every line of the header (`headers_all`). Split each line
on commas outside quoted strings and `<…>`. Parse each value as `name-addr`
(display name, then `<URI>`) or bare `addr-spec`.

**Rationale**:
- RFC 3261 §7.3.1 makes repeated header lines equivalent to one
  comma-joined line.
- RFC 3325 §9.1 allows two PAI values.
- §25.1's `quoted-string` admits `<` and `,`, so a naive `split(',')` or
  `split_once('<')` cuts a display name like `"Doe <Jr>, x"` in half.

`split_route_list` (`ims/agent/call.rs:698`) already implements exactly this
splitter, with tests. It moves rather than being rewritten.

**Alternatives considered**: a SIP parsing crate (`rsip`, `sip-codec`). This
was rejected because the bridge parses SIP by hand everywhere, and one crate
for one header family adds a dependency and a second parsing model
(constitution V).

## R2. Unbracketed `addr-spec`: whose are the `;params`?

**Decision**: it depends on the header.

- **`From`**: everything from the first `;` is a *header* parameter.
- **`P-Asserted-Identity`**: the `;params` belong to the URI.

**Rationale**:
- RFC 3261 §20.10 says: "If no `<` and `>` are present, all parameters
  after the URI are header parameters, not URI parameters". §20.20 applies
  the same rule to `From`/`To`.
- RFC 3325 §9.1's grammar (`PAssertedID-value = name-addr / addr-spec`)
  defines **no** header parameters. Any `;param` on an unbracketed PAI can
  only parse as part of the URI.
- The existing test `header_uri_keeps_parameters_of_an_unbracketed_uri`
  (PAI `sip:ipsmgw.example;lr`) is therefore correct and stays. The bug is
  only on `From`.

**Alternatives considered**: always cut at `;`. This was rejected because it
breaks the PAI grammar and the existing test.

## R3. Reading the number from `sip`/`sips`

**Decision**:
- **userinfo**: present only if the URI has an `@` before its first `;`
  or `?`. Otherwise there is no user part, and no number.
- **user**: userinfo minus `:password`, cut at the first `;`.

**Rationale**:
- `SIP-URI = "sip:" [userinfo] hostport …`. userinfo is optional and always
  ends in `@` (§25.1). Without it the URI names a host, not a user (the
  clarified decision for `sip:gateway.ims.example`).
- `user` may itself contain `;` (it's in `user-unreserved`), which is how a
  telephone-subscriber carries `;npdi;rn=…` inside the user part. So the
  `@` must be located *before* splitting on `;`.
- Searching for `@` only up to the first `;`/`?` is wrong for
  `sip:+91…;npdi@host`. The correct rule is: **take the first `@` in the
  URI**. `@` is not allowed unescaped in `user`, `password` or `host`, so the
  first `@` is always the end of userinfo. A URI parameter or header after
  the host could contain `%40` but never a bare `@`.
- Also, `user=phone` (§19.1.1) marks the user part as a telephone-subscriber.
  This bridge also treats a user starting with `+` as one, because Jio and
  Vodafone send `sip:+91…@ims…` without `user=phone`.

## R4. Reading the number from `tel`

**Decision**:
- **number**: everything before the first `;`.
- **local numbers**: reported as dialled. The `phone-context` is not
  prepended.
- **visual separators**: `-`, `.`, `(` and `)` are removed.

**Rationale**:
- RFC 3966 §3: `global-number = global-number-digits *par` and
  `local-number = local-number-digits *par context *par`. Every parameter
  (`ext`, `isub`, `phone-context`, or any `pname=pvalue`, including Huawei's
  `noa`/`srvattri`) follows a `;`, and the digits never contain one.
- §5.1.1: visual separators "are not used for URI comparison or placing a
  call".
- §5.1.5 does not define `local + context` as an equivalent global number. A
  context may be a domain name, and a digit context can be an area code
  rather than a country code. What the user would have dialled matches what
  GSM CLIP shows.

## R5. Which PAI value supplies the number, and which the report address

**Decision**:
- **caller number**: from the `tel` value if it yields one, otherwise from
  the first `sip`/`sips` value that does.
- **delivery-report address**: the first `sip`/`sips` value, otherwise the
  first value with a URI.

**Rationale**:
- RFC 3325 §9.1 says "If there are two values, one value MUST be a sip or
  sips URI and the other MUST be a tel URI". Both assert the same user. The
  `tel` value is the canonical E.164 form, so preferring it for the number is
  safe.
- The delivery report is a SIP request to a network node. TS 24.341
  §5.3.2.4 NOTE 1 names the IP-SM-GW identified in the delivered message's
  PAI, which is reachable by a SIP URI, not a `tel:` number.
- A tel-only PAI keeps today's behaviour (address the `tel:` URI), as the
  spec requires.

## R6. Percent-decoding and the injection guard

**Decision**: percent-decode the user part or telephone-subscriber. Then
reject it unless every character is ASCII alphanumeric or one of
`+ * # - . _ ~`. Reject an empty result too.

**Rationale**:
- RFC 3261 §19.1.4 and RFC 3966 §3 make `%HH` equivalent to the character.
  So `%2B91…` must equal `+91…`.
- Decoding can also produce `>`, `"`, `,`, `;`, whitespace or CR/LF. The
  number is then embedded raw in `"…" <tel:{caller}>` and `X-GSM-Caller-ID`
  (`vowifi/mod.rs:2029`), and logged.
- The allow-list is RFC 3986 `unreserved` plus the RFC 3966 dial characters
  `*` and `#`. That covers every telephone number and every plausible SIP
  username a carrier puts in an identity (`A2P`, `anonymous`). Invalid
  `%`-sequences (e.g. `%zz`) mean the value is rejected.
- No crate is needed: a 15-line decoder is simpler than a dependency.

## R7. Where the FR-015 warning is emitted

**Decision**:
- `extract_caller` stays pure.
- A new pure function `unresolved_caller_diagnostic(req) -> Option<String>`
  returns the warning text, with control characters escaped by `{:?}`
  formatting of each raw value.
- Two call sites log it, once each:
  1. **INVITE dispatch**, at the point where a request is known to be a new
     call: after the retransmission/re-INVITE early returns, before the busy
     check.
  2. **`handle_message`**, after TPDU decoding, only when the decoded body
     did not supply the sender, just before `received SIP MESSAGE`.

**Rationale**:
- `extract_caller` is called up to three times for one INVITE (dispatch
  decline paths, `inbound::handle_invite`, and its error path). Logging
  inside it would break "exactly one warning".
- RP-ACK/RP-ERROR and Type-0 messages return earlier and carry no sender
  worth reporting.
- A pure function is testable without capturing logs. The repo has no
  log-capture crate, and adding one for two call sites goes against
  constitution V.

## R8. Module placement

**Decision**: a new `gsm-sip-bridge/src/ims/identity.rs` holds the pure
parsing:
- the splitter
- `parse_name_addr`
- `uri_number`
- the percent-decode helper
- `header_identity`
- `header_uri_values`

`session.rs` keeps its existing public functions as thin wrappers, so no
caller changes.

**Rationale**:
- `session.rs` is about 900 lines of request handling. The parser is a
  self-contained grammar with about 30 table-style tests.
- Moving `split_route_list` there gives one home to the two users of the
  RFC 3261 list grammar: Record-Route and identity headers.

**Alternatives considered**: keep everything in `session.rs`. This was
rejected because it buries the grammar tests among dialog tests, and
`agent/call.rs` would have to import a splitter from `session.rs`, which
doesn't own it.
