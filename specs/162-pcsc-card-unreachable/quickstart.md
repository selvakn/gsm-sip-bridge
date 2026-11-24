# Quickstart: Verifying That a Card-Reader Line Rides Out an Unreachable SIM

Never paste a real IMSI into this repo. Scripts read it from the gitignored rig config, or from the card. Scratch goes in `./tmp`.

## 1. Automated tests (any host): US1–US3, FR-001–FR-022

```bash
make format && make lint && make test
```

The deterministic scenarios live in `gsm-sip-bridge/src/ims/agent/registration_tests.rs` (search for `card_reader_`). They use the scripted SIM (`registration_auth::scripted::card_after`) and `CardTiming::fast()`, so each finishes in well under a second (SC-007). Error mapping and classifier equivalence are in `modules/pcsc_card.rs` tests.

## 2. Hardware-gated tests (reader host): FR-001 on real pcsc-lite

```bash
CARGO_TARGET_DIR=$PWD/target-162 cargo test -p gsm-sip-bridge --lib pcsc_card::hardware -- --ignored --test-threads=1
```

The new tests:

- `a_reset_by_another_program_is_card_unreachable`: another holder resets the card (a bare `Card` drop, as in #234's control test), and our next transmit is `CardUnreachable`.
- `no_reader_with_this_imsi_is_card_unreachable`: `PcscTransport::connect("001010000000001")`.

All of #234's hardware tests must still pass.

## 3. Live: no regression (local card-reader rig)

Build an image from `git archive HEAD` (see the memory note on the docker build context). Run it on the local card-reader VoWiFi rig (host pcscd, OMNIKEY, Vodafone), as in `specs/161-pcsc-leave-card/quickstart.md` step 4.

Expect:

- `registration through P-CSCF … outcome="registered"`;
- refreshes `registration refreshed in place` over at least one refresh cycle;
- no `SIM card unreachable` while the card is in place.

## 4. Live: a reader outage does not exit the agent (US3, SC-004), maintainer-assisted

Needs a full registration to run while the card is unreachable. There is no sudo in-session, so the card must be taken away by the maintainer at the host, or from a privileged container.

1. Make the card unreachable, by any one of:
   - pulling the card;
   - unbinding the reader's USB device (`echo <busid> > /sys/bus/usb/drivers/usb/unbind`);
   - stopping the host pcscd.
2. Bring on a full registration. Vodafone accepts in-place refreshes unchallenged, and they do not touch the card, so either wait for the registration to lapse, or cut the Gm path (for example the P-CSCF black-hole recipe from the memory notes) so that the Gm-liveness escalation forces one.
3. Every backoff step should log `the SIM is out of reach; retrying the registration without counting toward the restart budget`. The agent should keep running past 6 failures, `vowifi-status` should show the line unregistered with `last_failure: … SIM card unreachable …`, and the registration-loss alert should fire.
4. Put the card back. Expect `outcome="registered"` within one backoff step (at most 120 s, SC-005).

Record what was and was not exercised in `tasks.md`. The challenged-refresh paths (US1, US2) are covered by step 1 only, because no available carrier challenges refreshes.
