# Phase 1 Data Model: Automatic VoLTE P-CSCF Priming

This feature adds no new persistent storage and no new on-disk format. It
adds one new transient, in-memory shape used only for the duration of a
single priming attempt.

## P-CSCF cache (existing, unchanged)

- **Location**: `[volte].pcscf_source_path` (default `/tmp/pcscf-0`) —
  already read by `volte-register`/`volte-bridge` today.
- **Format**: unchanged — a single IP address (v4 or v6), optionally
  trailing-whitespace, as `volte::pcscf::probe_epdg_cache` already parses.
- **Lifecycle**: unchanged from today except *who* writes it. Previously
  written only by a persistently-running `[vowifi]` line's steady-state loop
  (`orchestrate.rs`). Now also written, once, by a successful priming
  attempt (`orchestrate_prime::prime_pcscf`) — same file, same format, same
  downstream readers.

## Priming attempt (new, transient, in-memory only)

Not persisted anywhere; exists only for the duration of one
`orchestrate_prime::prime_pcscf` call.

| Field | Type | Notes |
|---|---|---|
| `line` | `vowifi::discovery::LineResolutionEntry` | The single candidate line from `discover`'s output (R1), always index 0 per FR-002b. |
| `shared_charon` | `engines::SharedCharon` | Request-scoped — its own conf/log paths, never the container-wide `SHARED_*` constants (R2). Dropped at the end of the call; nothing outlives it. |
| `pcscd_handle` | `Arc<runner::ChildHandle>` | The private pcscd process for this attempt, added to the synthetic `StartedState` below so teardown kills it. |
| `usim_bridge_handle` | `Option<Arc<runner::ChildHandle>>` | Present unless `line.pcsc_reader` (mirrors the existing persistent-line logic). |
| `captured_pcscf` | `Option<std::net::IpAddr>` | `Some` only after `EstablishOutcome::Established` (R2/R3); absent on timeout or a fatal establish error. |

### Synthetic teardown state

Priming builds a `shutdown::StartedState` scoped to only what this one
attempt actually started (the existing `Default` plus):

- `vowifi_lines`: exactly one `shutdown::StartedVowifiLine` describing the
  priming line's `strongswan` teardown info (conn name, tun iface, `if_id`),
  netns, and veth host name.
- `vowifi_child_handles`: the pcscd handle, the usim-bridge handle (if any),
  and the private charon's handle.

This is passed to the existing, unmodified `shutdown::build_shutdown_plan` /
`shutdown::execute_shutdown_plan` (R3) — no new fields are added to
`StartedState` or `StartedVowifiLine` themselves; a priming attempt reuses
them exactly as a real, persistent line's partial-failure teardown already
does today.

## State transitions (priming attempt)

```text
NotStarted
   │  pcscf_is_available() == false
   ▼
Discovering  ──(discover fails / no line)──▶ Failed (no teardown needed — nothing started)
   │ line found
   ▼
Establishing ──(timeout / FatalProcessDied)──▶ TearingDown ──▶ Failed
   │ Established { pcscf }
   ▼
Captured ──(write pcscf_source_path)──▶ TearingDown ──▶ Succeeded
```

`Failed` and `Succeeded` are both terminal for *this attempt*; the caller
(`orchestrate_volte`'s retry loop, R4) decides whether to try again, on its
existing cadence.
