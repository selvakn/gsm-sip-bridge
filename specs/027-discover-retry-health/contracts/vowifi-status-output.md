# Contract: `gsm-sip-bridge vowifi-status` output (extended)

Extends the existing contract (`print_status`, `vowifi/mod.rs`) — every current line of output for a
resolved, running line is unchanged. This adds a new section for configured lines that never
resolved.

## Current behavior (unchanged)

For every entry in the resolution file's `lines`, prints:

```
Line <index> (card <card_id>):
  VoWiFi registration (Agent A):
    state: <state>
    ...
  Recent calls (Agent B):
    ...
```

## New behavior

After the existing per-resolved-line output, for every entry in the resolution file's `failed`
list whose `reason` traces back to an explicitly configured override (a `modem_port`/`modem_serial`
pin, or a `pcsc_reader` entry — i.e. excludes `max_lines_exceeded`, which is about an
*auto-discovered*, unpinned modem losing out on a scarce slot, not a configured line failing to
start):

```
Configured line <identifier> (from config.toml): NOT RUNNING
  reason: <not_found | sim_absent | sim_locked | sim_unreadable: <detail> | no_at_port>
```

Where `<identifier>` is the `FailedLine.card_id` value — the configured `modem_port` path, the
`modem_serial`, or the synthetic `pcscN` id, per `data-model.md`.

## Exit code

`vowifi-status`'s existing exit code contract (`SUCCESS` if any queried line answered, `FAILURE`
otherwise) is unchanged — the presence of a `NOT RUNNING` configured line does not, by itself, flip
the command's own exit code; that line was never running to query in the first place. (Overall
system health for this condition is `healthcheck`'s job — see `healthcheck-contract.md` — not this
command's exit code.)

## Backward compatibility

A deployment with no configured overrides that ever fail to resolve (today's common case) sees byte-identical output to before this feature — the new section only appears when `failed` is non-empty for a configured-line reason.
