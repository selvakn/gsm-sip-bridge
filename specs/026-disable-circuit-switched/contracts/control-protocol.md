# Contract: control-socket behaviour with the circuit-switched path disabled

**Feature**: 026-disable-circuit-switched
**Satisfies**: FR-015, FR-019, FR-020

## Problem

`CardPool::run` is the sole consumer of the control-command receiver and the only thing that ever sends a `ControlResp`. With the pool not running, `control::server` still accepts connections and still forwards commands, but nothing replies — so a card command blocks until its socket times out. FR-020 requires a clear rejection "rather than failing obscurely or hanging".

## Wire format

Unchanged. No new `ControlCmd` or `ControlResp` variant is introduced. The existing `ControlResp::Err { error: String }` carries the disabled state.

## Command responses while disabled

| Command | Response | Notes |
|---|---|---|
| `ListSlots` | `Err { error }` naming `[cs].enabled` | **Not** an empty `OkSlots` — FR-019 requires "disabled" to be distinguishable from "enabled but no cards found". An empty `OkSlots` means the pool ran and found nothing; an `Err` means the pool never ran. |
| `CardRestart { slot }` | `Err { error }` naming `[cs].enabled` | FR-020 |
| `SetMode { slot, mode }` | `Err { error }` naming `[cs].enabled` | FR-020 |
| `GetMode { slot }` | `Err { error }` naming `[cs].enabled` | FR-020 |
| `Observe { report }` | **Never reaches this path** | Routed by `control::server::handle_connection` straight to `metrics::ingest::apply_report`, never through the pool mailbox. This is what keeps VoWiFi and VoLTE metrics flowing with the circuit-switched path off (FR-014, FR-015). |

## Error message requirement

The message must name the flag, not merely state that no cards exist. An operator who sees "no such slot" will go looking for a hardware fault; one who sees the flag named knows immediately why and how to reverse it.

Intent (exact wording is an implementation detail):

> circuit-switched path is disabled ([cs].enabled = false) — no cards are managed

## Behaviour when enabled

Entirely unchanged. Every command routes to `CardPool::handle_control_cmd` exactly as it does today, with the same responses. No client needs modification, and no existing control-socket test should need editing.

## Testability

The responder must be exercised through the real control socket with a real client round-trip, asserting a returned `Err` — not asserting the absence of a hang, which a timeout-based test would prove only weakly. A test that passes because it timed out and a test that passes because it got a reply must be distinguishable.
