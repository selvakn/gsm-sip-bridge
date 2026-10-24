# Contract: Discord embed for a retried text

Fresh texts: payload byte-identical to today (`forward_sms`, spec 006).

Retried texts differ only by:
- `title`: `[Late] SMS from <sender>`
- one extra field: `{ "name": "Originally received", "value": "<received_at>", "inline": false }`
- `timestamp` stays the original received time (as today).

Posted one embed per text, oldest `id` first (clarification Q2).
