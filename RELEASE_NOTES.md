# Release Notes

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
