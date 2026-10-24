# Changelog

All notable changes to this project are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project does not yet commit to [Semantic Versioning](https://semver.org/)
across releases.

## [Unreleased]

### Added

- **SMS Discord delivery retry and status** (`[sms].retry_enabled`, **on by
  default**, #121). A text whose Discord post fails is kept and re-posted,
  oldest first, after the next text arrives and every `retry_interval_sec`
  (1 h), as a `[Late]` message with the original receive time; it is abandoned
  after `retry_max_age_sec` (7 days). Leases in the store keep the daemon and
  the VoWiFi agents from double-posting. New `sms-status [--json]` subcommand
  and `gsm_sip_bridge_sms_undelivered`, `_abandoned` and
  `_oldest_undelivered_age_seconds` gauges. The store migrates v4 → v5
  automatically (one-way: an older binary refuses a v5 store). `failed` rows
  from before the upgrade are never retried.

- **AMR passthrough: client re-INVITEs are judged by the forwarding rule** (#111).
  A phone that renegotiates a forwarded call to parameters it can no longer be
  forwarded under (restricted `mode-set`, CRC, redundancy, a longer packet
  time, a telephone-event mismatch) now gets a 488 and the call carries on, as
  already happened for a codec change. Previously the relay followed it or
  stopped media.

- **`[vowifi].amr_client_rtp_latch`** (default `false`, #111). Lets a forwarded
  call follow a phone behind NAT whose RTP arrives from an address other than
  its SDP's: the first well-formed packet of the negotiated payload type
  re-points the call at its sender, only until a packet has been heard from the
  advertised address. Off keeps the strict source check.

- **`[vowifi].amr_mode_set`** (`strict` default, `relax`, `relax_drop`, #111).
  Relaxes the AMR passthrough rule that the phone's `mode-set` must equal the
  carrier's, which kept every Jio call transcoded. The relay clamps the codec
  mode request towards the phone into the carrier's set and counts (or drops)
  phone frames outside it. Verified live on Jio.

- **AMR passthrough** (`[vowifi].amr_passthrough`, off by default, #111).
  Inbound calls whose carrier and phone legs both negotiated compatible
  AMR-WB (or both AMR-NB) carry the carrier's frames to the phone without
  decoding them, with DTMF, hold/resume and a packing conversion between
  octet-aligned and bandwidth-efficient. Every other call is transcoded as
  before. `vowifi-status` recent calls show `media_path=`. Outbound calls are
  never forwarded. Needs a phone or `[sip_server]` that passes AMR through.

### Changed

- **The supervisor's P-CSCF probe is off by default.** The line supervisor's
  TCP connect to each P-CSCF's unprotected SIP port, which no handset makes,
  no longer runs. `[vowifi].supervisor_pcscf_probe = true` restores it.
  Visible consequences: strongSwan's own DPD now runs at `dpd_delay_sec`
  (the probe's traffic used to keep it from firing), and a dead single
  P-CSCF is detected through the agent in roughly 115s plus one supervisor
  poll instead of ~97s. A lost default route and an exhausted P-CSCF pool
  are repaired exactly as before, and `pcscf_unreachable` recoveries now
  read zero. The new `SUPERVISOR_PCSCF_PROBE` appears in
  `gsm-sip-bridge config --shell-env`
  ([#120](https://github.com/selvakn/gsm-sip-bridge/issues/120)).

### Added

- **Underlay guard for VoWiFi.** While the path from the host to a line's ePDG
  is down, because there is no route to it or because it has stopped
  answering, the supervisor no longer tears the tunnel down and restarts the
  IMS agent every ~80s for recoveries that are only inferred from the ePDG
  being unreachable. Before it acts on one, it sends a single on-demand
  liveness check, so a genuine carrier fault is still repaired. A dead
  charon, a vanished interface and a missing namespace route are never held,
  and a hold ends after 15 minutes at the latest. `vowifi-status` shows
  `uplink:` per line, three `gsm_sip_bridge_vowifi_uplink_*` metrics count it,
  and `[vowifi].underlay_guard = false` turns it off. The image's strongSwan
  gains `swanctl --dpd`
  ([#141](https://github.com/selvakn/gsm-sip-bridge/issues/141)).

- **`[vowifi].dpd_delay_sec`.** How long charon waits with nothing from the
  ePDG before sending a dead-peer-detection request on a strongSwan line;
  `0` turns it off. It was a fixed 30s and now defaults to 120s, the Android
  IWLAN default. With nothing else arriving between the agent's 120s
  keepalives, 30s DPD gave the ePDG an exchange every 30s to ignore, and on
  both Vodafone and Jio one ignored request tore down a working tunnel. No
  effect while the supervisor's P-CSCF probe runs, because its traffic keeps
  DPD from firing. Applies when the container is recreated
  ([#120](https://github.com/selvakn/gsm-sip-bridge/issues/120)).

- **Dead-peer loss counter.** `gsm_sip_bridge_vowifi_dead_peer_losses_total`
  counts IKE_SA losses to an unresponsive ePDG by `verdict`: `false` when
  inbound ESP was still arriving while charon retransmitted, `real` when it
  was not, `unknown` when the supervisor could not tell. The supervisor logs
  the verdict and its evidence
  ([#120](https://github.com/selvakn/gsm-sip-bridge/issues/120)).

- **Rebuild on an exhausted P-CSCF pool.** When a VoWiFi line's IMS agent has
  marked every P-CSCF it was given unavailable since the tunnel was last
  negotiated, the supervisor rebuilds the tunnel to get a new one
  (`recovered from PcscfPoolExhausted`). A carrier with a single P-CSCF, like
  Vodafone IN, used to stay down indefinitely once the supervisor's probe was
  out of the picture; it now recovers from the agent's own verdict. A call in
  progress defers the rebuild until it ends
  ([#120](https://github.com/selvakn/gsm-sip-bridge/issues/120)).

- **`[alerts.pcscf_pool_exhausted]`.** A Discord alert, with a paired
  all-clear, for a line that keeps rebuilding onto a dead P-CSCF pool for
  longer than `unhealthy_sec` (default 300s). Upgrade note: it defaults
  to enabled, like `gm_connection_lost`; set `enabled = false` under
  `[alerts.pcscf_pool_exhausted]` to silence it
  ([#120](https://github.com/selvakn/gsm-sip-bridge/issues/120)).

- **Tunnel recovery counter.** `gsm_sip_bridge_vowifi_tunnel_recoveries_total`
  counts every steady-state recovery the supervisor performs on a strongSwan
  VoWiFi line, by `module` and `reason` (`default_route_missing`,
  `pcscf_unreachable`, `ike_sa_lost`, `child_sa_rekey_stuck`, ...). A rising
  `default_route_missing` count is a route that keeps vanishing, which never
  counts as flapping
  ([#120](https://github.com/selvakn/gsm-sip-bridge/issues/120)).

- **`tunnel_address_family`.** Which inner address families a VoWiFi line's
  ePDG tunnel requests and carries: `dual` (default, unchanged), `ipv4` or
  `ipv6`, under `[vowifi]` and `[[vowifi.line]]`. `ipv4` stops an ePDG that
  assigns only IPv4 from rejecting each hourly CHILD_SA rekey with
  `INVALID_SELECTORS`. A `dual` line logs a hint when that happens; an
  `ipv4`/`ipv6` line ignores P-CSCFs of the other family. Strongswan engine
  only; `swu` refuses a non-`dual` value
  ([#126](https://github.com/selvakn/gsm-sip-bridge/issues/126),
  [#107](https://github.com/selvakn/gsm-sip-bridge/issues/107)).

- **P-CSCF failover.** The supervisor publishes every advertised P-CSCF to
  `/tmp/pcscf-N.pool` beside its unchanged single choice. The IMS agent marks a
  P-CSCF that fails while the tunnel is up (RFC 5626 delay + 5 min, or a long
  `Retry-After`) and registers through the next one at once. Marks persist in
  `/tmp/pcscf-N.state` across agent restarts, and the tunnel is rebuilt only
  when no advertised P-CSCF answers. `vowifi-status`/`volte-status` print
  `pcscf` and `pcscf_unavailable`
  ([#124](https://github.com/selvakn/gsm-sip-bridge/issues/124)).

- **Reg-event subscription tracking.** The reg-event `SUBSCRIBE` is
  correlated with its response and NOTIFYs and refreshed in its dialog. An
  unanswered one (Timer F, 128 s) is checked with an immediate Gm probe, and an
  unanswered probe re-registers the line at once. `vowifi-status`/`volte-status`
  print `reg_subscription` and `reg_subscription_last_failure`
  ([#124](https://github.com/selvakn/gsm-sip-bridge/issues/124)).

- **`reg_rejected_cooldown_sec`.** How long a reg-event `rejected` holds a
  line's registration off before one more attempt (default 1800,
  `60..=86400`), under `[vowifi]`, `[[vowifi.line]]`, `[volte]` and
  `[[volte.line]]`. `vowifi-status`/`volte-status` print it as
  `rejection_cooldown`
  ([#124](https://github.com/selvakn/gsm-sip-bridge/issues/124)).

- **Caller keypad digits relayed to the phone.** RFC 4733 digits received
  on the carrier leg of a VoWiFi/VoLTE call are re-sent on the paired phone
  or PBX leg (the conference bridge between them carries PCM only).
  Unpaired calls (the GSM bridge) are unaffected
  ([#113](https://github.com/selvakn/gsm-sip-bridge/issues/113)).
- **`gsm_sip_bridge_vowifi_child_sa_up` gauge.** Per-line ePDG CHILD_SA
  state, reported by the supervisor and zeroed when stale (3×
  `agent_report_interval_seconds`). `[alerts.tunnel_failure]` fires on it as
  well as on the IMS agent's tunnel signal, once per outage
  ([#107](https://github.com/selvakn/gsm-sip-bridge/issues/107)).

- **AMR and Opus toward the phone.** The VoWiFi/VoLTE phone or PBX leg can
  now use AMR-WB, AMR-NB or Opus (issue #109). With no configuration change,
  a phone or PBX that prefers one of them can now negotiate it; everything
  else behaves as before. New `[sip].codecs` sets the order explicitly
  (`PCMU` is required); `[sip].amr_octet_align` (default `true`) picks octet-aligned or bandwidth-efficient AMR when the bridge makes the offer. Audio is still decoded and re-encoded by the bridge.
  `vowifi-status` recent calls show the negotiated `phone_codec`. The
  circuit-switched path is unchanged.

### Changed

- **One P-CSCF probe per VoWiFi line.** The idle-tunnel keepalive is now off
  by default (`[vowifi].idle_keepalive`, and it honours
  `keepalive_interval_sec`, default 30). The container HEALTHCHECK reads each
  line's IMS agent status instead of connecting to the P-CSCF, and reports
  `IMS agent … not responding` or `Gm connection failed`. About two thirds
  fewer connects on an idle tunnel
  ([#120](https://github.com/selvakn/gsm-sip-bridge/issues/120)).

- **Stuck CHILD_SA rekeys are rebuilt before the hard expiry.** A CHILD_SA
  still in place with under 300 s left (a natural rekey replaces it with
  360-720 s left) is rebuilt by the supervisor as `ChildSaRekeyStuck`,
  once per CHILD_SA, instead of being left to expire and then recovered
  through `ChildSaMissing`/`ChildSaStuck`. The rebuild waits for an active
  call until 90 s remain; the VoWiFi agent publishes its call count in
  `/tmp/pcscf-N.calls`. Strongswan engine only
  ([#127](https://github.com/selvakn/gsm-sip-bridge/issues/127)).

### Fixed

- **A half-open IKE_SA is left to charon.** An IKE_SA still being negotiated
  was counted as a missing CHILD_SA and terminated on the second tick, which
  in an outage killed the attempt charon was still retrying. It is now left
  alone while charon has logged IKE activity for it in the last 200s, and
  rebuilt as `ike_sa_negotiation_stuck` after that
  ([#141](https://github.com/selvakn/gsm-sip-bridge/issues/141)).

- **A dead P-CSCF is given up on about 110 s sooner.** When the IMS agent's
  Gm keepalive went unanswered and reconnecting failed, nothing was tried
  again until the next keepalive was due, a full 120 s after the dead one was
  sent. The second and third attempts, and the escalation that marks the
  P-CSCF unavailable, now follow 10 s apart. On Vodafone, losing the only
  P-CSCF took 225 s to rebuild the tunnel. The expected time is now about
  115 s, plus up to 30 s for the supervisor's next check
  ([#137](https://github.com/selvakn/gsm-sip-bridge/issues/137)).

- **A lost default route is repaired on the next tick.** The supervisor
  checked a line's default route only after its P-CSCF probe had failed, so
  without that probe a deleted route was never put back. It is now checked on
  every steady-state tick, ahead of the probe, and reinstated within 30 s
  without touching the tunnel
  ([#120](https://github.com/selvakn/gsm-sip-bridge/issues/120)).

- **IPv6 lines repair a lost default route at once.** The supervisor's
  default-route check looked at IPv4 only, so on a line whose P-CSCF is IPv6
  a missing IPv6 route went unseen while the IPv4 one stayed: the line waited
  out ~90 s of unreachable strikes and rebuilt the tunnel, dropping any call.
  The check now follows the current P-CSCF's family and reinstates the route
  without touching the tunnel
  ([#133](https://github.com/selvakn/gsm-sip-bridge/issues/133)).

- **NOTIFYs answered per subscription dialog.** A reg-event `NOTIFY` for a
  subscription a renewal replaced is acknowledged without being acted on, and
  one for no subscription of ours is refused `481` (RFC 6665 §4.4.1); a late
  `SUBSCRIBE` response can no longer be mistaken for the Gm keepalive's answer
  ([#124](https://github.com/selvakn/gsm-sip-bridge/issues/124)).

- **Renewal re-registration loop.** A reg-event NOTIFY retiring the binding a
  renewal replaced was read as the current binding being lost, because it
  still echoed the IMEI. Contacts are now attributed by the registered
  address first; an IMEI-echoing entry at another address is logged as
  `replaced binding retired` and ignored
  ([#124](https://github.com/selvakn/gsm-sip-bridge/issues/124)).
- **Per-event deregistration policy.** `deactivated` re-registers at once,
  `unregistered`/`probation` wait on the backoff (at least the network's
  `retry-after`), and `rejected` ends the call and starts a cooldown that
  never spends the renewal-failure budget and refuses outbound calls so they
  go out on another line (TS 24.229 §5.1.1.7)
  ([#124](https://github.com/selvakn/gsm-sip-bridge/issues/124)).

- **Inbound INVITEs with multipart bodies.** Since v8.15.0 any inbound
  VoWiFi/VoLTE INVITE whose body was not bare SDP was refused `415`, which
  failed every inbound T2 call (SDP inside `multipart/mixed` beside a
  PIDF-LO part). SIP bodies are now decoded as MIME (RFC 5621) and the one
  SDP is taken from any `multipart/mixed`, `multipart/alternative` or nested
  body; no usable SDP is still `415` (now with `Accept`), a broken or
  ambiguous multipart body gets `400`, and `OPTIONS` advertises the same
  `Accept`. The SDP answer to a QoS-precondition offer (RFC 3312) states the
  desired status back without requesting a confirmation, and an
  `application/vnd.3gpp.sms` content type with parameters is decoded as SMS.
  Bare-SDP calls without preconditions get byte-identical answers
  ([#103](https://github.com/selvakn/gsm-sip-bridge/issues/103)).

- **Carrier-deleted IKE_SA recovered, with its cause.** The supervisor now
  tells a lost IKE_SA from a lost CHILD_SA, re-initiates, and restarts the
  IMS agent once, logging `carrier DELETE`, `dead-peer timeout` or `unknown`.
  The P-CSCF is taken from the new negotiation (waiting up to 20s) rather than
  the previous one, teardown deletes gracefully before forcing, and leaked Gm
  IPsec state is cleared before each agent start (strongSwan engine).
  `[alerts.tunnel_failure]` also fires on 3 or more rebuilds within
  `unhealthy_sec` and names the last loss cause
  ([#106](https://github.com/selvakn/gsm-sip-bridge/issues/106)).

- **Network-initiated deregistration missed when the binding is listed
  under several AORs.** A reg-event NOTIFY listing the bridge's contact under
  both a SIP and a `tel:` AOR with no IMEI echo was treated as unattributable
  and ignored. Own entries are now matched by IMEI or by the advertised
  Contact address, and every matching entry counts; a sole contact naming a
  different address (a partial NOTIFY about another device) is no longer
  taken as ours. A binding dropped within `REGISTRATION_SETTLE` (60s) of
  registering counts as a failed renewal, so persistent rejection rides the
  backoff and the supervisor-exit budget, deferred until any call ends. A
  detected deregistration also clears the registered metric. Reported by
  @mironovlab ([#117](https://github.com/selvakn/gsm-sip-bridge/issues/117)).
- **Lost ePDG CHILD_SA went undetected for minutes.** The CHILD_SA check
  matched the IKE_SA header, hiding a dropped CHILD_SA. It now matches the
  CHILD_SA line and its state; a first miss re-initiates, a second
  consecutive miss rebuilds the IKE_SA and restarts the agent
  (`DegradeReason::ChildSaStuck`). Steady-state `swanctl --terminate` is
  forced under a 10s timeout and `--list-sas` is bounded at 10s
  ([#107](https://github.com/selvakn/gsm-sip-bridge/issues/107)).

- **A rejected inbound call no longer rings the phone again.** When the
  phone or PBX rejected an incoming VoWiFi/VoLTE call, the bridge told the
  carrier `480 Temporarily Unavailable`, which some networks (T2) read as
  "try again elsewhere": about 11 seconds later the same caller's call was
  offered again and the phone rang a second time. A reject (`486`, `600` or
  `603` from the phone side) is now answered `486 Busy Here`. No answer, an
  unreachable PBX or any other phone-side failure is still `480`, so a
  broken PBX is never reported to callers as a busy line. Every final
  response now keeps the `180 Ringing`'s To-tag, the ACK after a reject is
  no longer logged as unmatched, and the `bridge_failed` metric separates
  declines (`pbx_declined`) from no-answers (`ring_timeout`) and from
  phone-side failures (new `pbx_failed`). Reported by @mironovlab
  ([#108](https://github.com/selvakn/gsm-sip-bridge/issues/108)).

- **VoWiFi line stuck with "control channel not bindable yet" forever.**
  The per-line veth pair was built once, right after the tunnel first came
  up, with every command's result discarded, and nothing ever looked at it
  again. If that single setup failed, Agent B retried its bind indefinitely
  and the supervisor never noticed. Setup is now checked (a failing step is
  logged with its stderr), and the steady-state loop re-verifies the pair
  every tick and rebuilds it with `veth missing; rebuilt`. A line whose bind
  is still failing after about five minutes now logs an `error`, not another
  routine warning. Reported in
  [#96](https://github.com/selvakn/gsm-sip-bridge/issues/96); the reported
  trigger (a reboot within the carrier's stale-session window) is unconfirmed.

- **Inbound Jio VoWiFi calls were torn down by the network moments after
  being answered, with `cause=503 "SDP Protocol Error"`, whenever the call
  was routed through one of Jio's Alcatel-Lucent border elements.** The
  error text is boilerplate and had nothing to do with the SDP: those
  elements validate the `Allow` header on the `200 OK` and reject an answer
  that does not claim `UPDATE`. Bisected live to that one method (0/12 calls
  connected without it, 12/12 with it; substituting `PRACK` at the same list
  length failed again). Responses to an inbound INVITE now advertise
  `UPDATE` via `ims::UAS_INVITE_ALLOW`; every other response still states
  only the methods this UAS actually serves. Supersedes the earlier
  conclusion in `docs/jio-lucent-sbc-sdp-protocol-error.md` that the failure
  was a carrier-side media bug with no fix on this side.

- **A SIP client reachable only over a routed network (Tailscale, or any
  VPN/mesh) under `network_mode: host` got complete silence in both
  directions on every call, even though registration, ringing, and
  answering all worked normally.** Neither the SIP transport nor any
  account's RTP media config ever set PJSIP's `public_addr`, so
  `create_rtp_rtcp_sock()` fell back to `pj_gethostip()`'s default-route
  candidate — a private, host-internal address — for both the SDP `c=`
  line and the Contact/Via headers. A remote softphone was told to send
  its RTP to an address it couldn't route to, so its outbound audio never
  arrived; with no inbound RTP, PJSIP's symmetric-RTP latching never
  triggered either. Reported in
  [#77](https://github.com/selvakn/gsm-sip-bridge/issues/77). A new
  optional `[sip].public_addr` setting (IP literal or hostname, resolved
  once at startup) now flows into the SIP transport and every PJSIP
  account-config rebuild this bridge performs — including
  `Account::set_identity`'s inbound caller-ID rewrite in SIP-server mode,
  which previously reset any address not re-applied there on every single
  inbound call. Deployments that leave the setting unset are unaffected.

- **A graceful container stop left every line's tunnel `if_id` claimed,
  costing the next restart ~2.5 minutes of silence per line.** Stop used to
  only signal every child process and remove each line's namespace *name* —
  never destroy the tunnel interface, which is the only thing that actually
  releases the XFRM `if_id` it claims. The next start would then find that
  id still held by a leftover device inside an unaddressable namespace, and
  wait on the kernel to reap it. Stop now waits for every child, terminates
  each line's IKE_SA, flushes this deployment's own XFRM state, and deletes
  each line's tunnel interface and veth pair before its namespace; a
  force-killed run's leftovers are reclaimed on the next start via a
  namespace directory now shared with the host. VoLTE lines' veth pairs,
  never deleted either, get the same cleanup.

- **The container's DNS resolver was replaced by the carrier's whenever an
  ePDG tunnel came up.** strongSwan's `resolve` plugin was writing the
  assigned IMS DNS servers into `/etc/resolv.conf`, *replacing* the resolvers
  Docker put there at container start rather than adding to them — leaving
  one carrier-controlled nameserver and no fallback. Whether that still
  resolves anything depends entirely on whether the host happens to have a
  route to it: observed live on a Jio line with no IPv6 default route, the
  assigned v6 resolver was unreachable outright and every outbound HTTPS call
  the daemon makes failed, Discord critical alerts and SMS forwarding
  included. Nothing reported it — the alert channel itself was the casualty —
  and it recurred on every IKE re-auth, so a container restart only bought
  until the next one. The plugin now writes to `/run/ims-resolv.conf`
  instead, leaving the system resolver alone; the config request sent to the
  ePDG is unchanged, so this cannot perturb a carrier that is fussy about it.

- **Jio VoWiFi inbound calls.** Our `200 OK` never declared its own
  capabilities (`Allow`/`Supported`), which Jio rejected ~460ms in with a
  boilerplate `cause=503 "SDP Protocol Error"` that had nothing to do with
  the SDP — five different answer bodies had failed identically before this
  was found. Also required: answering on the Gm *client* leg rather than
  the socket a request arrived on (Jio-specific, gated off by default), and
  a REGISTER `Contact` that echoes the tags the caller's own preferences
  require.
- **SMS over IMS.** Inbound `MESSAGE` never reached us at all on Jio until
  the REGISTER `Contact` declared `+g.3gpp.smsip` — a real device sharing
  the same line's registration carried the tag and received SMS; ours
  didn't and never saw one arrive.
- **A crash and message corruption on any binary SIP body.** Every SIP
  receive path converted raw socket bytes to text via a lossy UTF-8
  conversion *before* finding the message boundary or slicing
  `Content-Length` bytes of body. A carrier's SMS-over-IP body
  (`application/vnd.3gpp.sms`) is an arbitrary binary TPDU, not text; the
  lossy conversion could shift the buffer's length away from what actually
  arrived, corrupting whatever followed it on the same TCP connection and,
  in one observed case, panicking the connection's reader thread outright.
  Framing now happens on the original bytes throughout.
- IPv4 traffic on the Gm IPsec tunnel interface was silently dropped —
  `disable_policy` was only ever applied to IPv6.
- A registration's P-CSCF address could appear to "change" across a normal
  IKE rekey, tearing down an otherwise healthy line and reinitiating it
  unnecessarily.
- An abandoned outbound origination attempt was re-abandoned on every
  dispatch-loop tick instead of once, spinning at the loop's poll rate and
  occupying the only outbound slot.
- Compact SIP header forms (`v:`, `i:`, `f:`, `t:`, `l:`) were not
  recognised, which on a carrier using them exclusively broke both
  `Content-Length` framing (desyncing the reader on any response carrying a
  body) and transaction matching.
- Outbound calls: a `183 Session Progress` requiring `100rel` was never
  acknowledged (no `PRACK` support), so the network retransmitted the
  provisional until it gave up and abandoned the call; a response
  answering our own `PRACK` was mistaken for the INVITE's final response,
  causing an `ACK` for a call that was never actually answered; a carrier
  whose SDP answer arrives in the reliable provisional (leaving the `200
  OK` bodiless) was treated as having sent no answer at all; the
  originating identity used on the `INVITE` did not match the one used on
  its `ACK`/`CANCEL`/`BYE`; a `BYE` built without a `Contact` on the
  triggering response lost its `sip:` scheme.
- An XFRM policy leak: cleaning up a Gm IPsec session only removed the
  4 canonical policies for a single transport, while the current setup
  path installs 4 canonical + 4 cross-product policies for **both** TCP
  and UDP (16 total) — every re-registration or reinitiate leaked 12 of
  them permanently in the kernel's policy table.
- A malformed or adversarial `Security-Server` offer naming `q=nan` could
  win selection unconditionally and then never be displaced by a real
  offer (`x > NaN` is always `false`).
- A reordered retransmission of an older reliable provisional response
  (arriving after a newer one had already been acknowledged) could be
  `PRACK`ed a second time.

### Added

- A from-scratch decoder for 3GPP SMS-over-IP (TS 24.011 RP-DATA wrapping
  a TS 23.040 SMS-DELIVER TPDU): GSM 7-bit default alphabet and UCS-2 text,
  with or without a concatenation UDH. The real originating number now
  comes from the TPDU's `TP-OA`, not the SIP `From` — which on a real
  network names an IMS core relay element, not the sender.
- A deployment can now choose the REGISTER request-URI form
  (`register_request_uri`): the literal P-CSCF address, or the
  TS 24.229-mandated home-domain realm. Carriers disagree about which one
  they'll accept.

### Security

- Full, unredacted SIP datagrams (every header, plus the body — including,
  since the SMS decoder above, SMS text) were logged at `debug` level, a
  level this project's own deployments run with routinely in the field.
  Downgraded to `trace`, which preserves the same diagnostic capability for
  a deliberate debugging session without it landing in logs left running
  day to day.
