
Observed pending items
----------------------

- [x] ~~Update docs about the sip listener mode~~ — README, architecture.md,
      configuration.md, observability.md, and RELEASE_NOTES.md all cover
      `[sip_server]` mode as of 8.3.0, including the four hardening fixes
      that landed after the initial docs pass (digest replay/ordering,
      port-collision validation, wildcard listen_addr, and cross-process
      metrics export).
- [ ] Outbounds calls via the GSM (PC / VoLTE / VoWIFI), from pbx as well as sip client
      — **triaged 2026-08-06**: CS, VoWiFi, and PC/SC are audio-verified on real
      hardware (specs/025-outbound-calling T023/T072/T073); VoLTE specifically has
      never been independently exercised for *outbound* calling (T050e left this
      deliberately open). Believed to work — shares `ims::agent`'s origination code
      with VoWiFi — but unconfirmed. Plan: [docs/plans/volte-outbound-verification.md](plans/volte-outbound-verification.md).
- [x] ~~Agent A's `dispatch_loop` (`ims/agent.rs`) blocks entirely for up to
      ~80s (`OUTBOUND_INVITE_TIMEOUT` + `OUTBOUND_RING_TIMEOUT` +
      `VETH_INVITE_TIMEOUT`) while `originate_and_bridge` waits on an
      outbound VoWiFi/VoLTE carrier leg~~ — **implemented in
      specs/029-interruptible-origination-wait** (2026-08-08): origination is
      now a `PendingOrigination` state machine the loop advances on its own
      thread rather than a blocking call. Carrier responses arrive via
      `inbound.rx` (matched by `Call-ID`), so the loop keeps servicing
      everything else while the carrier rings: an inbound INVITE gets a prompt
      `486` (it occupies the line via `Admission`), and a caller hangup relayed
      by Agent B triggers a `CANCEL` toward the carrier within ~100ms instead of
      never being observed. Agent B's own attempt-phase wait became a poll loop
      too, so it notices its caller hanging up. Also fixed the outbound
      lifecycle so a placed call actually reaches `Bridged` (was silently stuck
      at `Answering`), and added a `caller_abandoned` outcome. Residual, by
      design: still one call at a time per line (`Admission::RejectBusy`) — an
      inbound call during an attempt is refused busy, not held.
      **Still needs the live hardware re-test** against the original mid-ring
      hangup scenario (spec T046) to confirm end to end.
      Plan: [docs/plans/dispatch-loop-interruptible-wait.md](plans/dispatch-loop-interruptible-wait.md).
- [x] ~~Line 0's Gm TCP connection (VoWiFi ePDG tunnel) silently resets some
      minutes after registration with no reconnect logic~~ — **implemented in
      specs/028-gm-tcp-reconnect** (2026-08-07): an idle SIP OPTIONS keepalive
      on the dispatch loop detects a dropped Gm connection within ~2 min and
      reconnects proactively (confirmed by a follow-up probe), escalating to a
      full re-registration on repeated failure, all deferred around active
      calls; covers both the client connection and the carrier-facing listener,
      across VoWiFi and VoLTE. Surfaced in `vowifi-status`/`volte-status`, a new
      `gsm_sip_bridge_vowifi_gm_connection_up` gauge, and an
      `[alerts.gm_connection_lost]` Discord alert. Found live during
      specs/025-outbound-calling's T072 hardware verification (pass 1,
      2026-08-03). **Still needs the live hardware re-test (spec SC-010 / T069)
      to confirm against the original scenario** — never reproduced
      synthetically.
      Plan: [docs/plans/vowifi-gm-tcp-reconnect.md](plans/vowifi-gm-tcp-reconnect.md).
- [ ] A specific attached Quectel EC20 (`2c7c:0125`, `EC20-CE-HDLG`) has one
      of its four generic serial ports (`/dev/ttyUSB1` in this session,
      found during specs/025-outbound-calling T023/T033 hardware
      verification, 2026-08-03) that hangs the kernel `option` USB-serial
      driver on any operation — even a bare `stty -F /dev/ttyUSB1 ...` from
      a shell blocks forever, uninterruptible by `timeout`/SIGTERM. Since
      `discover`'s and `CardPool`'s modem scans probe every `/dev/ttyUSB*`
      unconditionally, this silently wedges the **whole daemon's startup**
      (not just outbound calling) whenever this unit is attached — it
      happened once to the unmodified, already-deployed VoWiFi service too,
      restarted for unrelated reasons while this unit was attached. Not a
      bug in `AtCommander`'s timeout handling — the read timeout has no
      effect on this particular blocking syscall (confirmed via
      `/proc/<pid>/task/*/stack` showing `tty_wait_until_sent`). Worked
      around for this session by unbinding the interface at the kernel
      level (`echo '5-1.2.1.2:1.1' > /sys/bus/usb/drivers/option/unbind`,
      after remounting the privileged container's `/sys` read-write) — a
      host-level change, not something in this codebase to fix. A
      permanent fix would be host/environment work: a udev rule
      blocklisting the bad port, or root-causing why the `option` driver
      wedges on it. Per the user, this port is the GNSS/NMEA interface —
      plausibly explains the hang (an out-of-spec write to a port that
      doesn't expect AT-style command framing), though not confirmed. The
      unbind does not survive an unplug/replug or full reboot of the host,
      so this will need to be reapplied (or fixed permanently) if the unit
      is disconnected and reattached.
      Plan: [docs/plans/ec20-bad-port-isolation.md](plans/ec20-bad-port-isolation.md).
