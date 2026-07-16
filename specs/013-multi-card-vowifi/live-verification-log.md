# Live Verification Log: Multi-Card VoWiFi (single card, real hardware)

Status as of this session: **single-line path fully verified against real hardware and a real
carrier ePDG.** No second VoWiFi-capable modem was available, so true multi-line concurrency
(SC-004/SC-005) is not yet exercised — that's the next session's starting point. This log records
what was tested, four real bugs the testing surfaced (all fixed and re-verified), and what's left.

## Test hardware

- One EC200-class modem (vendor `2c7c`, product `0901` — audio-less, matches `KNOWN_DEVICES`),
  attached over USB, exposing `ttyUSB0`–`ttyUSB6`.
- Real SIM: home network MCC/MNC `404`/`094` (Vodafone Idea/Vi India). IMSI/MSISDN redacted from
  this log (present in operator-held test notes, not committed here).
- Deployment: local Docker host, `docker/docker-compose.yml`'s `gsm-sip-bridge` service,
  `[vowifi].tunnel_engine = "strongswan"`, image built from this branch.

## What was verified end-to-end

1. **Discovery** (`gsm-sip-bridge discover`): real USB topology walk found the modem, matched it
   against `KNOWN_DEVICES` (audio-less → correctly defaulted to the VoWiFi role), AT-probed across
   its serial interfaces (found both `ttyUSB0` and `ttyUSB6` answer AT — see bug 2 below), read a
   real SIM (IMSI, MCC/MNC derivation), and derived a correct N=1 line (unindexed `netns=ims`,
   `strongswan_tun_iface=tun23`, `pcscf_source_path=/tmp/pcscf`, etc. — FR-020 held on real output,
   not just in unit tests).
2. **strongSwan tunnel**: real IKE_SA_INIT/IKE_AUTH against Vodafone India's ePDG
   (`epdg.epc.mnc094.mcc404.pub.3gppnetwork.org`), real EAP-AKA over the shared pcscd/vpcd bridge
   (`vowifi-usim-bridge` forwarding `AT+CSIM` to the SIM), CHILD_SA established, P-CSCF assigned.
3. **IMS registration** (Agent A / `vowifi-ims-agent --line 0`): real Gm IPsec setup, real
   `REGISTER` → `200 OK`, real reg-event `NOTIFY` showing the AOR `active` for both the SIP and
   `tel:` URIs.
4. **Agent B** (`vowifi-sip-agent`): registered to the PBX, resolved the one line from the
   `discover` output, opened its per-line control-channel listener.
5. **Stability**: container ran 8+ hours continuously; the tunnel survived two IKE rekeys
   (`ims[1]→ims[2]→ims[3]`) and one transient Gm-connection reset that self-recovered without
   intervention. `vowifi-status` at the end: `state: Registered`, `last_failure: none`.
6. **The one thing this session couldn't verify from inside the coding-agent sandbox**: real
   `/sys/bus/usb/devices` attribute values were masked (`idVendor`/`idProduct` read as empty, some
   attributes flatly denied) even after enabling filesystem access — real hardware testing had to
   happen through directly-run shell commands outside that sandbox once real values appeared.

## NOT yet tested (needs a second card)

- **SC-003/SC-004**: no actual inbound call was placed to the SIM's number this session —
  `vowifi-status`'s "Recent calls" stayed empty throughout. Placing a real call to the test SIM's
  own number and confirming it bridges is a good first step next time, even before a second card
  is available.
- **SC-004/SC-005**: concurrent two-line operation, cross-talk-free simultaneous calls, and
  fault-isolation (one line's tunnel dropping without affecting the other) — needs two modems.
- **The vpcd multi-slot design's core open question** (research.md item 4): whether `pcscd`
  actually enumerates the vpcd reader's 8 built slots as 8 separate `SCardListReaders` entries so
  each line's `eap-sim-pcsc` finds its own SIM by IMSI. Slot 0 (the only one exercised) worked, but
  that doesn't prove slots 1–7 are independently addressable. **First thing to check** once a
  second card is available — `quickstart.md` section 0 has the exact commands.
- **SC-006**: full 24h soak, spanning a rekey on *each* line at once.

## Four real bugs this session's testing found and fixed

All four were found only by testing against real hardware/a real carrier — none were caught by
the unit test suite, which is exactly the boundary the constitution's Integration-First Testing
principle expects hardware-dependent behavior to live outside of. Each is fixed, covered by new
unit tests where the underlying logic is hardware-independent, and re-verified live.

### 1. `[vowifi].modem_port`'s default silently defeated auto-discovery

Docs (written earlier in this feature) claimed the default was `""` (auto-discover); the code
still defaulted to `"/dev/ttyUSB6"`. Since the field is never actually empty, `discover`'s role
assignment had no way to distinguish "operator explicitly pinned a port" from "just the code
default" for backward compatibility (acceptance scenario 5).

**Fix**: default changed to `""`; `vowifi::discovery::effective_line_overrides()` treats a
non-empty `modem_port` (with no `[[vowifi.line]]` entries) as an implicit single-line override —
`gsm-sip-bridge/src/config/mod.rs`, `gsm-sip-bridge/src/vowifi/discovery.rs`.

### 2. A multi-AT-port modem defeated port overrides

The test EC200 answers a live `AT` on *both* `ttyUSB0` and `ttyUSB6` (confirmed manually via
`vowifi-plmn`/`vowifi-imsi` against each). Probing always returns the first responder in sorted
order (`ttyUSB0`), so a `modem_port` override naming `ttyUSB6` never matched
`ProbedModem.at_port` — the override silently no-opped. It happened not to change the *outcome*
here (audio-less default already routed to VoWiFi), but would have for an audio-capable modem
relying on the override to force VoWiFi assignment.

**Fix**: `modules::discovery::scan_all_preferring()` takes a list of operator-configured ports and
tries them first per-device (`order_candidates_with_preference`), before falling back to
first-responder order. `main.rs` feeds it every configured override's port.

### 3. Log lines contaminated `eval`'d shell output

`tracing_subscriber::fmt::layer()` defaults to **stdout**. `logging::init()` runs for *every*
subcommand, including `discover --shell-env`/`config vowifi-shell-env`, whose stdout
`docker/entrypoint.sh` captures via `$(...)` and `eval`s. A live "discovered modem" INFO line
landed in the captured string and errored as `command not found` (harmless this run — the real
`LINE_COUNT=1` assignment was on its own line and still evaluated — but fragile, and a genuinely
different interleaving could have swallowed a real assignment).

**Fix**: `fmt::layer().with_writer(std::io::stderr)` — `gsm-sip-bridge/src/observability/logging.rs`.

### 4. `swanctl --uri` doesn't exist in this build; socket must come from `strongswan.conf`

Research done *before* live testing (this session, in response to being asked to fix the
pcscd/vpcd multi-line design) assumed `swanctl --uri <socket>` was a portable global flag. It
isn't, in this pinned strongSwan 5.9.3 build (`-u` is bound to `--uninstall`; passing `--uri`
before the subcommand name — the only way that seemed sensible — is also rejected outright,
since swanctl's first-pass `getopt_long` only recognizes registered command names until one
matches). Confirmed against the actual pinned source
(`src/swanctl/command.c`): `command_dispatch()` reads the vici socket via
`lib->settings->get_str(..., "swanctl.socket" / "swanctl.plugins.vici.socket", ...)` — i.e. from
`strongswan.conf`/`STRONGSWAN_CONF` — *before* parsing any CLI flags at all.

