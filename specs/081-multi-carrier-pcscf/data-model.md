# Phase 1 Data Model: Multi-Carrier VoLTE P-CSCF Priming

No new persistent data store or schema — this feature adds one new on-disk
file convention and one new transient, in-process grouping concept. Both
described below in terms of the existing entities they extend
(specs/080-volte-pcscf-auto-prime's P-CSCF cache, specs/020-volte-line-netns's
per-line resources).

## Per-line P-CSCF cache (extends specs/080's P-CSCF cache)

| Field | Type | Notes |
|---|---|---|
| Path | `PathBuf` | `<[volte].pcscf_source_path>-<card_id>` — `volte::pcscf::per_line_cache_path(base, card_id)`. Default base `/tmp/pcscf-0` → e.g. `/tmp/pcscf-0-ec20-ABCDEF`. |
| Key | `card_id: String` | The line's stable per-modem identity (`modules::discovery::derive_module_id`, alphanumeric-only, filename-safe). NOT the line's discovery-order `index` — see research.md R1. |
| Contents | `IpAddr` (text) | Same format the existing cache already uses — one IP address, parsed by `probe_epdg_cache`. No new parsing logic. |
| Lifecycle | Written once per successful per-line capture; read on every `resolve_line_pcscf` call for that line; never written by anything other than a priming pass. |

**Relationship to the existing (legacy) cache**: The literal
`[volte].pcscf_source_path` file (unkeyed) is unchanged and continues to
exist as the lowest-priority fallback tier (research.md R2) — it is never
written by this feature's new code, only ever read, preserving every
existing single-line/pinned deployment's behavior.

## Line-to-carrier binding (per manifest line, `VolteLineManifestEntry`)

Not a new struct — the existing `VolteLineManifestEntry` (`card_id`, `pcscf`
override, ...) already carries everything needed to compute a line's
resolution:

```text
resolve_line_pcscf(entry) =
    1. entry.pcscf                                   (explicit override, if set)
 else 2. read per_line_cache_path(base, entry.card_id) (if valid)
 else 3. read base literal path                        (legacy shared file, if valid)
 else None                                             (line skipped, logged)
```

No schema change to the manifest itself.

## Priming pass (new, transient, in-process only — never persisted)

The set of VoLTE lines that lack a usable address (tier 1 and 2 both miss)
at one evaluation point in `start_multiline`, primed together as one
concurrent operation and then fully torn down. Not written to disk, not a
config concept — purely an in-memory grouping for one call into
`orchestrate_prime`.

| Field | Type | Notes |
|---|---|---|
| `lines` | `Vec<(LineResolutionEntry, target_cache_path: PathBuf)>` | One entry per VoLTE line needing capture, each already carrying its own pass-local index (research.md R4) via `resolve_single_line`'s generalized signature, and its own destination cache path (`per_line_cache_path`, keyed by that line's real `card_id` — never the pass-local index). |
| Shared resources | One `SharedCharon`, one shared `pcscd` handle | Mirrors `start_vowifi_subsystem`; scoped to `orchestrate_prime`'s call, torn down via the existing `shutdown::build_shutdown_plan`/`execute_shutdown_plan` machinery (research.md R6) before the function returns, success or failure. |
| Outcome | `Vec<(card_id, Result<(), String>)>` | Per-line success/failure, each logged individually (FR-006) — a pass never returns a single pass/fail verdict, since one line's failure must never obscure another's success (FR-005). |

## Key invariants carried over unchanged from specs/020/080

- A priming pass never runs while `[vowifi].enabled` is persistently true
  (existing `orchestrate::run` mutual-exclusion check, untouched).
- A priming pass always completes (success, failure, or abandonment on real
  shutdown) before any real VoLTE or VoWiFi line's own resources are
  created for the same modem — no new invariant, same ordering 080 already
  guarantees, now holding per line instead of once.
