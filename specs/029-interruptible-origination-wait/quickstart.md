# Quickstart: exercising the interruptible origination wait

## Before every commit (non-negotiable, per `CLAUDE.md`)

```bash
make format
make lint     # clippy --workspace --all-targets -D warnings, + deny/shellcheck/unsafe
make test
```

`make lint` covers all test targets — a warning in an integration test fails the
build like one in production code.

## The tests that matter for this feature

```bash
# R2 — does origination reliably receive its own carrier responses?
cargo test --test test_outbound_origination_race

# US1 — caller hangs up mid-ring; is a CANCEL sent?
cargo test --test test_outbound_abandon

# US2/US3 — busy refusal during an attempt, and outcome recording
cargo test --test test_vowifi_call_metrics

# Regression: the slow-carrier case that took five hardware passes to get right
cargo test --test test_outbound_abandon slow_carrier
```

## Reading the state machine while it runs

The whole feature is observable from Agent A's logs. A healthy abandoned
attempt looks like:

```text
INFO outbound: sending INVITE to carrier            call_id=out-42 destination=+919000000000
INFO provisional response                          status=180 reason=Ringing
INFO outbound: caller abandoned the attempt        call_id=out-42 reason=caller_hangup
INFO outbound: sent CANCEL for an abandoned INVITE call_id=out-42
```

The middle line is the one that does not exist today — its absence is the whole
bug.

If you instead see this, the attempt was not interrupted and the old path ran:

```text
WARN outbound: could not place carrier call  reason=carrier_timeout: no final response...
```

## Manual verification against real hardware

Needs a real SIM with VoWiFi or VoLTE. The sandbox cannot do this — see the
`sandbox-blocks-root-network-testing` note; use the privileged container.

1. `vowifi-status` — confirm the line is registered and `can_answer`.
2. From a registered SIP phone, dial an outside number that will not be
   answered (a second phone left ringing).
3. Wait for ringback, then hang up the originating phone **while it is still
   ringing**.
4. Expected: the called phone stops ringing within a couple of seconds.
   Before this feature it kept ringing until the carrier gave up.
5. `vowifi-status` — the line reads idle immediately, and the attempt appears in
   recent calls as `caller_abandoned`.

While step 3's attempt is in flight, calling the line's own number from a third
phone should give a busy tone promptly, rather than silence.

## Use synthetic numbers only

`+919000000000`, `+919000000001`, … Never a real MSISDN in tests, fixtures,
logs committed to the tree, or commit messages — this is a public repository.