**Fix**: the per-line rendered `strongswan.conf` (`render_line_strongswan_conf` in
`docker/entrypoint.sh`) now also sets a top-level `swanctl { socket = ... }` block; every
`swanctl` invocation is prefixed `STRONGSWAN_CONF="$strongswan_conf"` instead of a nonexistent
`--uri` flag; `--file` (a real `--load-all`-specific option, confirmed against
`src/swanctl/commands/load_all.c`) moved to *after* `--load-all` on the command line.

## One more bug found after the four above (ongoing, not one-shot)

**5. The circuit-switched daemon's periodic rescan kept re-probing the modem VoWiFi already owns.**
`discover`'s one-shot resolution (research.md item 3) only protects *startup* — but the CS
daemon's `rescan_new_modules()` calls `scan_modules()` on an ongoing timer for the container's
entire lifetime, and `scan_modules()` re-ran the full AT-probe on every recognized device every
time, including the one already claimed and in active use by `vowifi-usim-bridge`. Observed live:
`AT+CPIN?: no status in response` roughly every 60s, indefinitely — the "modem claimed by both
subsystems" hazard the spec's own edge cases warn about, just manifesting after startup instead of
at it.

**Fix**: `modules::discovery::scan_modules()` now skips probing entirely (not just filtering the
result afterward) for any device whose card id is already a resolved VoWiFi line
(`active_vowifi_card_ids()`, read from the same line-resolution file `discover` writes).
Deliberately **not** applied to `scan_all()`/`scan_all_preferring()` themselves — those are also
what `discover` calls, and a `docker restart` (same container, same `/tmp`) can leave a stale
resolution file on disk from the *previous* run; a fresh `discover` must still re-probe everything
unconditionally, or it would refuse to ever rediscover its own line after a restart.
**Verified fixed**: zero occurrences of the warning across the subsequent 8+ hour run (previously
constant, every ~60s).

## Files touched this session (beyond the original feature implementation)

- `gsm-sip-bridge/src/config/mod.rs` — `modem_port` default fix.
- `gsm-sip-bridge/src/vowifi/discovery.rs` — `effective_line_overrides`, preference-ordering test.
- `gsm-sip-bridge/src/modules/discovery.rs` — `scan_all_preferring`/`order_candidates_with_preference`,
  `active_vowifi_card_ids`, `scan_modules`'s early-skip.
- `gsm-sip-bridge/src/observability/logging.rs` — stderr fix.
- `docker/Dockerfile` — `--enable-vpcdslots=8` (corrected pcscd/vpcd design, see research.md item 4).
- `docker/entrypoint.sh` — shared pcscd (not per-line), `STRONGSWAN_CONF`/`swanctl.socket`
  rendering, `--file` ordering, `LINE_VETH_SIP_IFACE`/`LINE_VETH_IMS_IFACE` now actually read from
  `discover`'s output instead of a locally-hardcoded `veth-sip$idx` pattern.
- `gsm-sip-bridge/src/main.rs` — `LINE_VETH_SIP_IFACE`/`LINE_VETH_IMS_IFACE` added to
  `discover --shell-env`'s output.
- `specs/013-multi-card-vowifi/research.md` item 4 — corrected (see git history for the
  superseded version): the original "one pcscd per line" decision was wrong on two counts
  (`eap-sim-pcsc` already disambiguates readers by IMSI; per-line pcscd can't coexist at all,
  pcsc-lite has no runtime socket override) — replaced with the verified shared-pcscd/multi-slot
  design.

## Resuming with a second card

1. Rebuild the image if the Dockerfile/Rust source has moved on since this log (it will have).
2. Run `quickstart.md` section 0 first — confirm the vpcd multi-slot enumeration question.
3. Then quickstart.md steps 2–6 (two tunnels, concurrent calls, fault isolation, soak,
   attribution) are the real remaining acceptance criteria.
