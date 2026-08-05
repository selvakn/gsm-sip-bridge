# Phase 0 Research: Disable Circuit-Switched Handling

No `NEEDS CLARIFICATION` markers survived `/speckit-clarify` — all four open questions were answered and recorded in the spec's Clarifications section. This document records what reading the existing code established, since several of the spec's requirements turn out to be cheaper or more expensive than they look.

## R1: Where the circuit-switched path can be cut

**Decision**: Gate at `commands/daemon.rs`, around `CardPool::new` + `card_pool.run(...)`.

**Rationale**: `daemon.rs:107` constructs the pool and `daemon.rs:109` spawns it. Everything circuit-switched — startup scan, periodic rescan, AT traffic, call bridging, scheduled restarts, and every CS metric touch — is reachable only from inside `CardPool::run`. One conditional there satisfies FR-005 through FR-009 at once, and leaves `CardPool` itself entirely unaware the flag exists.

**Alternatives considered**:
- *A flag threaded into `CardPool` internals*: rejected. It would need checks at `run`'s scan (`modules/mod.rs:341`), `rescan_new_modules` (`:801`), and `advance_scheduler` (`:925`), and would still register the CS metrics, defeating FR-021a.
- *Not spawning the daemon at all from `supervise`*: rejected during clarification. The daemon hosts the metrics endpoint, control socket, and message store that VoWiFi and VoLTE depend on.

## R2: The default-value trap

**Decision**: `RawCs` gets a hand-written `impl Default` returning `enabled: true`.

**Rationale**: The `section!` macro (`config/raw.rs:38`) applies `#[serde(deny_unknown_fields, default)]` to every section. `default` means an absent key or absent section falls back to `Default::default()`. Every other section that carries an `enabled` flag — `RawSms`, `RawVowifi`, `RawVolte`, `RawSipServer`, `RawOutbound` — is opt-*in* and correctly uses `#[derive(Default)]` for `false`. `[cs]` is the first opt-*out* flag in the file, so copying the neighbouring pattern produces exactly the regression User Story 2 forbids: every existing deployment would silently lose circuit switching on upgrade.

**Alternatives considered**:
- *`Option<bool>` with `unwrap_or(true)` at build time*: works, but pushes the default away from the declaration and makes `RawCs::KEYS` no less correct while being harder to read. Rejected on Principle V.
- *Naming the key `disabled` so `false` is the right default*: rejected — a negative boolean in config is a readability trap, and it would break the `[section].enabled` convention every other section follows.

## R3: The control channel is owned by the pool

**Decision**: Add a small responder task in a new `control/disabled.rs` that drains `control_rx` and replies to card-targeting commands with an error naming the flag.

**Rationale**: `daemon.rs:110` moves `control_rx` into `card_pool.run(...)`, and `CardPool::handle_control_cmd` (`modules/mod.rs:1303`) is the only thing that ever replies. If the pool does not run, `control::server` still accepts connections and still forwards commands, but nothing sends a `ControlResp` — so `card list` blocks until its socket times out. FR-019 and FR-020 both require a clear answer instead.

Only four variants need handling: `CardRestart`, `SetMode`, `GetMode`, `ListSlots`. The fifth, `Observe`, never reaches the pool mailbox — `control::protocol.rs:19-23` documents that `control::server::handle_connection` routes it straight to `metrics::ingest::apply_report`. That is what keeps VoWiFi and VoLTE metrics flowing with the path off (FR-014, FR-015).

`ControlResp::Err { error: String }` already exists, so `ListSlots` can report the disabled state without a new protocol variant. This satisfies FR-019's "distinguishable from enabled-but-no-cards-found": an empty `OkSlots` means the pool ran and found nothing; an `Err` naming the flag means the pool never ran.

**Alternatives considered**:
- *Run `CardPool` with discovery stubbed to return no modems*: rejected. It would still start the scheduler, still call `sip_bridge.register()`, and still touch `MODULES_ACTIVE`/`MODULES_FAILED`, breaking FR-009a and FR-021a.
- *A new `ControlResp::Disabled` variant*: rejected as unnecessary protocol churn; `Err` with a message naming `[cs].enabled` carries the same information and needs no client change.

