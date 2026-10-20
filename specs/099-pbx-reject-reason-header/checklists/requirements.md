# Requirements checklist

- [x] Every FR has a task (see tasks.md)
- [x] Cause beats status; precedence tested
- [x] 403 not a default; opt-in via `[sip].reject_statuses`
- [x] Parsing is tolerant; no failure on malformed headers
- [x] Wire format unchanged
- [x] No real subscriber data in fixtures (synthetic numbers only)
