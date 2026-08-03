# Contract: `ControlCmd::Dial` — same-process outbound dialing (CS)

**Feature**: 025-outbound-calling
**Supersedes, for the same-process case, the general line-command channel**
described in `line-command.md` — see research.md R-003 (revised 2026-08-03).

Circuit-switched modems always live in the main daemon process's `CardPool`,
regardless of which process ends up owning the SIP side (`owns_sip_side`
decides registration ownership, not where `CardPool` runs). Whenever the
process that received the outbound-triggering INVITE *is* the daemon, no
cross-process hop is needed — this is a direct extension of the existing
`ControlCmd`/`ModuleCmd` mechanism already used for `SetMode`/`Reboot`
(`gsm-sip-bridge/src/modules/mod.rs`).

## Request

```rust
// control/protocol.rs
enum ControlCmd {
    // ...existing variants...
    Dial { slot: u32, destination: String },
}
```

`destination` is verbatim from the originating INVITE's Request-URI user
part (FR-010); FR-014 validation already happened before a slot was chosen.

## Internal dispatch (daemon-side, not wire-visible)

```rust
// modules/mod.rs, alongside SetMode/Reboot
enum ModuleCmd {
    SetMode(NetworkMode, oneshot::Sender<Result<NetworkMode, String>>),
    Reboot,
    Dial(String, oneshot::Sender<Result<(), String>>),
}
```

`CardPool::handle_control_cmd`'s `Dial` arm mirrors `SetMode`
(`modules/mod.rs:1196-1261`) exactly:

1. Look up `SlotState` by `slot`; if `has_active_call` or the slot isn't
   `Ready`, reply `ControlResp::err("line busy or not ready")` immediately —
   this is the local check that plays the same role as `line-command.md`'s
   `Busy` outcome.
2. Clone `cmd_tx`, create a fresh `oneshot::channel()`.
3. Send `ModuleCmd::Dial(destination, resp_tx)` into the modem's
   `crossbeam_channel`.
4. `tokio::spawn` a task that awaits `resp_rx` under a bounded
   `tokio::time::timeout` (matching `SetMode`'s existing 30s pattern, though
   dial should use a **shorter** timeout — see Timeouts below) and forwards
   the result to the control command's own `reply` oneshot.

In the modem's own blocking loop (`run_module_loop`'s `cmd_rx` match,
`modules/mod.rs:1588` area):

```rust
ModuleCmd::Dial(number, resp_tx) => {
    if card.state != CardState::Idle {
        let _ = resp_tx.send(Err("line busy".to_string()));
    } else {
        let result = at.dial(&number).map_err(|e| e.to_string());
        if result.is_ok() {
            card.state = CardState::Answering; // dial in progress
        }
        let _ = resp_tx.send(result);
    }
}
```

## Response

```rust
enum ControlResp {
    // ...existing variants...
    Ok, // dial accepted, in progress
    Err { error: String }, // busy, or ATD failed
}
```

Reuses the existing `ControlResp::Ok`/`Err` shape — no new response variant
needed (unlike the cross-process `PlaceCallOutcome`, which distinguishes
`Busy` from `Failed`; here the `error` string carries that distinction,
consistent with how every other `ControlCmd` failure is already reported).

## Timeouts

Bounded by the same reasoning as `line-command.md`: well under a SIP
INVITE's Timer B (32s) — a request timeout in the low single-digit seconds
is enough, since this only confirms `ATD` was accepted, not that the call
was answered (call progress is relayed separately, over the SIP dialog, per
`sip-dialout.md`).

## Compatibility

With `[outbound].enabled = false`, `ControlCmd::Dial` is never sent — no
change to `CardPool`'s existing behavior for `SetMode`/`Reboot`/etc.
