# Contract: the initiate command and its log lines

## Command

Exactly one builder produces it; no other code may contain the string `--initiate` outside tests.

```text
timeout -k 2 25 env STRONGSWAN_CONF=<shared conf> swanctl --initiate --child <conn> --timeout 20
```

Properties a test asserts:
- `--timeout <n>` present with `0 < n < 30`.
- Wrapped in `timeout -k <k> <t>` with `n < t` and `t + k < 30`.
- `--child` names this line's connection only.
- Spawned through the runner's tracked `spawn`, never `spawn_detached`.

## Log lines (stdout, no subscriber data)

| Event | Format |
| --- | --- |
| started | `[supervise] line <idx>: <conn> initiate started` |
| skipped | `[supervise] line <idx>: <conn> initiate skipped, previous client still running (<n>s)` |
| client ended | `[supervise] line <idx>: <conn> initiate client exited after <n>s` |
| left to charon | `[supervise] line <idx>: <conn> IKE_SA #<id> still negotiating; leaving it to charon` (existing text) |
| unresponsive | `[supervise] line <idx>: swanctl --list-sas timed out (<k>/2)` |
| wedge recovery | `[supervise] line <idx>: charon control interface unresponsive; restarting the shared daemon` |
