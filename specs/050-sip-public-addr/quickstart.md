# Quickstart: verifying `[sip].public_addr`

## Automated tests

- `gsm-sip-bridge/src/config/mod.rs`'s test module (`try_parse`-based):
  `public_addr` absent → `SipConfig.public_addr == None`, behavior unchanged;
  an IP literal → parsed with no resolution; a syntactically malformed value
  → `BridgeError::Config` naming `sip.public_addr`; a non-resolving hostname
  → the same class of error (see `research.md` Decision 5 for the caveat on
  sandbox DNS reliability).
- `pjsua-safe/tests/*.rs`, run in both modes:
  - `cargo test -p pjsua-safe` (stub mode — fast, no real PJSIP)
  - `cargo test -p pjsua-safe --features pjsip-linked` (real PJSIP linked —
    the mode that actually exercises `tp_cfg.public_addr`/
    `acc_cfg.rtp_cfg.public_addr`)
  - New/extended case: create an `Endpoint` with `public_addr` set, create
    an `Account` via `register` and via `local`, confirm the configured
    address appears in the account's advertised media config; call
    `set_identity`, confirm it's still there afterward (the FR-004
    regression this feature exists to close).
- `make test` (workspace-wide) must stay green — this is the standard
  pre-commit gate (`CLAUDE.md`), not a feature-specific step.

## Manual / live verification (mirrors GitHub issue #77's own repro)

1. Set `[sip].public_addr` in the bridge's config to its Tailscale (or
   equivalent routable) address.
2. Start the bridge under Docker with `network_mode: host`, reachable to a
   test device only over that routed network (not the bridge's local LAN
   segment) — a second machine on Tailscale, or a phone on the Tailscale
   app, works.
3. Register a SIP softphone on that remote device to the bridge.
4. Place a call in each direction (softphone → bridge destination, and
   bridge-routed inbound → softphone) and confirm two-way audio — this is
   the exact scenario that was completely silent before this feature
   (issue #77).
5. For User Story 3 specifically: trigger an inbound call while the bridge
   is running in `[sip_server]` mode (so `Account::set_identity` runs for
   the caller-ID rewrite) and confirm audio still works — this is the path
   that would still be silently broken by a fix that only touched
   `register`/`local`.
6. Regression check: with the same config, place/receive a call from a
   softphone on the bridge's own host/LAN segment and confirm it still
   works exactly as before (User Story 1, Acceptance Scenario 3).
7. Negative check: set `[sip].public_addr` to an obviously invalid value
   (e.g. `not-a-real-host.invalid`) and confirm the bridge refuses to start
   with a clear error rather than starting up with silently broken audio
   (User Story 2, Acceptance Scenario 2).
