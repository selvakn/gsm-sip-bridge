# Contract: Automatic VoLTE P-CSCF priming during `supervise` startup

This feature's only externally observable interfaces are (1) `supervise`'s
startup output (stdout/stderr, per FR-007), (2) the file at
`[volte].pcscf_source_path`, and (3) whether/when VoLTE registration
proceeds. There is no new CLI subcommand and no new config field. Each row
below is independently verifiable by starting the bridge with the described
state and observing supervise's own output and the cache file — no
knowledge of internal implementation required.

## Startup behavior, given `[volte].enabled = true`

| Config / on-disk state at startup | Observable outcome |
|---|---|
| `[[volte.line]].pcscf` (or equivalent override) set to a valid address | No priming activity of any kind; VoLTE registration proceeds immediately, identical to today (FR-005, SC-003) |
| No override; `[volte].pcscf_source_path` already contains a valid address | No priming activity; VoLTE registration proceeds immediately using the existing file, identical to today (FR-005, SC-003) |
| No override; cache file missing | Supervise's output shows a priming attempt starting (FR-007) before VoLTE registration is attempted; on success, the cache file now contains a valid address and VoLTE registration proceeds (FR-002, FR-003, FR-004, SC-001) |
| No override; cache file present but empty or unparseable | Treated identically to "cache file missing" (FR-002, User Story 2 scenario 2) |
| No override; cache file previously valid, now missing (simulated redeploy) | Treated identically to "cache file missing" — re-primed automatically (FR-002, SC-002) |

## Priming outcome behavior

| Priming attempt result | Observable outcome |
|---|---|
| Succeeds | Cache file at `[volte].pcscf_source_path` contains the captured address; the transient line's netns/veth/tun/charon/pcscd/usim-bridge processes are gone (verifiable via `ip netns list` / `ps` inside the container showing nothing left over); VoLTE registration proceeds this same startup cycle |
| Fails (no candidate modem/SIM discovered, tunnel fails to establish, or times out) | Supervise's output distinguishes this from a carrier-side VoLTE registration failure (FR-007, SC-004); the attempt is retried on the same cadence already used elsewhere for VoLTE/VoWiFi startup failures (FR-008); nothing is left over from the failed attempt (partial teardown still runs) |

## Non-effects (explicitly unchanged by this feature)

| Component | Behavior |
|---|---|
| A deployment with `[vowifi].enabled = true` (persistent) | Unchanged — the existing FATAL mutual-exclusion check between `[vowifi].enabled` and `[volte].enabled` still applies exactly as before; priming never runs in this configuration because `[volte].enabled` can't also be true here |
| Standalone `volte-register` / `volte-listen` / `volte-call` invocations (outside `supervise`) | Unchanged — no priming; a missing/invalid cache and no override still fails with the existing "run VoWiFi once" guidance (FR-002a) |
| Circuit-switched GSM-to-SIP daemon | Unchanged — never delayed or disrupted by a priming attempt in progress or in retry (FR-010) |
| Persistent `[vowifi]` line startup/steady-state/recovery behavior | Unchanged — the code priming reuses is extracted, not modified; same functions, same order, same existing test coverage |
| `[volte].pcscf_source_path` file format/location, and multi-line VoLTE's shared use of it | Unchanged (FR-003, FR-002b) |
