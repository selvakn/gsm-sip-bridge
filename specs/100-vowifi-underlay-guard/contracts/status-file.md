# Contract: `/tmp/pcscf-N.uplink`

A per-line file beside the existing `/tmp/pcscf-N`, `.pool`, `.state` and `.calls` files. The path comes from `ims::pcscf_files::uplink_path(selection_path)`. The supervisor writes it with `write_atomic` on every verdict change and at startup. `vowifi-status` reads it.

## Format (one line of JSON)

```json
{"v":1,"state":"up"}
{"v":1,"state":"no_route","since":1792500000}
{"v":1,"state":"no_response","since":1792500000}
```

| Field | Type | Notes |
|---|---|---|
| `v` | integer | `1`. A reader that sees another version treats the file as unknown. |
| `state` | `up` \| `no_route` \| `no_response` | The line's underlay verdict. Anything other than `up` means the line is held. |
| `since` | unix seconds | When the hold started. Present only while held. |

## `vowifi-status` output

One line under each `Line N` header, after the registration block:

```
  uplink: up
  uplink: no_route (held 3m12s, recovery on hold)
  uplink: no_response (held 45s, recovery on hold)
  uplink: unknown
```

`unknown` is printed when:
- the file is missing (swu line, guard off, supervisor not yet started);
- the file is unreadable or unparseable;
- the version is unknown.

It is never an error, and it never changes `vowifi-status`'s exit code.
