
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
      **Live hardware test passed (2026-08-08, spec T046)**: a real outbound
      VoWiFi call over the EC200 line, hung up mid-ring, produced
      `sent CANCEL for an abandoned INVITE` toward the carrier ~1.6s after
      `180 Ringing`; a `200` that raced the CANCEL was cleanly ACK+BYE'd (no
      phantom leg), and the line recovered to `Registered`/`can_answer`
      immediately.
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
- [x] ~~A specific attached Quectel EC20 (`2c7c:0125`, `EC20-CE-HDLG`) has one
      of its four generic serial ports (`/dev/ttyUSB1` in this session,
      found during specs/025-outbound-calling T023/T033 hardware
      verification, 2026-08-03) that hangs the kernel `option` USB-serial
      driver on any operation — even a bare `stty -F /dev/ttyUSB1 ...` from
      a shell blocks forever, uninterruptible by `timeout`/SIGTERM. Since
      `discover`'s and `CardPool`'s modem scans probe every `/dev/ttyUSB*`
      unconditionally, this silently wedges the **whole daemon's startup**
      (not just outbound calling) whenever this unit is attached — it
      happened once to the unmodified, already-deployed VoWiFi service too,
      restarted for unrelated reasons while this unit was attached.~~ —
      **implemented in specs/030-bad-port-isolation** (2026-08-09, released in
      **v8.9.0**): the daemon-wedge gap is now closed in-code, superseding the
      host-level `udev`/`sysfs` unbind workaround. Discovery runs each per-port
      open/probe on an abandonable worker bounded by a timeout (default 5s): a
      port that never responds is abandoned and the scan moves on, and an
      interface that times out three times in a row is quarantined in memory for
      the process lifetime (keyed by its stable USB-topology path). So one hung
      port can no longer take down startup, the ongoing `CardPool` rescans, or
      the VoLTE `volte-discover-lines` scan. A new `[discovery].excluded_ports`
      config replaces the kernel unbind with an in-container escape hatch that
      *does* survive replug/reboot: list the bad port by exact `/dev/ttyUSB*`
      path or (preferred) USB-topology fragment (`5-1.2.1.2:1.1`, or `5-1.2.1.2`
      for the whole device), which the scan then never opens. Residual, by
      design: the kernel hang itself is **not** fixed (still a hardware/driver
      issue) — this *contains* it, and the abandoned worker stays blocked in the
      kernel for the process lifetime, bounded by the 3-strike quarantine and
      the blocklist. The abandon-and-continue *mechanism* is unit-tested against
      a never-returning fake, but has **not** been re-verified against the
      physical `EC20-CE-HDLG` unit that found this — the literal kernel hang
      needs that specific hardware to reproduce.
      Plan: [docs/plans/ec20-bad-port-isolation.md](plans/ec20-bad-port-isolation.md).
- [ ] `siptest` (specs/037-siptest-softphone) has no unified dialog engine —
      T026/T037 in that spec's task list. Registration runs as a blocking
      function in its own background thread, outbound calls run
      synchronously inside the HTTP handler via `spawn_blocking`, and
      inbound calls run on their own dedicated listener thread, instead of
      one `step(Input) -> Vec<Output>` state machine driving a shared
      per-call dialog table. Every documented *behaviour* (302 handling,
      digest auth, CANCEL/timeout, caller-ID capture) is implemented and
      tested regardless. Concrete consequence: no second concurrent dialog
      can be handled — an inbound call arriving while an outbound call is
      already mid-flight has no path to be processed, since the inbound
      listener thread is blocked inside the first call. Fine for the
      current single-call-at-a-time scope; would block true concurrent-call
      support if that's ever needed.
