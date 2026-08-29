# Release Notes

<!-- Rename this "## Unreleased" heading to the real "## vX.Y.Z" at release
     time -- publish.yml's notes extractor matches ^## v${TAG}$ exactly, so an
     "Unreleased" heading is not picked up for the GitHub release. -->
## Unreleased

## v8.15.1

- **An inbound call routed to a PBX/trunk with no fixed extension configured now identifies which line it came in on, not who called.** `[bridge].sip_destination` empty used to dial the *external caller's own number* at the PBX as its DID-passthrough fallback — useless for telling lines apart on a deployment where several GSM lines share one SIP trunk. It now dials *this line's own* SIM-read number instead (falling back to the caller's DID only when no SIM number is known yet); the caller's identity is unaffected, since it already reaches the PBX separately via `P-Asserted-Identity`/`X-GSM-Caller-ID`. SIP server mode has no per-line Request-URI to repurpose this way (the destination there must stay the registered phone's real contact), so it instead gets a new `P-Called-Party-ID` header carrying the line's number. Applies to both the circuit-switched and VoWiFi/VoLTE inbound paths. No configuration change.

## v8.15.0

- **An inbound call requiring SDP QoS preconditions on its own segment now connects instead of being refused outright.** `Require: precondition` used to get an unconditional `420 Bad Extension` before its SDP was even read. This bridge's own media relay has no real resource-reservation delay, so a precondition naming only its own segment is now confirmed inline, in the same `200 OK`, with no protocol machinery added. A precondition genuinely requiring end-to-end confirmation this bridge cannot honestly attest to (no way to learn the caller's own segment status) is still declined — now with `580 Precondition Failure` (RFC 3312's own response for exactly this case) instead of the old blanket `420`. No configuration change. Hardware round is regression-only: no carrier reachable here has ever sent `Require: precondition`, so the new logic is verified by unit tests only — see `docs/plans/mt-conformance-findings.md` MT-06.
- **The bridge now actually sends and reads the RTCP it has always claimed it would.** Every SDP answer on the carrier leg of an answered call has long stated `b=RS:`/`b=RR:` — a promise of RTCP reports — while sending or reading none at all. It now sends periodic RFC 3550 sender reports at the rate that declaration implies, reads what the carrier sends back, and sends a leaving-source indication on hangup. The loss, jitter, and round-trip figures the carrier reports about us now show up in the call's own log line and in three new operator metrics (`gsm_sip_bridge_rtp_loss_percent`, `..._jitter_seconds`, `..._round_trip_seconds`), alongside this bridge's own measurement of what it received — the same evidence past one-way-audio investigations have had to reconstruct by hand from packet counters. Scoped to the carrier-facing leg of answered calls only; the internal link to this bridge's own telephone-side software and originated calls are unaffected and keep their existing (unbacked) declaration, unchanged. Live-verified on a real Vi India line: the carrier returned RTCP on the very first call, with a correctly-derived round-trip time. No configuration change. See `docs/plans/mt-conformance-findings.md` batch 7.
- **Responses and SMS decoding now say and read a few more things correctly.** Caller identity now prefers a network-asserted identity (`P-Asserted-Identity`) over the caller-supplied `From` when both are present, since a real network's `From` on SMS/calls can name an internal relay element instead of the actual caller. A VoLTE line's SUBSCRIBE and the `200 OK` to an answered call now state that line's real access network instead of a hardcoded Wi-Fi value. A response now correctly annotates the caller's address (`received`/`rport`) when it differs from what the request's own header claimed. On the SMS side: a delivery report or status report is no longer misread as a deliverable text message; a message that fails to decode now gets an explicit failure report instead of being silently relayed as garbled text; a message-waiting notification using UCS-2 text now decodes correctly instead of coming through as gibberish; and an INVITE whose body isn't actually SDP is now declined instead of scanned as if it were. Not yet exercised on real carrier traffic beyond the ordinary case — see `docs/plans/mt-conformance-findings.md` batch 6.
- **A DTMF keypress on a bridged call now always arrives on the payload type the receiving side actually agreed to.** When both legs of a call already spoke the same audio codec, the relay forwarded every RTP packet untouched, including DTMF — but each leg's own SDP answer picks its telephone-event payload type independently, so a keypress could arrive labeled as something the receiver never negotiated. It's now relabeled to the receiving leg's own payload type when the two differ; the audio payload itself is never touched. Separately, a stream's SSRC changing mid-call (a legitimate signal that the source restarted) is now logged instead of passing unnoticed — it never affects whether the call continues. Not yet exercised on real carrier traffic beyond the ordinary case (matching payload types, no SSRC change) — see `docs/plans/mt-conformance-findings.md` batch 5.
- **An inbound call's answer now honestly describes what the offer actually asked for, instead of a fixed default.** An offer with more than one media section (a second audio line, or a video/text/application section) used to have every section but one silently dropped, with the answer never acknowledging the rest at all; every extra section is now explicitly declined in the answer, in the offer's own order. An offer stating it will only send, only receive, or send nothing (`sendonly`/`recvonly`/`inactive`) used to always be answered as if it had said two-way media regardless; the answer now states the correct counterpart. An offer naming a transport this bridge doesn't implement (anything other than plain RTP) used to be accepted as if it had asked for the one transport this bridge does support; it's now declined with a response distinct from an ordinary successful answer and from the existing "no acceptable codec" decline. Not yet exercised on real carrier traffic beyond the ordinary case — see `docs/plans/mt-conformance-findings.md` batch 4.
- **A hangup or a repeated call setup message is now checked against which call it actually names, instead of being applied to whatever call happens to be active.** A `BYE` naming a call other than the one in progress used to end the live call anyway; it's now refused rather than silently misapplied, and a `BYE` with no call active at all is now refused too, instead of getting an unconditional `200 OK` that implied a dialog existed. A retransmitted call-setup message for a call already answered or still ringing now gets the exact response already given, instead of being reprocessed or silently dropped; a late cancellation for a call already answered now gets an explicit acknowledgement. A second call-setup message for the call already in progress is no longer refused as "busy" when it's actually about that same call — it's declined honestly as unsupported instead, while a genuinely separate second call is still refused busy exactly as before. Not yet exercised on real carrier traffic — see `docs/plans/mt-conformance-findings.md` batch 3.
- **A network-side deregistration is now noticed and recovered from immediately, instead of leaving the line silently unreachable for up to an hour.** The reg-event `NOTIFY` a registrar sends when it drops our own binding (`state="terminated"`, `event` one of `deactivated`/`probation`/`rejected`/`unregistered`) was only logged before; the line kept reporting itself `Registered` and answering nothing until its next scheduled renewal. It now forces an immediate re-registration via the same escalation path a dead Gm connection already used. No configuration change.
- **DTMF now survives a transcoded call.** The relay between a wideband carrier leg and the telephone-side leg was feeding RFC 4733 `telephone-event` packets straight into the audio decoder — every digit a caller pressed on an AMR-WB call became an audible artefact and never reached the far leg at all. Digits are now recognized and forwarded on their own RTP stream, re-stamped onto the far leg's own negotiated event payload type. Live-verified: real keypresses on a real inbound AMR-WB VoWiFi call now arrive as clean, correctly-decoded events.
- **Inbound SMS decoding is now shared between the IMS registration and the modem's own storage, fixing several real gaps.** The modem-storage sweep read text mode (`AT+CMGF=1`), which cannot represent UCS-2 or expose a concatenation UDH, and decoded differently from messages delivered over the IMS registration — defeating de-duplication for exactly the messages most likely to arrive twice. Both routes now decode through the same TPDU parser. Also fixed along the way: an RP-ACK/RP-ERROR (meant to acknowledge an SMS this bridge never actually submits over IMS) could previously be misread as a delivered message and forwarded as text nobody sent; and UCS-2 decoding now reassembles UTF-16 surrogate pairs, so emoji outside the Basic Multilingual Plane no longer come through as `U+FFFD`. Live-verified: a real carrier billing SMS and real user-sent texts (including emoji) all decoded correctly through both routes.
- **The bridge no longer claims SIP capabilities it doesn't have, and now refuses cleanly when a caller requires one it can't honour.** `REGISTER`'s `Allow` and the `200 OK` to an inbound `INVITE`'s `Supported` used to claim methods and extensions (`PUBLISH`/`UPDATE`/`PRACK`/`INFO`/`REFER`, `timer`/`100rel`/`replaces`/`path`/`gruu`) with no implementation behind any of them. A caller's `Require` naming an unimplemented extension was also previously ignored rather than refused. Both are now driven from one list of what's actually implemented (empty today), and an unmet `Require` gets `420 Bad Extension` instead of being silently accepted. Live-verified: real inbound calls on the Vi India line still connect and bridge normally after this landed.
- **A codec mismatch on an inbound call is now reported accurately, and an SMS body this UAS can't render is refused rather than forwarded as garbage.** A carrier offer with no codec this bridge can answer with used to be declined `486 Busy Here`, which drives call-forward-on-busy/busy-tone treatment for the wrong reason — it's now `488 Not Acceptable Here` with a `Warning: 304` stating the real cause. An inbound `MESSAGE` whose `Content-Type` this bridge can't interpret is now refused `415 Unsupported Media Type` instead of being accepted and forwarded as whatever a lossy text conversion produces; a message with no `Content-Type` at all is still treated as plain text, unchanged.

## v8.14.0

- **SMS delivered over IMS is now acknowledged at the 3GPP layer, not just the SIP one — which is what makes a carrier actually deliver it.** TS 24.341 §5.3.2.3 asks a receiver for two answers to an inbound `MESSAGE`: the SIP response, and a *delivery report* sent as a **new `MESSAGE` request** back to the IP-SM-GW carrying an RP-ACK (§5.3.2.4). The bridge only ever sent the first, so the RP layer — the one the short message itself rides on, and the only one that tells the network the text was taken — was never acknowledged at all. Measured by A/B on a live Jio line: with the report off, a submitted SMS is *never* delivered, because the message centre sends only silent Type 0 reachability probes and holds the text (and `[cs].enabled` does not rescue it); turning it on released a message stranded twelve minutes earlier within 40 seconds, along with real traffic queued behind it. Both stranded messages did eventually arrive once acknowledgement resumed, so the failure mode is indefinite delay rather than loss. Controlled end-to-end: a tagged SMS sent from a Vi line arrived on the Jio line 173 ms later, decoded byte-exact, reported, `202 Accepted`, forwarded. Vodafone/Vi delivers real SMS having never received a report, so this is not a precondition everywhere — `[vowifi].sms_delivery_report = false` restores the old behaviour, but no carrier is known to need that. Also fixed alongside it: **silent Type 0 probes are no longer shown to the operator** (TS 23.040 §9.2.3.9 says acknowledge and discard — Jio probes every couple of minutes, and each one had been posting a bogus notification), and **a gateway that mislabels raw unpacked ASCII as packed GSM7 no longer renders as gibberish** (`welcome` had been arriving as `wJ1 -Ä`).
- **Inbound carrier calls now always answer with their capabilities stated.** The `200 OK` to a network-initiated `INVITE` always carries `Allow`/`Supported`, matching the fix that made inbound Jio calling work — previously conditional on the undocumented `GM_UAS_FULL_HEADERS` env var, which existed only in one deployment's container environment and was lost on any redeploy from the repo. The bridge also now answers every method it advertises: `OPTIONS` gets `200 OK`, a stray `CANCEL` gets `481`, and anything else outside `INVITE`/`ACK`/`CANCEL`/`BYE`/`OPTIONS`/`MESSAGE`/`NOTIFY` gets an explicit `405 Method Not Allowed` instead of being silently dropped. No configuration change.
- **`vowifi.respond_on_client` answers network-initiated requests over the Gm client leg instead of the socket they arrived on.** Replaces the undocumented `GM_RESPOND_ON_CLIENT` env var with a per-line config setting, default `false`. Needed for carriers (measured on Jio) that ignore responses sent from the protected server port; leave it off elsewhere — it contradicts RFC 3261 §18.2.2, and Airtel/Vi answer correctly on the arrival socket. See `docs/configuration.md`.
- **Three experimental `GM_*` env vars are gone.** `GM_UAS_CONTACT_NO_TRANSPORT`, `GM_ANSWER_OFFER_FRAMING` and `GM_SDP_MATCH_CARRIER` were never enabled in any working deployment and are not needed by the carrier they were written for; a deployment's `.env` that still sets them can drop those lines with no effect on behavior.
- **`siptest` can play a prerecorded message into a call.** `[media].play_file` names a 16-bit mono PCM WAV transmitted from the start of the call, resampled to whatever codec was negotiated, with the measurement tone plan resuming once the clip ends — so a call that opens with a spoken message is still measurable for the rest of its duration. Validated at daemon startup, so a bad path fails fast rather than mid-call. Also: `siptest call --wait` no longer reports failure on a call that rang, was answered, and completed within a normal duration — its HTTP client previously carried reqwest's 30s default timeout against a request that blocks for ring time plus call duration.

## v8.13.1

- **A backlog of texts in the modem's own storage no longer wedges the SMS sweep permanently.** `AT+CMGL="ALL"` over a full store returns far more than the AT layer's default response bounds allow — a live line with 208 stored messages returned 461 lines / 46,717 bytes — so every sweep aborted with `AT response exceeded 256 lines without terminating` and read nothing. That failure was self-locking: the sweep can only delete messages it first managed to list, so once roughly 128 accumulated the listing always overflowed, storage could never drain, and it grew until the network could no longer deliver to the card at all (the affected line reached 211 of 255 before this was caught). Each abandoned pass also left the rest of the reply still arriving, holding the modem's exclusive port lock for ~20s out of every 40 — well past `vowifi-usim-bridge`'s retry budget for EAP-SIM APDUs, so a stuck sweep could take VoWiFi registration down with it. The line cap and the command deadline are now a per-command budget, raised only for bulk listings and unchanged for every other AT command; both needed raising, since the deadline bounds *wire time* and a full store is ~56 KB, close to five seconds at 115200 baud on a perfectly healthy line. No configuration change. Affects any line whose modem storage has accumulated messages, on both the VoWiFi and VoLTE paths; a regression introduced in v8.13.0 by the AT stall watchdog's response cap.

- **A force-killed container now recovers on the next start instead of failing FATAL on a startup race.** The old container's `pcscd` can still hold the vpcd reader's host-shared port for a brief window after `docker kill` returns, because that teardown is asynchronous relative to the command returning. A single readiness attempt could not tell that race apart from a genuinely misconfigured port, so it failed immediately — and since `pcscd`/vpcd startup ran *before* leftover reclamation, that one hiccup also meant the force-killed run's own leftover namespaces and interfaces were never reclaimed, defeating the shutdown/resource-cleanup work on exactly the case it exists for. Startup now retries the spawn-and-wait cycle, reaping the failed `pcscd` between attempts and on the final give-up, and reclamation runs first so even a startup that ultimately fails leaves a clean slate for whatever tries next. No configuration change. Fixes #53.

## v8.13.0

- **Restarting the container no longer costs every line ~2.5 minutes of silence.** A graceful stop used to only signal every process and remove each line's namespace *name* — never the tunnel interface itself, which is the only thing that actually releases the XFRM `if_id` a line's `tun23-N` device claims. The next start would then find that `if_id` still held by a leftover interface inside a namespace nothing could address, and fail to recreate it until the kernel reaped the old container's mount namespace on its own. Stop now waits for every child (confirming even a SIGKILL, with a settle-and-retry on the device delete for a child still being reaped), terminates each line's IKE_SA, flushes this deployment's own XFRM state, and explicitly deletes each line's tunnel interface and veth pair before its namespace — in that order, bounded, with a fallback that still reaches the releasing steps if the stop allowance runs low. Live-verified on real Vodafone/Vi hardware: 167s down to 8s per restart, zero `if_id` conflicts across 10 consecutive restarts. VoLTE lines get the same veth cleanup, which they never had either. A force-killed run's leftovers can now be reclaimed on the next start too, via a namespace directory shared with the host (`docker-compose.yml`'s new `/var/run/netns` bind mount and `stop_grace_period: 90s`) — **opt-in**, off by default: set `GSM_SIP_BRIDGE_RECLAIM_LEFTOVER_NETNS=1` only when you're certain no second instance of this deployment is running on the host, since nothing the container can check from inside its own PID namespace can rule that out. No configuration change otherwise. See specs/041-shutdown-resource-cleanup.

- **A modem that stops answering can no longer take a line off the air silently.** A re-registration that blocked forever in a serial read used to freeze the very thread that answers calls, and nothing noticed — the supervisor only restarts a line whose process has *exited*. Every modem command is now bounded by a deadline the caller controls, and a watchdog restarts a line that has stopped making progress, including the background message sweep. A lapsed registration is now visible in `vowifi-status`, in a new `gsm_sip_bridge_vowifi_registration_expires_in_seconds` metric, and in the container healthcheck. Renewal follows the lifetime the network granted rather than assuming an hour. A line whose tunnel loses its default route — which a brief internet outage causes — now recovers in seconds instead of staying unreachable. Repeated failures escalate through the existing SIM-recovery ladder. `[vowifi].watchdog_recovery_enabled = false` preserves a wedged line for diagnosis. See specs/039-at-stall-watchdog.
- **The container no longer accumulates dead processes.** The tunnel keepalive shelled out to `timeout 3 bash -c '>/dev/tcp/...'` every 30 seconds, and the inner process was orphaned and never reaped — 462 zombies had built up in under four hours, which given enough uptime exhausts the process table and breaks something unrelated and hard to trace back. It now uses the built-in probe, which has the same 3-second timeout and waits for its child.

- **VoWiFi lines no longer lose SMS the carrier delivers through the modem's own storage.** A VoWiFi registration advertises voice capability, not messaging, so a carrier can deliver a text into the modem's own SIM storage instead of over the IMS registration — confirmed live: 6 of 7 parts of a real multi-part message sat unread because nothing on the VoWiFi path ever read that storage. The line's modem storage is now swept periodically (independent of `[cs].enabled`), reusing the same mechanism already shipped for VoLTE (specs/017). When a carrier delivers the same text over both the registration and the modem — which does happen — the operator now sees it exactly once instead of twice; VoLTE gains this same duplicate-suppression fix too, closing a latent gap that predated this change and affected it as well. The modem-storage sweep coexists safely with `vowifi-usim-bridge`'s own use of the same AT port (brief per-command sessions with a bounded retry, rather than one long hold), so it does not risk destabilizing EAP-SIM/registration. No configuration change. See specs/038-reliable-sms-delivery.

## v8.12.0

- **Inbound VoWiFi calls on Jio now connect instead of failing with a misleading `SDP Protocol Error`.** Jio's network was rejecting the bridge's `200 OK` because it never declared its own SIP capabilities (`Allow`/`Supported`) — the carrier's `cause=503 "SDP Protocol Error"` was boilerplate and had nothing to do with the SDP body itself, which sent five different SDP variants down a dead end before this was found. No configuration change; live-verified with a real inbound call on a Jio line (`media="both-ways"`, zero SDP-protocol-error `BYE`s).
- **SMS over IMS is now decoded and forwarded as readable text, not dropped or forwarded as raw bytes.** Three fixes together close this out: the bridge now declares `+g.3gpp.smsip` in its REGISTER `Contact`, which Jio requires before it will even attempt SMS delivery; the SIP parser was rewritten to frame messages on the actual wire bytes instead of a lossy copy, fixing a crash (and subsequent message corruption) on any binary-body `MESSAGE` — a bug affecting 3GPP SMS-over-IP on any carrier; and a new TS 23.040/24.011 TPDU decoder turns the delivered payload into an actual sender and message body. Live-verified end-to-end on a Vodafone/Vi SIM.
- **Outbound call signalling is now protocol-conformant end to end.** Fixes PRACK handling for reliable provisional responses, a `CSeq`-method dispatch bug, per-dialog identity (previously shared across dialogs), a provisional response that carries the SDP answer, and a malformed `BYE`. Also fixes an IPv4 XFRM `disable_policy` gap that was silently dropping decrypted Gm traffic (previously IPv6-only), spurious reinitiation triggered by ordinary P-CSCF churn, an origination retry spin loop, and compact SIP header parsing that could kill the reader on Jio's `183`. Jio itself still refuses every outbound origination at the network/provisioning level (announcement server, no `180`, vague reason codes) — that is carrier-side, not a protocol defect, and is not fixed by this change. Live-verified: outbound Vodafone/Vi calls place and connect.
- **Outbound calls now play the carrier's pre-answer audio instead of silence.** A carrier that sends an announcement before answering (or rejecting) an outbound call — e.g. Jio playing ~13.7s of message audio before a `480` — is now relayed live to the caller, the same way any other SIP client would render early media. Previously that audio was discarded until the real `200 OK`, so a caller heard nothing at all and a carrier-side rejection read as "no call is being placed." No configuration change; applies to every outbound call automatically. Live-verified against a real Jio line with a real phone call (specs/037-p-early-media).
- **New `siptest` daemon for agent-driven end-to-end testing of the bridge.** A standalone SIP softphone, driven over an HTTP control API, that registers to the bridge like an ordinary handset, places and receives real calls, and verifies audio in both directions with tone generation and Goertzel detection rather than just packet counts — closing the long-standing gap where `MediaReport::round_trip_delay` was always `None`. Supports PCMU and G.722 (`--codec {auto,pcmu,g722}`), records both directions to WAV, and rejects an inbound `INVITE` from anywhere but the configured bridge host outright, with no reply, so it cannot be made to send RTP at an arbitrary third-party destination. Ships as a separate binary (`siptest`); no change to the bridge's own runtime behavior. See `specs/037-siptest-softphone/quickstart.md`.

## v8.11.1

- **The cellular-internet sidecar now self-heals a wedged QMI control endpoint instead of deadlocking with the internet down.** When the carrier drops the modem's data session, the modem's QMI control channel can hang up — the `/dev/cdc-wdm0` node stays present but every `qmicli` open returns `endpoint hangup`, and the libqmi `qmi-proxy` keeps serving the dead handle, so the channel stays wedged even after the modem itself recovers. The sidecar's teardown previously treated only a *vanished* device node as recoverable, so in this state it retried the WDS stop forever, kept the session identity, and refused to dial again — leaving internet down until a manual container restart or USB reset. Teardown now diagnoses why the stop failed: if QMI is unreachable it recycles the stale `qmi-proxy` (so the next command re-opens the current device) and drops the stale identity; if the session has already ended it drops the identity — in both cases the supervise loop redials on its own. Observed live on an EC20F recovering a dropped session with no operator action. Also fixes a `session_connected` probe that matched `'disconnected'` as a substring of `connected`, so a torn-down session is no longer misread as still up. QMI-only and unchanged for the bridge; no configuration change (specs/032).

- **Every alert now names the SIM and the host it came from.** Both SMS forwards and critical-event alerts carry the card's phone number and an instance identity, so an operator watching a shared alert channel can triage without cross-referencing machine-shaped card ids against a spreadsheet or the logs (specs/034). The phone number reuses the per-line `msisdn` (VoLTE) and a new `msisdn` on `[[vowifi.line]]`, falling back to the live `AT+CNUM` value for circuit-switched cards; when no number resolves the Phone field renders the literal `unknown` rather than a fabricated value. The instance identity is a new optional `[alerts].instance_name`, shown in every embed footer as `gsm-sip-bridge · <name>`, falling back to the system hostname when unset. No configuration change is required — existing alerts simply gain the two fields.
- **The published bridge image is now ~60% smaller, and the legacy SWu tunnel engine is opt-in.** The default image (`ghcr.io/selvakn/gsm-sip-bridge:<version>` and `:latest`) is now a **slim** image built without the SWu/Python tunnel engine — no Python interpreter, no Python dependency tree, no vendored SWu-IKEv2 dialer (~72 MB, over 60% of the old image). It runs the default `strongswan` VoWiFi engine exactly as before. The bridge also no longer installs the `bind-tools` (the ePDG DNS lookup now resolves in-process, so `dig` is gone), `wget`, or — on the slim image — `net-tools` apk packages; the Alpine base still provides busybox `wget`/`route` applets, so this is a package trim, not a capability change. **Action required only if you set `[vowifi].tunnel_engine = "swu"`:** the slim image no longer contains that engine and the bridge now fails fast at startup with a message telling you so. Pull the **full** image instead — same name, `-swu` tag suffix (`ghcr.io/selvakn/gsm-sip-bridge:<version>-swu`), published on demand via the new "Publish SWu (full) Docker Image" workflow. Deployments on the default `strongswan` engine need no change; floating tags simply resolve to the slim image. The SWu engine's code and tests remain in the tree and in CI unchanged (specs/033).

## v8.10.0

- **One cellular card can now carry both the internet and the calls, with the bridge waiting for the uplink before it starts.** On a site with no wired or Wi-Fi uplink, the same SIM that handles VoWiFi can also provide the internet: VoWiFi uses the modem only as a SIM/APDU reader (`AT+CSIM`) and its ePDG tunnel rides the host's ordinary default route, so the modem's data bearer is free to carry traffic at the same time. Bringing that data connection up is now a separate, opt-in **cellular-internet sidecar container** (specs/032) that dials over the modem's **QMI** interface — deliberately QMI-only, so the modem's AT port stays free for the bridge and the two never contend. It reports healthy only once a DNS probe actually resolves through the cellular link (not merely that the interface has an address), and the bridge is gated on that with `depends_on: condition: service_healthy`, so it never starts registering against an uplink that does not exist yet. The sidecar owns the session lifecycle independently: it self-heals a dropped connection, survives a modem re-enumeration, and can be restarted without touching the bridge.
- **Opt-in and off by default — existing deployments are unaffected.** The base `docker-compose.yml` is unchanged and contains no reference to the sidecar, so nothing new runs and no startup dependency is added unless you explicitly include the overlay:

  ```
  docker compose -f docker/docker-compose.yml \
    -f docker/docker-compose.cellular-internet.yml up -d
  ```

  Configure it with `INTERNET_APN` (required — the carrier's *internet* APN, never an IMS one) and, if your modem's QMI node is not `/dev/cdc-wdm0`, `INTERNET_QMI_DEV`. See [docs/ec20-internet-plus-vowifi.md](docs/ec20-internet-plus-vowifi.md).
- **Start both services together from a clean state.** The ePDG tunnel runs with `mobike = no`, because the carrier's ePDG advertises MOBIKE support and then never answers — which means an already-established tunnel cannot migrate to a new source address. Switching the host's default route underneath a running bridge will therefore strand its tunnel until it is restarted; bringing the uplink up first and letting the bridge bind to it is exactly what the readiness gate arranges. Validated on an EC20F with a Vi India SIM carrying internet and VoWiFi calls simultaneously.
- **Requires a QMI-capable modem.** The sidecar drives data over QMI (EC20/EC25 and other Qualcomm-based Quectel modules). A non-QMI module — notably the UNISOC-based EC200U, which exposes no `/dev/cdc-wdm*` at all — is out of scope, and the sidecar fails fast at startup with an explicit message rather than silently degrading or reaching for the AT port. A modem that already brought a session up by itself (autoconnect is enabled out of the box on some units) is adopted rather than fought.
- **New published image.** Tagged releases now also publish `ghcr.io/selvakn/gsm-sip-bridge-internet` (multi-arch, amd64 + arm64, same version tags as the bridge), so a same-card-internet deployment can pull the sidecar instead of building it from a repo checkout. The bridge image itself is unchanged in content and size — the internet tooling ships only in the sidecar.

## v8.9.1

- **Fixed a hostname lookup in the PJSIP build that could stall every call by 5 seconds and get it rejected.** PJSIP resolves the local hostname as the first candidate when working out which address to advertise for media, and — with no STUN server configured, which is this project's default — it does so *per call*, while building the media transport, before the SDP answer and so before the `200 OK`. On a host where the container's hostname isn't in `/etc/hosts` and the nameserver drops rather than answers the query, that lookup blocks for the full resolver timeout (5s by default) on every call, which is long enough for the carrier or PBX to give up on the `INVITE` and reject the call; the inbound VoWiFi path pays it twice per bridged call. The build now sets `PJ_GETHOSTIP_DISABLE_LOCAL_RESOLUTION`, which skips the hostname candidate and leaves address selection to the default route plus a full interface enumeration — which is what actually got picked anyway, since the hostname typically resolves to a loopback address that PJSIP then ranks below the default route. No configuration change, and no behaviour change on hosts that were already resolving their hostname quickly. Reported by a user hitting the 5s stall.

- **A serial port that hangs the kernel driver no longer wedges the whole discovery scan (or daemon startup with it).** A specific modem interface can hang the kernel `option` USB-serial driver on any operation — even a bare open — in a way no userspace read-timeout can break; probing it during the startup/rescan scan used to block the entire daemon, and had already taken the deployed VoWiFi service down once on an unrelated restart (specs/030). Each per-port probe now runs on an abandonable worker bounded by a timeout (default 5s): a port that doesn't respond is abandoned and the scan moves on, and an interface that times out three times in a row is quarantined in memory for the rest of the process's life (logged once, at `WARN`, when it crosses that threshold). This is automatic and needs no configuration. Applies to startup discovery, the ongoing circuit-switched rescans, and the VoLTE `volte-discover-lines` startup scan.
- **New `[discovery]` config — an operator escape hatch for a known-bad port.** `excluded_ports` lists ports to never open or probe, by exact `/dev/ttyUSB*` path or (preferred, since it survives replug/reboot) a USB-topology fragment: `5-1.2.1.2:1.1` for one interface, `5-1.2.1.2` for a whole device. The abandon-on-timeout log prints the interface path so it can be copied straight into the list. `probe_timeout_ms` (default 5000, clamped up from a 1000ms floor) tunes the abandon budget. The section is optional; absent, discovery behaves exactly as before.
- **Outbound call origination no longer blocks the line's dispatch loop, so a mid-ring caller hangup is relayed and an inbound call isn't dropped.** Placing an outbound carrier call used to park the agent's dispatch loop in a blocking wait for up to ~80s (specs/029). During that window a caller who hung up mid-ring was never noticed — so **no `CANCEL`** reached the carrier (it kept ringing the destination and could connect a phantom call) — and an inbound call arriving in the meantime got silence until its own timer expired. The origination wait is now interruptible: the loop keeps watching the caller's line, sends a `CANCEL` within ~100ms of a caller hangup, and stays responsive to inbound work throughout. CANCEL/INVITE responses are correlated by `CSeq`, and a CANCEL that fails to send is retried rather than dropping a live INVITE.

## v8.8.0

- **A silently-dropped carrier signaling (Gm) connection is now detected and reconnected automatically, instead of leaving a line dead until the next renewal or a restart.** A registered VoWiFi/VoLTE line whose Gm TCP connection was reset without notice used to keep reporting itself `Registered` while being unable to place or receive a call — for up to ~55 minutes, until a scheduled renewal happened to rebuild it, or indefinitely (found live during specs/025 T072). The line's dispatch loop now sends a lightweight SIP `OPTIONS` keepalive on a 2-minute idle timer; a failed or unanswered probe triggers a proactive reconnect (confirmed by a follow-up probe before the line is reported healthy again), and repeated failure escalates to a full re-registration — all deferred around any call in progress. Both halves of the Gm association are covered (the connection the line registered over and the carrier-facing inbound listener). Health is visible in `vowifi-status`/`volte-status` (`gm_connection: up | reconnecting … | failed …`) and on the metrics endpoint (`gsm_sip_bridge_vowifi_gm_connection_up`).
- **Upgrade note — new alert defaults enabled.** A new `[alerts.gm_connection_lost]` category fires when a line's Gm connection stays unrecoverable past `unhealthy_sec` (default 300s), with a paired recovery notice. Unlike the other alert categories it defaults **enabled**, since a registered line that cannot restore its connection is unambiguously an incident. An existing `config.toml` with no such section gets it on automatically at the 300s threshold and the global `[alerts]` webhook; set `enabled = false` under `[alerts.gm_connection_lost]` to silence it.

## v8.7.1

- **Fixed a startup/re-registration race that could leave a VoWiFi line stuck "unreachable" for minutes.** A modem line's `vowifi-ims-agent` used to independently reopen the modem's AT port at its own startup and on every re-registration to derive MCC/MNC/IMSI/IMEI whenever they weren't explicitly pinned in config — the default case. That raced `vowifi-usim-bridge`, which starts around the same moment and holds the same port open indefinitely for AT+CSIM proxying; the two processes writing to the same serial port concurrently could garble each other's AT command/response bytes (`AT+CIMI failed: 3`), and since `vowifi-usim-bridge` never releases the port, the agent kept failing every 5s restart until it happened to land in a moment usim-bridge was briefly idle. `discover` already reads the modem's IMSI once during its own probe; it now also derives whatever else is missing (IMEI, MCC/MNC) in that same one-shot pass, before any long-running process exists to race against — closing the gap instead of just narrowing it.

## v8.7.0

- **A configured VoWiFi line that never comes up at startup is now retried, self-heals, and is reported instead of silently missing.** A `[[vowifi.line]]` pin (`modem_port`/`modem_serial`) that raced a slow-enumerating modem at boot used to just be absent from `vowifi-status` with no indication anything was wrong. `discover` now keeps retrying in the background when no VoWiFi line resolved at startup, and starts the subsystem automatically the moment the pinned line is found — no restart needed. If it's still missing, `vowifi-status` and `healthcheck` now report it explicitly (`Configured line ... NOT RUNNING`), and an opt-in Discord alert (`[alerts.line_discovery_failed]`) fires after a grace window, with a matching recovery notice if it comes up later.
- **`discover`'s SIM probe now retries a transiently-unreadable card instead of giving up immediately.** A SIM that answers `AT+CPIN?` with an error right at boot (found live on real EC20 hardware) used to be reported as unusable for the rest of that run. `discover` now attempts one `AT+CFUN=0` -> `AT+CFUN=1` radio cycle and re-probes before giving up — scoped to the one-shot startup scan only, never the ongoing background rescan, so a modem that's already registered or mid-call is never touched.

## v8.6.0

- **`[scheduled_restart].restart_mode`: choose a soft radio cycle instead of a full modem reset.**
  The nightly preventive restart has always sent `AT+CFUN=1,1` to every card — a complete module
  reset that can move the card's ttyUSB path. Setting `restart_mode = "radio"` sends `AT+CFUN=0` ->
  `AT+CFUN=1` instead: it drops and re-acquires network registration without power-cycling the
  module or re-enumerating USB. Default (`"full"`) is unchanged; manual `card restart` is
  unaffected either way.

## v8.5.1

- **VoWiFi USIM bridge now self-heals a SIM that drops off the modem bus.**
  A physically/electrically dropped SIM used to surface only as a repeating
  `AT+CSIM failed` warning with no automatic recovery — the existing
  auto-recovery (power-cycle via `AT+CFUN=0` -> `1` after 3 consecutive
  failures) was wired to a different process than the one that now actually
  talks to the SIM, so it never fired. The recovery is now wired into the
  process that owns the modem connection, so a dropped SIM reconnects on its
  own within seconds instead of requiring a manual container restart.

## v8.5.0

- **`gsm-sip-bridge pcsc-list`: enumerate every PC/SC reader's card without
  hand-decoding `EF_IMSI`.** A multi-reader `pcsc_reader` deployment
  (specs/023-omnikey-pcsc-vowifi) needs each line's `imsi_override` to name
  which physical reader's card it owns — previously that meant running
  `pySim-read.py`/`opensc-tool` and decoding the SIM's BCD-encoded `EF_IMSI`
  by hand. This lists every real (non-vpcd) reader with whatever card it
  holds — IMSI, MCC/MNC (from the same `EF_IMSI`/`EF_AD` read `vowifi-plmn
  --pcsc-imsi` uses), and a carrier name looked up against
  `mcc-mnc-lookup.com` — in one table. A reader with no card, or a card whose
  identity can't be fully read, still gets a row with a status note instead
  of aborting the whole listing; a failed carrier lookup (offline, API
  unreachable) just leaves that column blank. Diagnostic-only, same category
  as `vowifi-imsi`/`vowifi-plmn` — no effect on any running line.

## v8.4.0

- **Circuit-switched call handling can now be turned off entirely.**
  `[cs].enabled` (default `true`) stops the circuit-switched daemon's modem
  discovery, periodic rescan, AT traffic, and call handling outright — for a
  deployment where every call already goes over VoWiFi or VoLTE, there is no
  more background probing of a path that will never carry a call. A
  deployment that does not set it is byte-for-byte unaffected.

  **Upgrade-visible behaviour**: if this deployment previously registered a
  trunk with a PBX and you set `[cs].enabled = false` without also enabling
  VoWiFi or VoLTE, that trunk registration stops — with nothing behind it,
  keeping it up would advertise capacity that no longer exists, so the PBX
  will mark it down. The startup log names `[cs].enabled` as the reason. If
  VoWiFi or VoLTE is enabled, this has no effect: that subsystem already
  owns the telephone-facing side and is untouched.

  A voice-capable modem with no `[[vowifi.line]]` override, which the
  circuit-switched path reserves for itself while enabled, becomes
  available to VoWiFi once the flag is off — so a VoWiFi-only deployment
  needs no `[[vowifi.line]]` entries even on hardware VoWiFi alone would not
  have claimed by default. The metrics/control/message-store services this
  process hosts alongside the circuit-switched path keep running unchanged.
  See `docs/configuration.md#cs`.

- **Outbound calling.** `[outbound].enabled` lets the PBX — or, in SIP server
  mode, a registered phone — dial out through the mobile network, on
  whichever line (circuit-switched, VoWiFi, or VoLTE) is idle. Off by
  default, and a deployment that does not enable it is unaffected.

  A PBX INVITE to the bridge's trunk account, or a registered phone's own
  INVITE (302-redirected back to the bridge's dial-out port instead of the
  `403` it always got before), picks the first idle line with no path
  preference and dials it. The destination is the Request-URI's user part,
  dialed verbatim — no allow-list. Refused with `503` if every line is busy,
  `484` for an invalid destination; a carrier rejection is relayed with its
  own status where the path can determine it, distinguishing "unanswered"
  from "refused" in both logs and the `gsm_sip_bridge_outbound_attempts_total`
  metric.

  Circuit-switched and VoWiFi/VoLTE lines each live in their own process
  with their own line pool — there is no cross-process fallback between
  them, and no cross-process audio bridge exists for the circuit-switched
  case, so a line pool with nothing idle refuses outright rather than
  reaching into the other process's lines.

  Two known limitations, tracked in `docs/todo.md`: circuit-switched call
  progress is coarse (accepted once dialing is confirmed, not once
  genuinely answered — the modem's `ATD` response doesn't distinguish the
  two), and the VoWiFi/VoLTE dispatch loop blocks for up to ~80 seconds
  while a call is in flight, during which a caller hanging up mid-ring can't
  trigger a CANCEL and an unrelated inbound call is dropped.

## v8.3.0

- **The bridge can now be the SIP server itself, so a small deployment needs no
  PBX at all.** Set `[sip_server].enabled` and IP phones REGISTER directly to
  the bridge; inbound calls from any of the three carrier paths — circuit-
  switched, VoWiFi, VoLTE — ring one configured account. Off by default, and a
  deployment that does not enable it is byte-for-byte unaffected.

  Previously a PBX was a hard dependency however small the site: the bridge
  could only deliver a call by registering to a telephone system as a trunk and
  INVITEing it. For one SIM and one desk phone, standing up and maintaining
  that PBX was the larger half of the work.

  Phones authenticate with digest credentials from
  `[[sip_server.account]]`, and the registrar handles the full registration
  lifecycle: challenge, refresh, expiry negotiation, explicit
  un-registration, replay rejection, and a handset that moves to a new
  address. Inbound only — a phone cannot dial out over the mobile network,
  which the bridge has never been able to do, and the attempt is refused
  explicitly rather than left to time out.

  **Enabling it requires moving one port.** `[sip_server].listen_port` and
  `[sip].local_port` are two separate SIP endpoints and cannot share one UDP
  socket, and both default to 5060 — so leave 5060 for the phones and set
  `[sip].local_port = 5062`. The mismatch is refused at startup with the fix in
  the message, as is a leftover `[sip].server`, a `ring_aor` matching no
  configured account, and duplicate account names, a `listen_port` colliding
  with a running VoWiFi/VoLTE telephony agent's fixed internal ports or
  per-line loopback span, and `[vowifi].enabled` alongside a running VoLTE
  telephony side (both would try to host the registrar). None of those fail
  silently at call time.

  Digest authentication is replay-safe: a nonce-count is only accepted after
  the credential that goes with it is verified, so a captured or guessed
  request can no longer advance — and thereby lock out — another account's
  counter, and a legacy request that claims `qop=auth` without the `nc`/
  `cnonce` RFC 2617 requires it to carry can no longer skip the replay check
  meant for it. The wildcard `listen_addr` (the default) no longer appears
  verbatim as the calling identity in a handset's From header; the realm
  stands in for it instead.

  Five `gsm_sip_bridge_sip_server_*` metrics; see
  `docs/operations.md#sip-server-mode` for the runbook, including the one
  handset setting ("accept SIP only from proxy") that interacts with the
  two-port design. On the VoWiFi/VoLTE paths the registrar is hosted by the
  telephony agent, which serves no `/metrics` of its own — those gauges are
  now reported over the agent's existing control channel and exported by the
  daemon that actually serves `/metrics`, rather than silently reading zero.

- **A line whose tunnel interface cannot be recreated no longer tears down its
  SA — or kills its agent — every 30 seconds while it waits.** The steady-state
  `TunVanished` branch recreated the line's XFRM interface and then terminated +
  reinitiated its IKE_SA unconditionally, including when the recreation had just
  failed, and the caller restarted the line's `vowifi-ims-agent` on top of that.
  A failed recreation is now its own outcome (`DegradeReason::TunUnavailable`)
  and does neither.

  Measured live 2026-07-31, across nine container restarts on two Airtel lines:
  when the container is replaced, the *previous* run's `ims<N>` namespaces
  survive it, and with them the `tun23-<N>` devices holding this deployment's
  `if_id`s — so for about two and a half minutes no tunnel interface can exist
  at all. Both lines came up in 163–195s on immediate replacement, against 11s
  when the same restart followed a three-minute stop, which is what identifies
  the wait rather than the startup as the cost. Over two startups through that
  window:

  | | before | after |
  |---|---|---|
  | IKE_SA setups, line 0 | 8 | 2 |
  | IKE_SA setups, line 1 | 6 | 2 |
  | time to both lines up | 195s, 195s | 166s, 166s |

  Two per line over two startups is the floor — one apiece. None of the extra
  setups could produce a data path, and all of them were visible to the carrier
  as connection churn. Waiting costs nothing by comparison: a line with no
  interface carries no traffic either way, and the first tick after the id
  frees recreates it and recovers normally.

  The agent-restart half of this changed nothing measurable — 48 restarts over
  two startups, before and after. Nearly all of them are the agent's own
  crash-loop (it starts, cannot reach its P-CSCF without an interface, exits,
  and is restarted 5s later), which this does not address; suppressing that
  would mean not starting the agent until its line has an interface, which is
  not attempted here. The tick-driven kills are gone because killing a process
  over a condition it cannot affect is wrong on its own terms.
- **The `if_id`-claimed diagnostics no longer send you after a leak that isn't
  there.** The interface-creation failure message stated as fact that
  `ip xfrm state flush && ip xfrm policy flush` releases a claimed `if_id`. It
  does not, and the reason is now understood: an XFRM interface registers its
  `if_id` in the namespace it was *created* in rather than the one it is moved
  to, so `tun23-N` holds the id against the host namespace while being
  invisible to `ip link show` there — no XFRM state is involved and nothing
  flushes a netdev. Neither `reclaim_stale_xfrm`, nor the shutdown plan's
  `ip netns del`, nor a clean exit 0 shortened the wait; a fully stopped
  deployment still held both ids 2m29s later. `docs/operations.md` now leads
  with that measurement, explains why a *healthy* deployment also refuses its
  own ids, and keeps the unfiltered flush only for the genuinely different case
  of foreign XFRM state that `supervise` declines to touch.

## v8.2.0

One correction, to a claim this project had been repeating in config
validation, docs, and its own code comments: that a card-reader line's
network identity cannot be read from the card. It can. Everything that
identifies a SIM — the IMSI, and the MCC/MNC derived from it — lives on the
card and is now read from it, leaving `imei_override` as the only identity
field a `pcsc_reader` line genuinely cannot obtain that way, because an IMEI
belongs to a device rather than a card.

### Upgrading — no action required

Nothing here needs a config edit. A `[[vowifi.line]]` that pins `mcc`/`mnc`
keeps working exactly as before; an explicit pair still wins over the derived
one. Removing them is now the better default, and the reason is below.

- **A `pcsc_reader` line no longer needs `mcc`/`mnc` configured.** v8.1.0
  required them "because there is no modem to derive them from", which was the
  wrong reason: both live on the card. The MCC is the first three IMSI digits,
  and whether the MNC is 2 or 3 digits long is stated by the card's own `EF_AD`
  (`6FAD`, TS 31.102 §4.2.18 byte 4) — the modem path had simply been reading
  that byte via `AT+CRSM`, and the derivation was never ported to
  `modules::usim::ApduTransport` when the rest of the SELECT/READ/AUTHENTICATE
  logic was. It is now: `usim::read_mnc_length` and
  `plmn::derive_plmn_from_card` work over either transport, `vowifi-plmn` takes
  `--pcsc-imsi <IMSI>` alongside `--modem <port>`, and both consumers
  (`supervise` for the ePDG FQDN, `vowifi-ims-agent` for the IMS realm) use the
  card path for a card-reader line. Existing configs that set `mcc`/`mnc`
  keep working unchanged — an explicit pair still wins — but **omitting them is
  now preferred**, since a hand-written pair with the wrong MNC length silently
  builds both the wrong ePDG FQDN and the wrong IMS realm. The one card that
  still needs them pinned is one whose `EF_AD` omits the MNC-length byte (some
  legacy 2G SIMs): the modem path falls back to the serving PLMN from
  `AT+COPS`, but a reader has no radio, so that line fails at startup with an
  error saying to set them explicitly.
- `imsi_override` remains mandatory on a `pcsc_reader` line, with its rationale
  corrected in the error message and docs: it is the reader-to-line binding key
  (which physical card this line owns must be known before any card session
  exists, and `eap-sim-pcsc` needs it in the rendered NAI), not a workaround
  for an unreadable IMSI — `PcscTransport::connect` reads `EF_IMSI` off every
  candidate reader. `imei_override` also stays optional and auto-generated; an
  IMEI is a device identity and genuinely is not on the card.

## v8.1.0

Repository maintenance, one structural fix, and a run of VoWiFi fixes found
by getting two lines up simultaneously for the first time. CLI help text and
every `*-shell-env` output were verified byte-identical across the refactors.

### Upgrading — one config change needs action

**If you set `pcscf_source_path` by hand, update it.** `[vowifi]`'s value is
now a *base* with the line index appended, so a config still saying
`/tmp/pcscf` produces `/tmp/pcscf-0`, `/tmp/pcscf-1`, ... and the literal
`/tmp/pcscf` is never written again. `[volte].pcscf_source_path` must
therefore name one specific line — it defaults to `/tmp/pcscf-0`. Both VoLTE
failure paths now say so explicitly rather than reporting a bare "no P-CSCF
available", so a stale setting diagnoses itself.

Deployments that never set either key need no edit. Nothing else requires
one: the per-line charon assets and the healthcheck start period below both
change under the container, not in your config.

### Multi-line VoWiFi now actually works

Two lines had never both established at once before, and four separate
defects were hiding behind that. All were found on real hardware (an EC200
with a Vodafone SIM plus an OmniKey PC/SC reader with a second), and both
numbers now register with their carriers and answer inbound calls.

- **Breaking (operational): one shared charon serves every line.** Each line
  used to spawn its own. charon's socket-default plugin sets `SO_REUSEADDR`
  but never `SO_REUSEPORT`, so N daemons in one network namespace all
  wildcard-bind `0.0.0.0:500`/`:4500` and exactly **one** receives every
  reply; the losers retransmit into the void and give up, which the carrier
  reports to callers as the line being *"switched off"*. Demonstrated rather
  than deduced: on one boot line 0 established and line 1 timed out, and on
  the very next restart of the same image the two swapped.

  Per-line isolation never depended on per-line processes — a CHILD_SA is
  bound to its line by the `if_id` in its own connection block and by the
  pre-created `tunN` XFRM interface in that line's netns, and the kernel keys
  SA lookup on `if_id` regardless of which daemon installed it. Connections
  are now named per line (`ims0`, `ims1`, ...); the template's
  `remote { id = ims }` stays literal, since that is a protocol identity the
  ePDG matches to select the IMS APN. Recovery stays scoped: the daemon is
  restarted only when genuinely dead (atomically, so two lines noticing the
  same death cannot restart it twice), and every other fault is repaired per
  connection, leaving other lines' tunnels up. Rendered assets move from
  `/etc/strongswan-line-N.conf`, `/tmp/charon-N.log`, `/var/run/charon-N.vici`
  and `/etc/swanctl/conf.d-N/` to single shared paths plus one connection file
  per line in `/etc/swanctl/conf.d/`.
- **MOBIKE is off.** charon defaults it on and, on a multi-homed host,
  promptly sends an INFORMATIONAL carrying `ADD_4_ADDR`/`ADD_6_ADDR`.
  Vodafone India's ePDG advertises `MOBIKE_SUP` and then never answers it, so
  charon retransmits five times, concludes the peer is dead, and tears down a
  completely healthy tunnel (observed: established 03:21:55, gone 03:24:45).
  MOBIKE exists so a *client* can survive its own address changing; a bridge
  on a fixed host can only ever lose by it.
- **The P-CSCF plugin config is generated, not shipped fixed.** The osmocom
  fork's plugin keys its `enable` block by *connection name*, so naming
  connections per line silently disabled it everywhere. The failure looks
  nothing like its cause: charon simply omits `PCSCF4`/`PCSCF6` from its
  `CPRQ`, no carrier ever returns a P-CSCF, and every line establishes a good
  tunnel then tears it down for lacking one — about every 30s, forever.
- **Agent A waits for a busy modem serial instead of failing.** A modem line's
  serial has two legitimate claimants — the usim bridge (while charon has the
  virtual card powered on for EAP-AKA) and the IMS-AKA registration — and
  `serialport` opens exclusively. Losing the race failed the agent outright and
  the supervisor rebuilt the whole IMS session five seconds later, over a
  conflict that clears in seconds. Neither side holds the port for long: the
  bridge drops it on Power Off, and the registration transport is dropped when
  the REGISTER exchange returns, so a bounded wait resolves it. (Verified on a
  live two-line deployment: with both lines registered, nothing held the port
  at all.)
- **Agent B retries a control-channel bind instead of giving up.** The veth it
  binds to only exists once that line's tunnel is up, so `EADDRNOTAVAIL` is a
  normal startup condition — but the bind was one-shot, permanently dropping
  whichever line was slower, with a single already-scrolled-past ERROR to say
  so. Lines come up in whatever order their carriers answer, so which line was
  lost varied run to run.
- **Breaking: `[vowifi].pcscf_source_path` is a base, not a file.** The line
  index is appended (`/tmp/pcscf-0`, `/tmp/pcscf-1`, ...). Each line's
  supervisor writes its own tunnel-assigned P-CSCF there and each line's Agent
  A reads it back, so one shared file had concurrently establishing lines
  overwriting each other — the loser registered against the *other* carrier's
  proxy, unreachable from its own netns, and crash-looped. Observed live
  holding an address belonging to neither line.

  `[volte].pcscf_source_path` consequently defaults to `/tmp/pcscf-0`, naming
  one VoWiFi line explicitly: with several lines there is no single "the"
  address to borrow. **If you set either key by hand, update it** — a config
  still saying `/tmp/pcscf` will now find nothing, and both VoLTE failure
  paths say so explicitly rather than reporting a bare "no P-CSCF available".
- **Fixed: a line could re-establish its tunnel every ~30s, forever.** The
  line's XFRM `if_id` stays claimed by leftover kernel state from an earlier
  container run — XFRM policies/states and leaked default-namespace veth ends
  that all outlive the container — so its tunnel interface cannot be
  recreated. The supervisor detected it missing, recreated it, failed, and
  tore down a healthy CHILD_SA to retry. Every step of the interface setup
  discarded its result, so nothing said why; the kernel had been answering
  `RTNETLINK answers: File exists` for a name that existed nowhere. The
  outcome is now checked and reported, with the remedy, and
  `docs/operations.md` documents the cleanup. Deliberately not automatic:
  flushing XFRM state would destroy an unrelated IPsec deployment sharing the
  host, and iproute2 offers no way to delete selectively by if_id.
- **Fixed: the container's `HEALTHCHECK` called working deployments
  unhealthy.** A line's P-CSCF is reachable only inside that line's ePDG
  tunnel namespace, and the probe ran in the default namespace, so it could
  never succeed. This was a regression in the bash-to-Rust port above: the
  original ran the probe under `ip netns exec`, and the port kept the
  namespace for the interface-address check directly above it while dropping
  it here. Entering a namespace in-process would need `setns` — the only
  `unsafe` this binary would contain — so the probe re-executes the binary
  under `ip netns exec` via a hidden `tcp-probe` subcommand, adding no
  runtime dependency.

- **The healthcheck's start period is 180s, not 15s.** A line is healthy only
  once its ePDG tunnel is up and has been assigned a P-CSCF, which is carrier
  round-trips, not process startup: measured 30-60s to establish, with one
  line churning ~2min before settling. The old 15s declared the container
  unhealthy at 105s, mid-startup. This only became visible once the probe
  above started reporting true state.

### Maintenance

- **The `CommandRunner` handle-lifecycle bug class is now a compile error.**
  The same defect shipped seven times across the supervision loops; none was
  caught by the 650+ mock-based tests covering that code. `ChildHandle` is no
  longer `Copy`, `wait` consumes it, a new `reap` replaces every
  `signal(Term); wait()` pair with signal-poll-escalate, and genuinely shared
  claims are `Arc<ChildHandle>`. A new `conformance` module asserts every
  handle invariant against **both** the mock and real runners, so the
  mock/real divergence that hid the bug fails in CI instead of in production.
- **`docker/healthcheck.sh` is now `gsm-sip-bridge healthcheck`** — the last
  orchestration left in bash after specs/021. Its per-line checks go through
  the same tested `CommandRunner` seam as the rest of `supervise`, with nine
  tests covering the cases the bash had none for (per-line fault reporting,
  the engine-specific tunnel interface that once made every strongswan
  container report unhealthy, and zero-lines degrading rather than failing).
  The image no longer needs `bash` arrays, `/dev/tcp`, or `eval` of a
  shell-env dump.
- **A shared `line` module** now holds what VoWiFi and VoLTE had each
  reinvented: candidate classification, stable card-id ordering, the
  `max_lines` cap, per-index resource derivation, and manifest read/write.
  `shift_ipv4` existed twice byte-identical; `volte::discovery` imported
  `FailedLine` *from `vowifi::discovery`* (the LTE path depending on the
  Wi-Fi path); and `modules::discovery` kept private copies of VoLTE's
  manifest path constants with a comment saying to keep them in sync by
  hand. All three are gone — `line` sits below both subsystems, so the
  layering dilemma is removed rather than documented.
- **Breaking: a VoLTE line's index-0 namespace and veth interfaces are now
  suffixed** (`volte0`, not `volte`), matching VoWiFi. VoLTE special-cased
  index 0 to keep the unindexed base for single-line back-compat, so the two
  subsystems had two rules for the same derivation and line 0 was the one
  line whose names could not be predicted from its index. Teardown reads the
  names back out of the manifest, so a restart picks up the new ones cleanly.
- **Line manifests carry a `schema_version` and refuse a mismatch.** They are
  contracts between processes that may be different builds (a rolling update,
  or a `volte-status` from another binary), and both previously used
  `#[serde(default)]` — so a renamed field deserialised to its default and a
  line came up with, say, an empty APN. That exact failure is documented in
  `volte::discovery` as having attached the network's default bearer instead
  of the IMS one, looking fully configured while the P-CSCF was unreachable.
- **Dependencies refreshed**: `rand` 0.8 -> 0.9 (`thread_rng`/`gen_range`
  renamed), `rusqlite` 0.32 -> 0.37, `toml` 0.8 -> 0.9, `cron` 0.12 -> 0.17,
  `socket2` 0.5 -> 0.6, `base64` 0.22 -> 0.23, `md-5` 0.10 -> 0.11, plus every
  semver-compatible update. `toml` 0.9 changed `str::parse::<Value>()` to
  parse a bare *value* rather than a document, so the config loader now uses
  `toml::from_str`. **`prometheus` deliberately stays on 0.13**: 0.14 changes
  `with_label_values` to take `&[&String]`, which is 30+ call sites of churn
  for no fix and no feature.
- **Fixed: pjlib aborted on every container shutdown.** `Endpoint::drop`
  called `pjsua_destroy()` without registering the calling thread, unlike
  every other method on the type. `Drop` runs on whoever owns the value at the
  end — a Tokio worker, once `pool_handle.abort()` tears the CardPool down —
  and pjlib refuses to be called from a thread it has never seen. The
  assertion fired *after* the clean-shutdown log lines and the exit code
  stayed 0, which is why it read as cosmetic noise. Thread registration is now
  applied at every pjlib entry point rather than reasoned about per caller.
  Verified on hardware: the assertion is gone and shutdown ends with a proper
  "SIP account unregistered".
- **`config.toml` is now parsed by serde**, replacing ~1400 lines of
  hand-written `toml::Value` walking with declarative structs. The file format
  is unchanged — every key, default and range is preserved field by field —
  but the parsed shape is now split from the runtime shape, which is what
  makes `deny_unknown_fields` safe: `VowifiConfig`/`VolteConfig` carry
  per-line *derived* fields (`netns`, `veth_*`, `strongswan_if_id`) that are
  not settings, and deriving strictness on them directly would have started
  *accepting* them as settable. Each section's key list is now generated by
  the same macro that generates its struct, so the parser, the docs test and
  serde cannot disagree. See
  [docs/migrating-config-to-strict-parsing.md](docs/migrating-config-to-strict-parsing.md).
- **Breaking: an unknown key in `config.toml` now fails startup** instead of
  emitting a `tracing::warn!` and continuing. A typo silently did nothing:
  `max_line = 2` (missing the `s`) left the real setting at its default, and
  the one WARN was buried in a container's modem-probing startup noise —
  often emitted before the configured log level had even been applied. In a
  system where a wrong value has produced a line that attaches to the wrong
  bearer and looks healthy while being unreachable, a setting the operator
  believes they wrote and the bridge silently ignored is not a warning. Every
  offending key is reported in one error, qualified by section, so several
  typos are learned in one run rather than one per restart.
- **New `tests/test_config_docs.rs`** asserts `docs/configuration.md` and
  `config.toml.example` actually cover what the parser accepts, in both
  directions. Now that an unknown key is fatal, an undocumented key is one an
  operator can only find by reading the source, and a stale key in the
  example would fail every fresh deployment on first start.
- **Breaking: the three VoLTE metrics using the pre-v5 `gsm_bridge_` prefix
  are renamed** to `gsm_sip_bridge_volte_registered` / `_pdn_up` /
  `_registrations_total`. Every other metric moved to `gsm_sip_bridge_` in
  v5; these three were added later and reintroduced the old prefix, so all 31
  metrics now share one. Update any dashboard or alert rule referencing them.
- **An I/O failure no longer reports as a configuration error.**
  `From<io::Error>` mapped *every* I/O error to `BridgeError::Config`, so a
  serial port that vanished mid-call, a refused socket, and an unwritable log
  all told the operator to check config.toml. There is now an `Io` variant
  that retains the source, so callers can match on `ErrorKind`.
- **Store migrations are a table** rather than a chain of near-identical
  `if version == "N"` blocks; adding one is a single entry instead of ~10
  lines with a hand-copied version number in the `UPDATE`.
- **~100 stale references to `docker/entrypoint.sh`** across the source and
  docs claimed it still supervises agents, creates veth pairs, and runs
  cleanup traps. It has been a 28-line exec shim since specs/021; they now
  point at the `supervise::` module that actually does the work, and
  `supervise/mod.rs` carries a table mapping each concern to its module.

- **The CLI handlers moved out of `src/main.rs` into
  `gsm_sip_bridge::commands`** (2099 lines → 28). A binary crate's items
  cannot be imported from `tests/`, so all 40 handlers — line resolution,
  call reporting, and the `*-shell-env` printers whose output
  `docker/healthcheck.sh` `eval`s — had no tests at all, for a purely
  structural reason. `main.rs` is now argument parsing, logging setup, and a
  dispatch call; the 24-arm `if let Some(Commands::X(..))` chain became a
  `match`, so a new subcommand that is not wired up is a build error rather
  than one that silently falls through and starts the daemon.
- **The three `*-shell-env` printers now return a `String`** rather than
  writing to stdout (`render_vowifi_shell_env`, `render_discover_shell_env`,
  `render_volte_discover_lines_shell_env`), which is what makes them
  assertable. Six new tests in `tests/test_shell_env_contracts.rs` pin the
  contract: array element counts matching `LINE_COUNT` (which
  `healthcheck.sh` indexes in a `seq` loop), every key still emitted when
  zero lines resolve, and `shell_quote` escaping against injection.
- **`make lint` now lints the whole workspace** (`cargo clippy --workspace
  --all-targets -- -D warnings`). It previously covered only the
  `gsm-sip-bridge` and `pjsua-safe` crates' default targets, so `amr-safe`,
  `amr-sys`, `pjsua-sys`, every integration test, and every `#[cfg(test)]`
  module were never linted — hiding ~15 warnings including genuinely dead
  test-support code. All of them are now fixed and the gate is clean.
- **`deny.toml` no longer hard-errors.** It used `[advisories]
  vulnerability`/`notice` and `[licenses] unlicensed`, all removed from
  cargo-deny and now rejected outright — so `cargo deny check` failed for
  anyone who had the tool installed, which `make lint`'s `if command -v`
  guard quietly hid. Rewritten against the current schema, and CI now
  installs cargo-deny so the dependency policy is actually enforced.
- **`make test` prefers `cargo nextest`** when installed, which applies the
  20s per-test timeout `.config/nextest.toml` has always described but
  nothing ever ran; falls back to `cargo test` otherwise. CI installs it.
- **Removed dead weight**: `docker/grafana/dashboards/gsm-bridge.json` (an
  orphaned 28-panel dashboard mounted nowhere — `docker-compose.yml` only
  mounts `grafana/provisioning` — and querying the pre-v5 `gsm_bridge_*`
  metric names that no longer exist), the entirely unused
  `gsm-sip-bridge/tests/common/` harnesses (`PtyHarness`, `PbxHarness`,
  `temp_store`, `null_alsa_device`) along with the 25 `mod common;`
  declarations that existed only to satisfy them, and the vestigial no-op
  `make test-bash` target.

```
docker pull ghcr.io/selvakn/gsm-sip-bridge:8.1.0
```

## v8.0.0

A VoWiFi line's SIM no longer needs a modem at all — it can sit directly in
a physical PC/SC smart-card reader instead.

- **PC/SC card-reader-backed VoWiFi lines** (`specs/023-omnikey-pcsc-vowifi`) — validated against a real OmniKey AG 3x21 reader — cover **both** halves of a line: the ePDG tunnel (strongSwan's `eap-sim-pcsc` talking to `pcscd` directly) and, new in this release, IMS-AKA SIP registration itself. Until now only the tunnel had a PC/SC path; `ims::register_session` (used by `vowifi-ims-agent`) talked to the SIM exclusively over a modem's `AT+CSIM`, so a genuinely card-reader-only deployment's tunnel came up but the line never registered or answered a call. A new `modules::usim::ApduTransport` trait generalizes the existing SELECT/READ RECORD/AUTHENTICATE logic over either transport (`AtCommander`'s `AT+CSIM` or the new `modules::pcsc_card::PcscTransport`), so both paths share one implementation. Opt in with `[[vowifi.line]] pcsc_reader = true` plus mandatory `imsi_override`/`mcc`/`mnc` (no modem to derive them from); coexists with modem-backed lines in the same deployment, sharing `[vowifi].max_lines`. Requires `[vowifi].tunnel_engine = "strongswan"` (the default) — the `swu` engine has no PC/SC support and refuses to start with a `pcsc_reader` line configured. See [docs/omnikey-pcsc-vowifi.md](docs/omnikey-pcsc-vowifi.md).
- **New [docs/supported-hardware.md](docs/supported-hardware.md)** — a compatibility matrix of every modem/reader model this project runs against (Quectel EC20, EC200/EC200U, and now the OmniKey AG 3x21 reader) crossed with the three call modes (circuit-switched, VoWiFi, VoLTE), distinguishing what's actually been live-tested from what the code merely doesn't prevent.
- No modem means no `AT+CGSN` IMEI either — a stable, Luhn-valid one (TS 23.003 Annex A) is auto-generated per line from its own IMSI unless `imei_override` is set explicitly.
- With more than one `pcsc_reader` line configured, each connects to *its own* physical reader — matched by reading each candidate reader's own `EF_IMSI` and comparing it to the line's configured IMSI (the same disambiguation `eap-sim-pcsc` already does for the tunnel side), with each candidate's probe held inside a PC/SC transaction so two lines' concurrent probes at startup can't interleave and corrupt each other's reads. Config now also rejects two `pcsc_reader` lines sharing the same `imsi_override` outright, since that would let both resolve to the same physical card while whatever SIM the other line actually meant went unused.
- **Fixed: `pcscd`'s virtual `vpcd` reader was required even in a deployment with no modem-backed lines at all**, so a genuinely card-reader-only setup failed to start on a virtual reader nothing would ever use. Now provisioned only when at least one modem-backed line exists.
- **Fixed: `READ RECORD` against a real PC/SC reader silently returned nothing.** `AT+CSIM` over a modem resolves a `Le=00` ("give me whatever's there") request transparently; a real PC/SC reader instead answers with `SW=6C1A` ("wrong length; here's the real one") and no data, which — undetected — made every `EF_DIR` record look empty and USIM AID discovery fail outright.
- **Live-verified end to end** with the modem physically removed (a genuinely card-reader-only deployment, not just mixed): ePDG tunnel established, IMS-AKA `REGISTER` got `200 OK`, the network's own `NOTIFY` confirmed an active registration for the MSISDN, and a real inbound call was signaled and dialed into the PBX. Earlier, in a mixed modem + card-reader deployment, `eap-sim-pcsc`'s reader/card discrimination was proven correct in production code — it found the live card but correctly refused to use it for the modem line's own, different IMSI.

```
docker pull ghcr.io/selvakn/gsm-sip-bridge:8.0.0
```

## v7.2.0

Discord alerts now cover every critical operational failure, not just
inbound SMS — plus four IMS reconnect bugs and a zombie-process leak found
while hardening the v7.1.0 supervise migration before it fully landed on
`main`.

- **Discord alerting generalized beyond SMS** (`specs/022-discord-critical-alerts`) — a new `[alerts]` config section covers five categories: SMS (existing), module/modem lifecycle failure (SIM absent/unreadable, discovery failure, AT-worker unresponsive), IMS/SIP registration loss, VoWiFi tunnel failure, and PBX missed calls. Registration-loss and tunnel-failure only alert once a condition survives a continuous 5-minute unhealthy streak (configurable), evaluated at real report-arrival time rather than at `/metrics` scrape time, and each sends a distinct recovery notice once healthy again. One shared default webhook, per-category enable/disable (SMS on by default, the four new categories off), and per-category webhook overrides. New `gsm_sip_bridge_critical_alerts_total{category,outcome}` and `gsm_sip_bridge_critical_event_active{category,module}` metrics with matching Grafana panels. Live-validated end to end against real EC20 + Airtel hardware.
- **Fixed: a failed Discord delivery could permanently suppress an incident.** Alert state now moves `Pending` → `Alerted` only on confirmed delivery, retrying on the next unhealthy report instead of the operator never being told.
- **Fixed: a line with no Prometheus scraper could miss its own alert transition entirely** — evaluation moved from the `/metrics` scrape handler into the report-ingestion path itself, so it runs on the real report cadence regardless of who scrapes.
- **Fixed: recovery from a "given up" module/modem slot could require a manual restart** — the retry-loop success path now clears stale given-up state the same way the rescan path already did; previously only one of the two recovery paths did.
- **Four IMS reconnect fixes**, found live-testing the `021-entrypoint-supervise-rust` migration after v7.1.0 was tagged: a PBX-initiated hangup whose BYE failed on a silently-dead carrier transport could leave the GSM leg connected forever (now reconnects and retries the BYE); the reconnect itself could then fail permanently by trying to rebind a port the dead socket still held open, or by rebinding the independent Gm server (now only the client-reader thread restarts), or by falling back to a plain connection that violated the already-installed IPsec policy.
- **Fixed: helper processes' `timeout` grandchildren leaked as zombies under the new Rust `supervise` PID-1 process** (~1 every 30s, from the idle-tunnel keepalive and healthcheck probe). `tini` now sits as PID 1 ahead of it, reaping orphans without racing `supervise`'s own tracked-child `wait()`.

```
docker pull ghcr.io/selvakn/gsm-sip-bridge:7.2.0
```

## v7.1.0

The container's own supervision logic moves from bash into the Rust binary itself, as a tested `gsm-sip-bridge supervise` subcommand.

- **`docker/entrypoint.sh` shrinks from ~1350 lines to a 28-line shim** (`specs/021-entrypoint-supervise-rust`) — a strangler-fig migration, done in independently-shippable phases: config/asset rendering (strongswan.conf, swanctl.conf, updown scripts, vpcd reader.conf) as pure, snapshot-tested Rust functions; container teardown as a typed, ordered `ShutdownPlan` replacing a trap over ~15 hand-tracked PID arrays; one generic per-line `LineSupervisor` state machine (over a `TunnelEngine` trait) replacing three duplicated bash supervision loops (strongswan, SWu, circuit-switched); and finally wiring all of it into the real `supervise` subcommand entrypoint.sh now just execs into.
- **Live-validated end to end on real hardware** — cold start, warm restart, forced degraded-state recovery (a killed `charon` process, a vanished tunnel interface, a broken vici connection), and clean shutdown, for both VoWiFi (strongSwan) and VoLTE (`bridge_inbound`) — not just unit-tested against mocks.
- **Fixed: a warm container restart could leave the tunnel silently, permanently broken.** If the kernel's XFRM interface for a line's tunnel vanished while strongSwan still believed the CHILD_SA was established, the old script recreated the interface before reinitiating; the port had detected the vanished interface but never actually recreated it, so the tunnel renegotiated a fresh IKE_SA every ~30 seconds forever without ever installing a working data path.
- **Fixed: recovering from a dead/broken charon could get permanently stuck after a kill.** The vici socket needs a moment to come up after charon respawns before `swanctl --load-all` can talk to it; missing that delay on the recovery path (present on the original cold-start path) let `--load-all` silently fail and every subsequent re-initiate fail with "CHILD_SA config not found" — invisible to the healthcheck, since it only checks whether the tunnel interface has an address, and a stale pre-kill address doesn't reveal it.
- **Fixed: a container shutdown could race an in-flight tunnel recovery or a line still starting up for the first time**, leaving a freshly (re)spawned charon/SWu-dialer process unsignaled and still running after the container was "done" shutting down. Every supervision loop that can spawn a long-lived process now coordinates with shutdown through a reader-writer lock, so no new process can be created once shutdown has begun, and shutdown can't proceed until every in-flight spawn has finished registering.
- **Fixed: real charon log output was never actually being read correctly** — a P-CSCF-extraction regex expected the marker at the start of a log line, but charon prefixes every line with a facility tag (`[CFG] `), so a freshly established, fully working tunnel was indistinguishable from one stuck without a P-CSCF, and got torn down and renegotiated every steady-state tick regardless of actually working.
- Fifteen real bugs total were found and fixed this way across the whole migration — none caught by the (extensive) mock-based unit test suite alone, each one found either by testing directly against the real EC20 + Airtel hardware or by an unusually thorough review cycle. Full write-up of every one, and the reasoning behind every judgment call made along the way, in `specs/021-entrypoint-supervise-rust/DECISIONS-LOG.md`.

## v7.0.0

The VoLTE release. Alongside the circuit-switched GSM bridge and the VoWiFi bridge, the system now performs **its own IMS registration and call bridging over the LTE data path** — a third inbound call path, on par with VoWiFi rather than a hand-off to the modem's own (often poor-quality) internal VoLTE audio.

- **Host-side VoLTE-to-SIP bridge** (`specs/015-volte-host-ims` through `017-volte-inbound-bridge`) — the bridge registers to the operator's IMS core over the modem's LTE *data* PDN using the same registration/IMS-AKA/Gm IPsec machinery the VoWiFi path already proved out (both now implement a shared `ImsTransport`), then answers and bridges inbound calls to the same SIP/PBX destination. Opt in with `[volte].enabled` + `[volte].bridge_inbound`; the modem's own internal VoLTE (`docs/ec20-volte-setup.md`) is unaffected when this is off, and `volte-discover`/`volte-register`/`volte-call`/`volte-status` remain available as standalone diagnostics without enabling call bridging at all.
- **Multi-modem VoLTE with per-line network isolation** (`specs/018-volte-multi-modem`, `specs/020-volte-line-netns`) — auto-discovers every SIM-ready LTE modem (bounded by `[volte].max_lines`, default 8) and runs each as its own line with its own persistent registration, sharing one PBX trunk registration — the same multi-line model VoWiFi uses. Each line's carrier-facing half now runs as its own process inside its own network namespace and veth pair (`volte-carrier-agent --line N`, supervised by `docker/entrypoint.sh`), so one line's SIP/RTP can never egress on another line's LTE interface.
- **SMS over VoLTE**, plus a store-schema fix to actually persist it — incoming SMS on a VoLTE line is read from modem storage and recorded like the other paths. Schema bumped to v4: v3's `CHECK (transport IN ('cs','vowifi'))` silently rejected every VoLTE call/SMS row; existing databases migrate automatically.
- **New `transport="volte"`** everywhere `cs`/`vowifi` already appear (`gsm_sip_bridge_active_calls`, the `calls`/`sms` tables), plus VoLTE-specific gauges: `gsm_bridge_volte_registered`, `gsm_bridge_volte_pdn_up`, `gsm_bridge_volte_registrations_total{outcome}`.
- **Fixed: double PBX trunk registration.** The circuit-switched daemon, VoWiFi's outbound leg, and VoLTE's inbound bridge could all try to register the same `[sip]` account when more than one path was enabled, and the loser churned the PBX with a REGISTER 408 loop forever (observed live). Registration is now confirmed against live PJSUA state instead of assumed on send, and exactly one path owns the trunk registration at a time.
- **Fixed: multi-card VoWiFi** — five bugs latent since `specs/013-multi-card-vowifi`, found running two concurrent lines for real for the first time: a hardcoded charon pidfile guard that blocked every line but the first from starting, a hardcoded XFRM `if_id` that bound every line's CHILD_SA to line 0's interface, an updown hook that silently fell back to line 0's namespace/interface name when its per-line environment didn't propagate, incomplete circuit-switched exclusion of a role-assigned-but-unresolved modem, and an IMEI reader that could return an `AT+CGSN` command echo instead of the modem's real IMEI. Verified against two real SIMs on two carriers, concurrent registration and calls.
- **Breaking: config restructuring** — `[audio]` split into `[audio]` (profile/vad/
  latency, shared by every call path) and `[modem_audio]` (rx_gain/eec_mode/
  tx_level/rt_audio_prio, circuit-switched USB audio only — VoWiFi/VoLTE never
  touched these). `[vowifi]`/`[volte]` top-level sections now hold only fields
  genuinely global across every line; per-line settings (mcc/mnc/modem matcher/
  imsi_override for VoWiFi, modem matcher/cid/apn/pcscf/iface/msisdn for VoLTE)
  live only in `[[vowifi.line]]`/`[[volte.line]]` now, each with a sane default
  when omitted. Pure per-line infrastructure that was always mechanically derived
  (veth names/addresses, the ePDG netns name, the strongswan XFRM interface name/
  id) is no longer configurable at all. `[volte].use_tcp`/`.sec_agree` are removed
  outright — they were parsed but never actually consumed by `volte::bridge`
  (already hard-coded to `true`). Review `config.toml.example` and
  `docs/migrating-config-reorg.md` when upgrading.
- **Documentation** — README, `docs/architecture.md`, and the docs index now cover all three call flows (CS, VoWiFi, VoLTE) end-to-end, including the VoLTE call-flow diagram and the distinction from the modem-internal VoLTE setting.

```
docker pull ghcr.io/selvakn/gsm-sip-bridge:7.0.0
```

## v6.3.0

Multi-card VoWiFi, and Grafana call/SMS metrics restored on that path.

- **VoWiFi now supports multiple SIMs concurrently** (`specs/013-multi-card-vowifi`), matching the capability the circuit-switched USB-audio path has had since feature 004. Attach several VoWiFi-capable modems and the system auto-discovers each one's AT-command interface, runs one ePDG tunnel/IMS registration per SIM (a "line"), and bridges inbound calls and SMS from all of them concurrently — no more hand-typing a single serial port in config. A new `discover` CLI subcommand scans once and writes the resolved line list; `vowifi-ims-agent` takes a `--line N` flag to run a specific one. `[vowifi].max_lines` caps how many the scanner will bring up (default 8), and `[[vowifi.line]]` entries let an operator pin or override individual lines. If VoWiFi is enabled but discovery finds no usable modem, the subsystem now degrades and logs loudly instead of crash-looping the whole container.
- **Grafana's call and SMS panels stopped updating for VoWiFi traffic when the v6.0.0 split moved calls onto two separate agent processes** (`specs/014-vowifi-metrics-restore`) — only the main daemon's registry was ever scraped, and neither agent exported metrics or wrote to the `calls`/`sms` tables, so VoWiFi activity was invisible to both Grafana and sqlite-web call history even though the bridge itself worked. Both agents now forward call/SMS/registration events over the existing control socket to the daemon's single Prometheus registry and SQLite store, tagged per-line via the same card identifier `discover` assigns. Existing circuit-switched metrics are unchanged in value, gaining only a `transport="cs"|"vowifi"` label. New: per-line IMS registration and ePDG tunnel-state gauges, bridge-failure-reason counters (ring timeout, PBX decline, caller cancel, agent unreachable), and an `agent_up`/`agent_last_report_seconds` liveness pair so a crashed or silent agent is visible before its next supervised restart, without ever double-counting across a restart.

```
docker pull ghcr.io/selvakn/gsm-sip-bridge:6.3.0
```

## v6.2.0

VoWiFi on a VoLTE-capable modem, and two failures that made it look like the SIM or the modem was at fault.

- **The modem's own IMS/VoLTE stack is now disabled while VoWiFi is enabled.** Our `REGISTER` carries `+sip.instance="<urn:gsma:imei:$IMEI>"` — the modem's own IMEI — so a VoLTE-registered modem claims the same IMPU with the same instance-id. Per RFC 5626 the network does not see two devices: it treats whichever registration arrives second as a re-registration of the first and deactivates the older binding. Against Airtel the tunnel came up, EAP-AKA authenticated and `REGISTER` returned `200 OK` — then ~0.7s later a reg-event `NOTIFY` arrived carrying `state="terminated" event="deactivated" reason=noresource` for our own contact, the modem's VoLTE registration won, and no terminating call could ever reach the bridge. The entrypoint now reconciles `AT+QCFG="ims"` against `[vowifi].enabled` on boot (2 = forcibly disabled for VoWiFi, 1 = forcibly enabled otherwise, so VoLTE keeps working when the bridge is off), rebooting the module only when it is in the wrong mode. **Note:** the modem persists this setting across power cycles, and correcting it costs one ~30s module reboot on the boot that fixes it. With VoLTE off, circuit-switched calls fall back to 2G/3G via CSFB.
- **vpcd's port moved off the kernel's ephemeral range** (35963 → 15963, `[vowifi].vpcd_port`). vsmartcard's upstream default sits inside `net.ipv4.ip_local_port_range` (32768-60999), and under `network_mode: host` the container shares that namespace — so an unrelated outbound connection can already hold the port when pcscd starts. The driver's `bind()` then fails with `EADDRINUSE`, the virtual reader is never registered, and VoWiFi dies with two symptoms that name neither the port nor each other: charon reports `no smart card reader` and `vowifi-usim-bridge` spins forever on `Connection refused`. Found in the field with a redis client parked on 35964. The port is now rendered into `/etc/reader.conf.d/vpcd` from config, so the driver's listener and the bridge's dial target always agree.
- **The entrypoint fails loudly when the vpcd reader does not come up**, and pcscd's log is surfaced in `docker logs` as `[pcscd]`, instead of leaving an unexplained reconnect loop.
- **Docs:** `network_mode: host` is *not* what grants USB/ALSA access (that is `privileged` + the `/dev` mount) — a false rationale that hid the port collision above. New `operations.md` entries for the two failures above, and for the host-firewall rule VoWiFi needs when running with host networking (a default-deny ufw drops Agent A's control channel and RTP across the veth, so calls arrive and then fail).

```
docker pull ghcr.io/selvakn/gsm-sip-bridge:6.2.0
```

## v6.1.0

Vodafone India VoWiFi interop, plus automatic MCC/MNC.

- **VoWiFi now works on Vodafone India** — two independent fixes were needed to get from "tunnel won't establish" to a stable `200 OK` registration:
  - The strongSwan `ims` connection now pins its IKE and CHILD_SA proposals to the classic 3GPP baseline (matching the known-good SWu dialer). charon's default proposal set made the `IKE_SA_INIT` request 852 bytes, which Vodafone's ePDG rejected outright with `INVALID_SYNTAX` before any SIM interaction happened.
  - The `Security-Client` header now offers `ealg=aes-cbc` (encrypted Gm IPsec) alongside the existing `ealg=null`. Vodafone's P-CSCF blanket-refuses integrity-only offers with an instant `403 Forbidden` and no challenge — a response so uninformative it initially looked like a network-side subscriber block. `gm_ipsec.rs` already implemented AES-CBC end-to-end; it had just never been exercised against a network that requires it.
  - See `docs/vowifi-epdg-research-notes.md` for the full bisection story.
- **Automatic MCC/MNC** — `vowifi.mcc`/`vowifi.mnc` are now optional. Left unset, they're derived from the SIM at startup: MCC from the IMSI (`AT+CIMI`), and the MNC's 2-vs-3-digit length from the SIM's EF_AD file (`AT+CRSM`), falling back to the registered PLMN (`AT+COPS`) when EF_AD is unreadable. Explicit config values still take precedence.

```
docker pull ghcr.io/selvakn/gsm-sip-bridge:6.1.0
```

## v6.0.0

The VoWiFi release. Alongside the existing circuit-switched GSM bridge, the system now answers calls the carrier delivers over Wi-Fi Calling (VoWiFi/IMS) and bridges them to the same SIP/PBX destination — the carrier decides which path delivers a given call. Built on the foundation work of the [Osmocom foss-ims-client project](https://osmocom.org/projects/foss-ims-client/wiki/VoWiFi_with_Asterisk).

- **Inbound VoWiFi-to-SIP bridge** (`specs/011-vowifi-sip-bridge`) — an IKEv2/IPsec ePDG tunnel to the carrier, IMS-AKA registration with real Gm IPsec (kernel XFRM), and two supervised agent processes (one inside the tunnel's `ims` network namespace, one PBX-facing) joined by a veth pair. Enabled via the new `[vowifi]` section in `config.toml`; disabled by default. Live-validated end-to-end against Airtel India. See `docs/vowifi-bridge.md`.
- **Wideband audio end-to-end** — a carrier's AMR-WB (16 kHz) call stays wideband all the way to the PBX (AMR-WB → L16/16000 over the veth link → G.722), instead of narrowing to 8 kHz. Narrowband carriers (PCMU/AMR-NB) bridge exactly as before.
- **strongSwan ePDG engine, now the default** (`specs/012-strongswan-epdg`) — proper IKE rekeying, re-authentication, and dead-peer detection; the network namespace and veth link survive reconnects. Includes a vpcd/pcscd USIM bridge (`vowifi-usim-bridge`) that runs EAP-AKA against the SIM inside the modem via `AT+CSIM`, with no physical smart-card reader. The original SWu dialer remains available as `tunnel_engine = "swu"`.
- **SMS over VoWiFi** — SMS delivered via IMS is captured and forwarded to Discord like modem SMS.
- **Breaking: config consolidation** — all non-secret settings moved from `.env`/environment variables into `config.toml` (`MCC`/`MNC`/`APN`/`TUNNEL_ENGINE`/veth names/keepalive → `[vowifi]`; log level → `[logging].level`). `.env` now holds secrets only. Review `config.toml.example` when upgrading.
- **Breaking: Alpine/musl image** — the Docker image was rebuilt on Alpine, dropping from 629 MB to ~116 MB, and the CS-GSM and VoWiFi/ePDG images were unified into one (`docker compose up --build` from `docker/` runs both paths).
- **EC200 support** — USB discovery now recognizes the Quectel EC200 series alongside the EC20.
- **Documentation overhaul** — restructured README, a docs index (`docs/README.md`), and new architecture, hardware-setup, observability, and development guides.

```
docker pull ghcr.io/selvakn/gsm-sip-bridge:6.0.0
```

## v5.6.4

- **Fix: timezone support in Alpine container** — Alpine's musl libc requires the `tzdata` package to read timezone information from `/usr/share/zoneinfo`. Without it, the `TZ` environment variable has no effect and the container reports all times in UTC, making logs hard to correlate with local events. Added `tzdata` to the runtime stage so `TZ=Asia/Kolkata` (or any other timezone in `.env`) now correctly converts timestamps.

```
docker pull ghcr.io/selvakn/gsm-sip-bridge:5.6.4
```

## v5.6.3

- **Fix: module permanently stuck after scheduled restart** — When the modem's `AT+CFUN=1,1` reboot caused a two-phase USB re-enumeration, the `NetworkLost` event would transition the slot to `Recovering` without setting `next_retry_at`. The retry loop requires a non-None `next_retry_at` to fire, so the slot was permanently invisible to recovery — staying stuck in `Recovering` with no worker and no scheduled retry. All subsequent hourly scheduled restart cycles skipped the slot (non-Ready), requiring a manual container restart to recover. Fix: `NetworkLost` now resets `retry_count = 0` and sets `next_retry_at` with the configured initial backoff, matching the behavior of all other `Recovering` transitions.

```
docker pull ghcr.io/selvakn/gsm-sip-bridge:5.6.3
```

## v5.6.2

Makes the `rt_audio_prio` real-time scheduling from v5.6.1 actually take effect (it was a no-op on the musl release binary).

- **Fix: RT scheduling was a no-op on musl** -- musl's `sched_setscheduler()` libc wrapper is a stub that always returns `ENOSYS`, so the promotion silently failed (`errno=38`). Now invokes the `sched_setscheduler` syscall directly, which works on both glibc and musl.
- **Fix: targeted the wrong threads** -- promotion looked for a thread named `media`, but the threads that actually drive ALSA I/O are `alsasound_captu` (capture / GSM→SIP) and `alsasound_playb` (playback). Now prefix-matches `alsasound`, `media`, and `clock`, so the capture thread that matters for overruns is promoted. Log wording also distinguishes "no thread matched" from "matched but promotion failed".

```
docker pull ghcr.io/selvakn/gsm-sip-bridge:5.6.2
```

## v5.6.1

Same scope as the v5.6.0 tag, which failed to publish (musl build error in the new
real-time scheduling code); v5.6.1 is the first image-producing release of this work.

Audio-quality release targeting the noisy/choppy GSM-leg audio traced to ALSA capture-layer corruption (XRUNs, frozen/repeated frames) on the EC20 USB-audio path — not network noise, so gain/echo tuning could not fix it.

- **Larger, configurable ALSA sound-device buffers** -- New `[audio] snd_rec_latency_ms` and `snd_play_latency_ms` keys (range 20–2000, default 150 ms vs PJSUA's 100/140) size the capture/playback ring buffers, absorbing scheduling jitter that caused XRUNs. Raise these if the logs report `alsa_capture_overrun` / `alsa_playback_underrun`.
- **Real-time audio thread scheduling** -- New `[audio] rt_audio_prio` key (0 = off, 1–99 = `SCHED_FIFO` priority) promotes PJMEDIA's `media` sound-device thread to real-time once a call's audio device opens, so the ALSA buffer is serviced ahead of best-effort work. Requires `CAP_SYS_NICE` (privileged container); best-effort and logged, never fails the call.
- **XRUN visibility** -- PJMEDIA overrun/underrun log lines are now detected, counted, and surfaced as structured `WARN` events (`kind`, `direction`, running `total`) for log-based alerting.
- **Native sample-rate verification** -- On call setup the EC20 capture device is probed and a `WARN` is logged if it cannot run natively at PJMEDIA's 8 kHz clock (silent resampling injects high-frequency artefacts on the GSM leg).

```
docker pull ghcr.io/selvakn/gsm-sip-bridge:5.6.1
```

## v5.5.3

- **Fix: AT+QRXGAIN range corrected to 0–65535** -- Per the Quectel EC20 AT manual, `<rxgain>` is a 16-bit downlink digital gain value (0–65535), not 0–100. The config key `rx_gain` now accepts the full range as a `u32`. Typical tuning value: `rx_gain = 35000`.

## v5.5.2

- **Fix: SIP→GSM audio muted by AT+QRXGAIN** -- v5.5.1 incorrectly sent `AT+QRXGAIN=50` unconditionally during module init. `AT+QRXGAIN` controls the earpiece/playback gain (SIP→GSM direction), not the receive-from-network direction. Setting it to 50 overrode the modem's firmware default (~80–100), near-muting what the GSM caller hears from SIP. The command is now only sent when `rx_gain` is explicitly set in `config.toml`; the modem firmware default is left untouched otherwise.

## v5.5.1

- **GSM Receive Gain Control** -- New `[audio] rx_gain` key (integer 0–100, default 50) sends `AT+QRXGAIN=<val>` to the EC20 modem during module init. Controls the hardware gain on audio arriving from the GSM network before it reaches the ALSA interface — i.e. how loud the remote GSM caller sounds on the SIP side. Lower this if the GSM audio sounds too loud or distorted.
- **SIP Conference Bridge Gain** -- New `[audio] tx_level` key (float 0.0–2.0, default 1.0) applies a software gain on the GSM→SIP path via `pjsua_conf_adjust_tx_level` on every call start. 1.0 = unity, 0.7 ≈ −3 dB, 0.5 ≈ −6 dB. Use `rx_gain` first (hardware attenuation); `tx_level` is a post-ALSA digital trim.

## v5.5.0

- **Scheduled Card Auto-Restart** -- Cards are now automatically restarted via `AT+CFUN=1,1` on a configurable cron schedule (default: `0 1 * * *`, 1 AM nightly). Restarts happen one card at a time in slot order. A random jitter is applied to the start time and to the gap between cards to avoid synchronised reboots. Cards with active calls are deferred and retried once after all other cards have been processed. Manual restarts during a scheduled cycle are serialised to prevent double-restarts. Adds `gsm_scheduled_restart_total{slot, outcome}` Prometheus counter for observability.

  Configure via `config.toml`:
  ```toml
  [scheduled_restart]
  enabled           = true
  cron              = "0 1 * * *"
  start_jitter_secs = 300
  gap_secs          = 30
  gap_jitter_secs   = 15
  ```

## v5.3.1

- **Fix SIGABRT on Call Start** -- Audio monitor thread called `pjsua_conf_get_signal_level` without registering with pjlib, triggering the `pj_thread_this` assertion and crashing with exit code 139. Fixed by calling `ensure_pjsip_thread()` at the start of the spawned thread.

## v5.3.0

- **Card Restart Reboots Modem** -- `card restart` now issues `AT+CFUN=1,1` to perform a hardware modem reboot before re-initializing. Re-initialization is delayed 10 seconds to allow the EC20 to fully boot. Previously only the software state was reset without touching the modem hardware.
- **Audio Level Logging at Call End** -- At the end of every bridged call, logs per-direction signal levels sampled once per second via `pjsua_conf_get_signal_level`. Fields `gsm_to_sip_avg`, `sip_to_gsm_avg`, `gsm_to_sip_total`, and `sip_to_gsm_total` (scale 0=silence, 255=max) appear in the call-end log line to help diagnose no-audio issues.

## v5.2.0

- **Fix Repeated Discovery Log** -- `discovered EC20 module` was logged at INFO every 5 seconds for already-managed modules due to the hotplug rescan. Downgraded to DEBUG; startup visibility is provided by `module initialized` and new hotplug cards by `new module detected`.
- **Hotplug Rescan Interval** -- Increased USB rescan interval from 5 seconds to 60 seconds. Hot-plugging cards is rare and the frequent scan was unnecessary.
- **`--config` Optional for Card Commands** -- `gsm-sip-bridge card <subcommand>` no longer requires `--config`. clap 4.6 did not accept an empty-string default for `PathBuf`, causing a spurious error. The argument is now `Option<PathBuf>`; card commands fall back to the default socket path (`/tmp/gsm-sip-bridge.sock`) when omitted.

## v5.1.0

- **Auto-Recovery** -- Cards automatically reload on USB disconnect or network loss with exponential backoff and per-slot give-up tracking (IMEI-keyed persistence).
- **Startup Diagnostics** -- Phone number and network type logged per card at startup.
- **Unix Socket Control API** -- On-demand daemon management via Unix socket.
- **CLI Card Subcommands** -- `card restart`, `card set-mode`, `card get-mode`, `card list` for runtime card management.
- **SQLite Schema v2** -- `card_slots` and `card_mode_prefs` tables with automatic v1→v2 migration.
- **Network Mode Preferences** -- 2G/4G preferences persisted and re-applied on card initialization.

## v5.0.4

- **gsm-echo ALSA Audio Loopback** -- Added real ALSA capture/playback to `gsm-echo`. Previously, `AT+QPCMV=1,2` routed audio to USB but nothing read or wrote the ALSA device, resulting in silence. Now spawns a dedicated loopback thread (8kHz, S16_LE, mono) on call answer and stops it on hangup, with overrun/underrun recovery.
- **VoLTE Detection** -- `gsm-echo` now queries `AT+QNWINFO` on each incoming call and logs `volte=true/false` based on whether the active RAT is LTE.
- **Docker Build DNS Fix** -- Added `network: host` to docker-compose build config to resolve BuildKit DNS failures reaching package mirrors.
- **EC20 VoLTE Setup Guide** -- Added `docs/ec20-volte-setup.md` documenting the procedure to enable VoLTE on the EC20 module (deactivate MBN profile, force IMS, LTE-only mode).

## v5.0.3

- **Fix Missing USB Audio Routing** -- Added `AT+QPCMV=1,2` to module initialization, routing voice audio through the USB Audio Class interface. Without this command, audio went to the EC20's analog PCM pins instead of the USB ALSA device, resulting in silence on both GSM echo and SIP-bridged calls.
- **Wire gsm-echo Debug Binary** -- Replaced the placeholder stub with a working implementation that auto-discovers an EC20 module (or accepts `--serial`/`--audio` overrides), configures AT commands, and monitors for incoming calls with auto-answer and call lifecycle logging.
- **Wire sip-echo Debug Binary** -- Replaced the placeholder stub with a working implementation that loads config, registers with the SIP PBX, and waits for incoming calls with graceful shutdown via SIGINT/SIGTERM.

## v5.0.2

- **Docker Image Size Reduction** -- Migrated to Alpine-based runtime with static PJSIP linking. Image reduced from 129MB to 25MB (81% smaller). Uses a 4-stage build: PJSIP static on Alpine, bindgen on Debian, Rust build on Alpine, minimal Alpine runtime.
- **Static PJSIP Linking** -- All PJSIP libraries statically compiled into the binary; no `.so` files needed at runtime. Added `PJSUA_SYS_BINDINGS` and `PJSUA_SYS_STATIC` env vars to `pjsua-sys` build script for pre-generated bindings and static link control.
- **Call Stability Fix** -- Fixed stale `SIP_PEER_DISCONNECTED` flag causing subsequent calls to immediately hang up. The flag from a previous call's BYE was not consumed when the module was in Idle state.
- **Audio Quality Tuning** -- Disabled echo cancellation (`ec_tail_len=0`), set max quality, explicit 20ms ptime, and auto jitter buffer for improved audio on musl runtime.
- **Removed `alsa` Crate** -- Dropped unused direct ALSA dependency from `gsm-sip-bridge`.
- **Release Binary Optimization** -- Added `strip=true` and `lto="thin"` to workspace release profile.
- **Healthcheck** -- Switched from `curl` to `wget` in both Dockerfile and docker-compose.

## v5.0.1

- **Ringback Tone Fix** -- The tonegen was playing the 400 Hz ringback only once instead of looping. Now uses `PJMEDIA_TONEGEN_LOOP` so the GSM caller hears continuous ringing until the SIP extension answers.
- **Uptime Metric Fix** -- `gsm_sip_bridge_uptime_seconds` was defined but never set. Now computed on each Prometheus scrape.
- **Call Duration Histogram Fix** -- `gsm_sip_bridge_call_duration_seconds` was never observed. Now recorded at end of each call.
- **SIP Call Rate Metric Fix** -- `gsm_sip_bridge_sip_calls_total` was never incremented. Now tracks initiated/error outcomes.
- **Audio Errors Metric Fix** -- `gsm_sip_bridge_audio_errors_total` was never incremented. Now tracks sound device failures.
- **README Refresh** -- Full rewrite with Mermaid diagrams, TOML config examples, and architecture documentation.
- **Grafana Dashboard Screenshot** -- Added fresh capture from the running instance.

## v5.0.0

- **Complete Rust Rewrite** -- Replaced the C++17 implementation with a Rust workspace for memory safety, eliminating all manual memory management.
- **Three-Crate Architecture** -- `pjsua-sys` (bindgen FFI), `pjsua-safe` (safe wrappers with `// SAFETY:` comments), `gsm-sip-bridge` (zero `unsafe` binary).
- **Async Runtime** -- Tokio-based event loop with `crossbeam_channel` for the DB writer thread.
- **TOML Configuration** -- Replaced INI format with TOML; secrets support `env:VAR_NAME` syntax.
- **DID Passthrough via Headers** -- Outbound SIP INVITE carries `P-Asserted-Identity` and `X-GSM-Caller-ID` headers; leading `+` stripped from request URI.
- **PJSIP Conference Bridge Audio** -- Bidirectional audio via `pjsua_conf_connect` in `on_call_media_state` callback; ALSA device matched by card name from `/proc/asound/`.
- **SMS Text Mode** -- Switched from PDU to text mode (`AT+CMGF=1`) for simpler parsing and more reliable extraction.
- **SQLite Store Thread** -- Dedicated writer thread with `StoreCommand` enum; WAL mode for concurrent access.
- **Discord SMS Forwarding** -- Async webhook posting with DB status tracking (`pending`/`sent`/`failed`).
- **Multi-Arch Docker Image** -- Published to GHCR for linux/amd64 and linux/arm64.
- **CI Pipeline** -- GitHub Actions with clippy, rustfmt, cargo-deny, and full test suite.
- **Prometheus Metrics** -- All v4.x metrics carried forward with `gsm_sip_bridge_` prefix, plus new `store_writes_total`, `store_queue_depth`, and `build_info`.
- **Thread Registration** -- All PJSIP API calls preceded by `pj_thread_register()` to prevent assertion crashes from async threads.
- **Graceful Shutdown** -- SIGTERM/SIGINT handling with proper PJSIP cleanup and DB flush.

## v4.1.1

- **SIP Registration Retry** -- PJSIP now automatically retries registration after 5 minutes when the server rejects with a permanent failure (e.g. 403 Forbidden), preventing the bridge from silently going offline.
- **Database Rename** -- SMS and call database renamed from `sms.db` to `data.db` to reflect its broader scope; update `db_path` in `config.ini` if overridden.
- **sqlite-web Browser** -- Docker Compose stack now includes an optional read-only web UI (`sqlite-web`) for browsing call and SMS records at `http://localhost:8088`.

## v4.1.0

- **Call Logging** -- Every incoming GSM call is recorded in a local SQLite database with caller ID, module ID, timestamp, duration, SIP destination, and outcome (answered/missed/failed).
- **SMS Persistence** -- All received SMS messages are stored in SQLite with sender, body, timestamp, module, and Discord forwarding status, surviving restarts and Discord outages.
- **sqlite-web UI** -- Docker Compose stack now includes a read-only web interface for browsing call and SMS records at `http://localhost:8088`.

## v4.0.0

- **SMS-to-Discord Forwarding** -- Captures incoming SMS from all modules, persists to a local SQLite database, and posts rich embed notifications to a configurable Discord webhook.
- **SMS Monitoring** -- Independent SMS polling on all modules via AT commands (`AT+CMGL`), with automatic SIM cleanup after read.
- **Configurable via `[sms]` section** -- Enable/disable SMS, set Discord webhook URL, and configure database path in `config.ini`.

## v3.0.1

- **Build Performance** -- PJSIP Docker build layer is now cached across branches and tags, significantly reducing CI build times.
- **CMake FetchContent** -- Replaced vendored mINI header with CMake FetchContent for cleaner dependency management.
- **License** -- Added GNU GPL v3 license.

## v3.0.0

- **Prometheus Metrics** -- Exposes call counts, SIP registration state, module health, audio errors, and call duration histograms on a `/metrics` endpoint (default port 9091).
- **Grafana Dashboard** -- Ships a pre-provisioned dashboard with panels for system overview, call rates, active calls, duration percentiles, module health, and error rates.
- **Docker Compose Monitoring Stack** -- One-command deployment of the bridge with Prometheus and Grafana in host network mode.

## v2.0.0

- **Multi-Card Support** -- Detects all connected EC20 modules at startup, assigns stable hardware IDs derived from USB serial numbers, and handles concurrent calls across modules independently.
- **Automatic Module Recovery** -- Failed modules (SIM issues, serial errors) are retried every 30 seconds and rejoin the active pool when functional.
- **Single-Card Override** -- Explicit `--serial` and `--audio` flags bypass auto-detection for single-module setups.

## v1.1.0

- **DID Passthrough** -- `sip_destination` is now optional. When empty, the GSM caller's number is used as the SIP DID, letting the PBX inbound route decide the destination extension.
- **SIP Media Renegotiation Fix** -- Audio bridge now reconnects correctly after SIP re-INVITE (media hold/resume scenarios).
- **SIP TCP Transport Fix** -- Fixed connection type when using TCP transport.

## v1.0.0

- **GSM-to-SIP Call Bridging** -- Auto-answers incoming GSM calls on a Quectel EC20 module and bridges audio bidirectionally to a SIP extension via a PBX.
- **SIP Audio Echo** -- Standalone SIP echo server for testing (echoes audio back to caller).
- **GSM Audio Echo** -- Standalone GSM echo tool for hardware validation (echoes modem audio back to caller).
- **Caller ID Forwarding** -- GSM caller's number is forwarded to SIP via P-Asserted-Identity header for DID routing.
- **Lock-Free Audio Pipeline** -- SPSC ring buffers connect ALSA capture/playback to the PJSIP conference bridge with minimal latency.
- **USB Auto-Discovery** -- Detects EC20 modules by scanning the USB bus for vendor/product ID `2c7c:0125`.
- **Docker + CI** -- Multi-platform Docker image (amd64/arm64) with GitHub Actions CI pipeline.