## R4: Metrics absence is free, presence is not

**Decision**: Do nothing to suppress CS series. Add one `CS_ENABLED` gauge, set unconditionally in `daemon.rs`.

**Rationale**: Every CS metric is a `once_cell::sync::Lazy` whose initialiser calls `register_gauge!`/`register_counter_vec!` — registration happens on **first dereference**, not at startup. Grepping every touch site for `MODULES_ACTIVE`, `MODULES_FAILED`, `MODULE_INIT_TOTAL`, `MODULE_RETRIES_TOTAL`, and `SCHEDULED_RESTART_TOTAL` finds them exclusively inside `modules/mod.rs`. With `CardPool` not running, none is ever dereferenced, so none is registered and none appears in a scrape. FR-021a holds by construction.

This is a property worth pinning with a test rather than trusting, because it silently breaks the moment any non-CS code path touches one of those statics.

The status gauge (FR-021b) is genuinely new and must be set in **both** states — its whole purpose is distinguishing "deliberately disabled" from "process down", which requires it to be present when the path is enabled too.

**Alternatives considered**:
- *Explicitly unregistering CS collectors when disabled*: rejected as unnecessary work against a property that already holds.
- *Zero-valued series*: rejected during clarification — it is the false-alert case FR-021/SC-005 exist to prevent.

## R5: Suppressing the daemon's telephone-facing side

**Decision**: Extend `owns_sip_side` in `sip/mod.rs:107` with `&& config.cs.enabled`, and extend the existing skip log to name the reason.

**Rationale**: FR-009a explicitly requires reusing the existing suppression. `SipBridge::register` (`sip/mod.rs:150`) already early-returns when `owns_sip_side` is false, *before* the `sip_server.enabled` branch and before `register_trunk` is consulted. So one added term suppresses both the upstream trunk registration and the host-side registrar — exactly FR-009a's two clauses — with no second code path.

The existing log line at `sip/mod.rs:152` already explains the VoWiFi/VoLTE case; it needs to distinguish the new reason to satisfy FR-009b.

**Alternatives considered**:
- *A separate early return keyed on the flag*: rejected. Two mechanisms deciding the same thing is what FR-009a was written to prevent.

## R6: Which subsystem reserves modems

**Decision**: Add a `cs_enabled: bool` parameter to `vowifi::discovery::RoleAssignment::from_probed`. VoLTE needs no change.

**Rationale**: The reservation rule is a single branch at `vowifi/discovery.rs:41` — `is_overridden_to_vowifi(modem, overrides) || !modem.has_audio_capability` sends a modem to VoWiFi, everything else to circuit-switched. With the path off, that fallback must go to VoWiFi instead. `resolve_volte_lines` (`volte/discovery.rs:106`) applies no audio-capability filter at all, so VoLTE never reserved anything for circuit-switched use and is unaffected by FR-010a.

The production caller is `commands/discover.rs:65`; the only other reference constructs a `RoleAssignment` literal inside a VoLTE test.

FR-010b falls out for free: `resolve_lines` applies the readiness filter and `max_lines` bound after the partition, so freeing more candidates cannot bypass either.

**Alternatives considered**:
- *Reading the flag inside `from_probed` from a global*: rejected — the function is pure and heavily unit-tested; an explicit parameter keeps it that way.

## R7: Documentation is load-bearing, not optional

**Decision**: Update `docs/configuration.md` and `config.toml.example` in the same commit as the config change.

**Rationale**: `tests/test_config_docs.rs` asserts that every key in `section_key_lists()` appears as a `| \`key\`` table row in the reference, and cross-checks the example. Adding `[cs]` without documenting it fails `make test`. Sequencing the docs with the config change rather than at the end of the feature keeps every intermediate commit green (Principle II).
