# Contract: Alert Discord Embed (with identity fields)

The bridge POSTs a Discord webhook embed for each alert. This contract specifies
the additive changes; all existing fields and delivery semantics are unchanged
(FR-009).

## Common additions (both SMS and critical embeds)

- **`footer.text`**: MUST be `"gsm-sip-bridge · {instance}"`, where `{instance}`
  is the configured `[alerts].instance_name`, or the system hostname when unset.
  Always present and non-empty.
- **`fields[]`**: MUST include a `Phone` field:
  ```json
  { "name": "Phone", "value": "<number-or-unknown>", "inline": true }
  ```
  `value` is the resolved card/line number, or the literal string `"unknown"`
  when no number is determinable. Always present.

## SMS embed (`forward_sms`)

Existing: `title` = `"SMS from {sender}"`, `description` = body,
`fields` = `Module`, `Sender`. **After**: append the `Phone` field; footer carries
the instance.

Example (synthetic):
```json
{
  "embeds": [{
    "title": "SMS from +919000000000",
    "description": "hello",
    "timestamp": "2026-08-11T10:00:00Z",
    "color": 3447003,
    "fields": [
      { "name": "Module", "value": "ec20-A1B2C3", "inline": true },
      { "name": "Sender", "value": "+919000000000", "inline": true },
      { "name": "Phone",  "value": "+919000000001", "inline": true }
    ],
    "footer": { "text": "gsm-sip-bridge · bridge-01" }
  }]
}
```

## Critical-event embed (`send_alert`)

Existing: `title` = `"Critical: …"` / `"Recovered: …"`, `description`,
`fields` = `Category`, optional `Module/Line`. **After**: append the `Phone`
field; footer carries the instance.

Example (synthetic, unresolved number):
```json
{
  "embeds": [{
    "title": "Critical: Module/Modem Lifecycle Failure",
    "description": "SIM unreadable after 5 recovery attempts",
    "timestamp": "2026-08-11T10:00:00Z",
    "color": 15158332,
    "fields": [
      { "name": "Category",    "value": "module_lifecycle", "inline": true },
      { "name": "Module/Line", "value": "ec20-A1B2C3",      "inline": true },
      { "name": "Phone",       "value": "unknown",           "inline": true }
    ],
    "footer": { "text": "gsm-sip-bridge · bridge-01" }
  }]
}
```

## Invariants (test targets)

1. Every emitted embed contains exactly one `Phone` field.
2. Every emitted embed's `footer.text` starts with `"gsm-sip-bridge · "` and has
   a non-empty instance suffix.
3. A resolvable number renders verbatim; an unresolvable one renders `"unknown"`.
4. No existing field is removed or renamed; delivery/retry/category behavior is
   byte-for-byte unchanged apart from the two additions.
