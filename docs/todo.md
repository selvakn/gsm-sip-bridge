
Observed pending items
----------------------

- [x] ~~Update docs about the sip listener mode~~ — README, architecture.md,
      configuration.md, observability.md, and RELEASE_NOTES.md all cover
      `[sip_server]` mode as of 8.3.0, including the four hardening fixes
      that landed after the initial docs pass (digest replay/ordering,
      port-collision validation, wildcard listen_addr, and cross-process
      metrics export).
- [ ] Outbounds calls via the GSM (PC / VoLTE / VoWIFI), from pbx as well as sip client 
- [ ] Agent A's `dispatch_loop` (`ims/agent.rs`) blocks entirely for up to
      ~80s (`OUTBOUND_INVITE_TIMEOUT` + `OUTBOUND_RING_TIMEOUT` +
      `VETH_INVITE_TIMEOUT`) while `originate_and_bridge` waits on an
      outbound VoWiFi/VoLTE carrier leg. Nothing else the loop is
      responsible for runs during that window: an inbound carrier INVITE
      arriving then is effectively dropped (its bytes sit in `inbound.rx`,
      but the caller's own SIP Timer B will very likely give up before the
      loop gets back around to it), and a caller hanging up mid-ring can't
      be observed to trigger a CANCEL (`cancel_pending_invite` only fires on
      Agent A's own timeout, not on a phone/PBX-side hangup). Fixing either
      needs an interruptible wait on this loop — a materially bigger change
      than a review pass makes (specs/025-outbound-calling, second/third
      code review, 2026-08-03).
- [ ] Line 0's Gm TCP connection (VoWiFi ePDG tunnel) silently resets some
      minutes after registration with no reconnect logic — found live during
      specs/025-outbound-calling's T072 hardware verification (pass 1,
      2026-08-03). Pre-existing resilience gap, unrelated to outbound
      calling's own correctness; the call just stops being placeable on
      that line until the process is restarted.
