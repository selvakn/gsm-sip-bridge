# Quickstart: verifying a bounded modem port open

## 1. Tests

```bash
make test
```

The new tests are in `modules/at_commander.rs` (the open's bound, the outstanding open, the late drop, `try_reopen`) and in the budget derivations of `volte/sms.rs` and `ims/agent/watchdog.rs`.

## 2. Build and deploy

Build the deployable binary with `~/rebuild-gsm-sip-bridge.sh` from a branch that has this feature, and restart the service. Check that both lines register.

## 3. Live: an attached but silent modem (SC-001, SC-002)

The method of 2026-11-28. On the host, find the modem's USB/IP connection (the one that is not the card reader's):

```bash
ss -tn dst <usbip-server>:3240
```

On the router, drop both directions of that connection for 90 s (then 180 s on a second run), removing the rules afterwards:

```bash
sudo sh -c 'R="-p tcp -m comment --comment usbip-modem-test"; iptables -I FORWARD $R -s <host> --sport <port> -d <usbip-server> --dport 3240 -j DROP; iptables -I FORWARD $R -s <usbip-server> --sport 3240 -d <host> --dport <port> -j DROP; date -u; sleep 90; iptables -D FORWARD $R -s <host> --sport <port> -d <usbip-server> --dport 3240 -j DROP; iptables -D FORWARD $R -s <usbip-server> --sport 3240 -d <host> --dport <port> -j DROP; date -u'
```

Pick a window with no challenged refresh due (the agent log's last `the network challenged the refresh` plus about 5 h).

Expected, from the drop until a minute after it:

- `journalctl -u gsm-sip-bridge`: no `vowifi-ims-agent exited` and no `watchdog report` for the modem line.
- `/tmp/ims-agent-0.out`: `modem SMS sweep failed … AT command timeout: /dev/ttyUSB0 did not open within 7000 ms` once, then `… an earlier open of /dev/ttyUSB0 has not returned` on the following passes, 20 s apart; no `registration through P-CSCF` and no `initial REGISTER` in the window.
- After the rules are removed: the next sweep passes run without warnings, or report `No such file or directory` if the USB/IP client dropped the device meanwhile, until it is back.
- The line stays registered (`vowifi-status`, or the reg-event `NOTIFY` lines).

## 4. Live: confirm where the open blocks (optional)

With this feature the blocked open runs on its own thread, named `at-open-ttyUSB0`, which stays blocked while the drop is in place. Read its kernel stack then:

```bash
sudo sh -c 'for P in $(pgrep -f "vowifi-ims-agent --line 0"); do for t in /proc/$P/task/*; do echo "== $(cat $t/comm)"; cat $t/stack; done; done'
```

On the previous build the same command shows it under the SMS sweep's thread, but only with `[vowifi].watchdog_recovery_enabled = false` (restore it afterwards), or the watchdog ends the process first.

## 5. Healthy modem (SC-005)

A normal day: the sweep logs no new warnings, challenged refreshes answer in place as before, and `SIM challenge timings` (with `RUST_LOG=…,gsm_sip_bridge::ims=debug`) show `open_ms` within a few milliseconds of before.
