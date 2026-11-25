# Quickstart: Verifying That the Supervisor Paces Itself While a Card-Reader SIM Is Out of Reach

Never paste a real IMSI into this repo. Scratch goes in `./tmp`. Times shown to the maintainer are in IST.

## 1. Automated tests (any host)

```bash
CARGO_TARGET_DIR=$PWD/target-161 make format && make lint && make test
```

The deterministic scenarios use injected clocks and `MockCommandRunner`, so each runs in well under a second (SC-007):

- `supervise/card_outage.rs` tests: transitions, the check cadence, both backoffs, reset on every path out.
- `supervise/sim_recovery.rs` tests: classification order, and the marker pinned to the agent's constant.
- `supervise/line_supervisor.rs` tests (search for `card_`): the card gate at every SIM-needing site, composition with the uplink hold, modem lines untouched, `needs_the_sim` pinned to `recovery_renegotiates`.
- `supervise/orchestrate.rs` tests: `card_check`'s exit-status mapping, and the agent wait returning early on `Present` or shutdown.
- `metrics/ingest.rs` tests: the tunnel-failure text with and without `card_hold`.

## 2. Live (local card-reader rig, `tmp/pr228-reader`)

Build an image from `git archive HEAD` (memory: docker build context) and point `tmp/pr228-reader/docker-compose.yaml` at it. `docker compose down && docker compose up -d` (recreate, don't restart). Wait for `outcome="registered"` in `/tmp/ims-agent-0.out`.

The recipe is in the memory note `live-card-outage-test-recipe`.

### 2a. Agent loop (US1, SC-001, SC-003)

1. Unbind the reader:
   ```bash
   docker run --rm --privileged --entrypoint sh -v /sys:/sys gsm-sip-bridge:<tag> -c 'echo 5-1.1.3.1 > /sys/bus/usb/drivers/usb/unbind'
   ```
2. Restart the agent: `docker exec <c> pkill -f 'vowifi-ims-agent --line 0'` (run it inside the container, not with a pattern that matches your own shell).
3. Over 10 minutes, expect one `SIM card out of reach; agent restarts and tunnel attempts are slowed` line and at most 15 `exited (SIM card out of reach); restarting in Ns` lines, with N growing 5, 10, 20… up to 120.
4. Rebind (`…/usb/drivers/usb/bind`). Expect `SIM card reachable again after Ns; resuming` and `outcome="registered"` within 30 s.

### 2b. Tunnel loop (US2, SC-002, FR-009)

1. Set `[vowifi].gm_options_keepalive = true` temporarily (it is off by default). Recreate, and wait for registration.
2. Unbind the reader, then route the P-CSCF to the silent veth peer, as in the recipe, to force the PcscfPoolExhausted path.
3. Expect no `IKE_SA deleted` for the PcscfPoolExhausted rebuild while the card is out (it is held), and no `SCardConnect: No smart card inserted` burst. Count charon's `initiating IKE_SA` lines over 10 minutes: at most 6.
4. Remove the route and rebind. Expect `reachable again`, the held recovery or a fresh IKE_SA, and `outcome="registered"` within 30 s.
5. Restore `gm_options_keepalive`, and recreate.

### 2c. Alert text (US3, FR-014)

During 2b, after the tunnel-failure threshold has passed, the web UI timeline's tunnel-failure entry for the line ends with `; SIM card out of reach, tunnel attempts slowed until it is back`. With the card in, a tunnel failure's text is as before.

### 2d. No regression

With the card in: registration, one in-place refresh cycle, and no `card` lines in the supervisor log.

Record what was and was not exercised under Deviations in `tasks.md`.
