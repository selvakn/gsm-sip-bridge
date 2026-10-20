# Contract: outcome classification (delta on spec 084)

Inputs: `Disconnect { status, q850 }`, `RejectStatuses`.

| # | Condition (first match wins) | Outcome | Carrier gets |
|---|---|---|---|
| 1 | status in 400..=699 and q850 in {21, 17} | Rejected | 486 |
| 2 | status in RejectStatuses | Rejected | 486 |
| 3 | status in {480, 408} | NoAnswer | 480 |
| 4 | anything else, including no status | Failed | 480 |

Record string: `declined:<reason>:<status>` as in 084, plus `:q850=<n>` on
`pbx_rejected` when a cause was present.

Unchanged: control wire format, To-tag rules, ACK recognition, metrics
label set.
