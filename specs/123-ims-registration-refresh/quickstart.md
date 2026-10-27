# Quickstart: Developing and Verifying Registration Refresh

## Build and gate (every commit)

```bash
make format
make lint     # clippy --workspace --all-targets -D warnings
make test
```

Use a private target directory when other worktrees are active (`CARGO_TARGET_DIR=...`); the shared one in `.cargo/config.toml` can be clobbered.

## Fast loop for the IMS agent

```bash
cargo test -p gsm-sip-bridge ims::            # unit + agent tests
cargo test -p gsm-sip-bridge --test test_ims_registration_refresh
```

Tests that need XFRM (Slice C) must run in the privileged Docker container; there is no sudo in the dev session.

## Simulated carrier

`tests/test_ims_registration_refresh.rs` starts a loopback P-CSCF that can: accept a refresh, answer 423 / 503 / 403 / 500 / 401, delay or drop the answer, retransmit-check on UDP, and answer an OPTIONS probe with the same CSeq number as a pending REGISTER. All identifiers in fixtures are synthetic (`+919000000000` style numbers, `001010000000000` style placeholders for subscriber ids).

## Live verification

Record every run in `specs/123-ims-registration-refresh/live-verification.md` (date, line, carrier, transport, commit, outcome). Never paste real numbers or subscriber ids.

### Slice B (before it is called done)

1. **Schedule**: deploy to the Jio Pi (local `siptest` against the remote Pi, see the project memory notes) and watch the log: the first refresh is ~3000 s after registration for a 3600 s grant.
2. **12 h idle soak** on Jio VoWiFi: every refresh `ok`, one Call-ID, consecutive CSeq, no `terminated/rejected` own-contact events (SC-003).
3. **Mid-call refresh**: place a call that spans the refresh instant (shorten the wait by watching `next_refresh` in `vowifi-status`); call stays up, audio continuous, registration extended (SC-002).
4. Repeat 2 and 3 on the local Vodafone VoWiFi rig (SC-005a). A rejection there must show as `refresh_mode: fallback` and the line must stay registered.
5. **Failover**: with two P-CSCFs, make the first answer 503 with a long Retry-After (test double or an nsenter /32 route on the Pi; no iptables there) and confirm the next registration uses the second.

### Slice C (only after the above is recorded)

6. **Mid-call AKA**: wait for or provoke a network challenge during a call; call stays up, server port unchanged, client port changes, old connection drains.
7. **UDP**: set `use_tcp = false` for a line and repeat 3 and 6.

Switching transports: recreate the container, do not just restart it (stale `/tmp` state reads as a carrier fault).
