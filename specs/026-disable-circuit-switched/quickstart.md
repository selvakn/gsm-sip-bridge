# Quickstart: disabling the circuit-switched path

**Feature**: 026-disable-circuit-switched

## For an operator

Add two lines to `config.toml` and restart:

```toml
[cs]
enabled = false
```

That is the whole change. Everything else in the file stays as it is — `[modules]`, `[resilience]`, and `[scheduled_restart]` remain valid and are simply inert, so flipping the flag back on later restores your tuning untouched.

### What stops

- Modem discovery at startup, and the periodic rescan (roughly 2,880 bus scans a day at the default 30-second `[modules].retry_interval_sec`)
- All AT traffic issued for circuit-switched purposes
- Circuit-switched calls, inbound and outbound
- Scheduled card restarts

### What keeps running

- The metrics endpoint, the control socket, and the message store
- VoWiFi and VoLTE entirely — including the registration service your desk phones bind to, which those subsystems host themselves

### Two things that surprise people

**Your upstream trunk registration goes away.** If this deployment registered a trunk with a PBX, it stops. That is deliberate: with no circuit-switched path there is nothing behind the trunk, and keeping the registration would invite the PBX to route calls into a dead end. The startup log names `[cs].enabled` as the reason. If VoWiFi or VoLTE is enabled, that subsystem owns the telephone-facing side and nothing changes for your phones.

**Voice-capable modems become available to VoWiFi.** While the flag is on, a voice-capable modem with no explicit `[[vowifi.line]]` override is reserved for circuit-switched use. With the flag off nothing is reserved, so VoWiFi may pick it up — subject to the usual SIM-readiness check and `max_lines` bound.

### Verifying it worked

```bash
curl -s localhost:9091/metrics | grep cs_enabled
# gsm_sip_bridge_cs_enabled 0

curl -s localhost:9091/metrics | grep modules_active
# (no output — the series is absent, not zero)
```

An absent `modules_active` with `cs_enabled 0` present means the path is off on purpose. If `cs_enabled` is missing too, the daemon is not running or the scrape is failing.

Card commands answer rather than hang:

```bash
gsm-sip-bridge card list
# error: circuit-switched path is disabled ([cs].enabled = false) — no cards are managed
```

### Adjusting alerts and dashboards first

The circuit-switched series disappear rather than reporting zero, specifically so that `modules_active == 0` style rules go quiet instead of firing forever. Panels that chart those series will show "no data". If you want a panel that distinguishes "off on purpose" from "daemon down", chart `gsm_sip_bridge_cs_enabled`.

## For a developer

### Reproducing the before/after

```bash
# Baseline: no [cs] section, behaves exactly as before
make test

# Confirm the default is TRUE — the highest-risk assertion in the feature
cargo test -p gsm-sip-bridge --test test_config cs_defaults_to_enabled
```

### The one line most likely to be wrong

`RawCs` must have a **hand-written** `Default` returning `true` — `#[derive(Default)]` gives a bare `bool` `false`, which would silently disable circuit switching for every existing deployment on upgrade. Most `enabled` flags in `config/raw.rs` (`RawVowifi`, `RawVolte`, `RawOutbound`, `RawSipServer`) are opt-in and correctly derive `false`; `RawSms` is the one other opt-out flag and already hand-writes `enabled: true` for the identical reason — `RawCs` follows that precedent, not the opt-in majority. If exactly one test fails and it is the default-value test, check this line first.

### Before committing

```bash
make format
make lint     # workspace-wide, including all test targets
make test
```

All three must pass — see `CLAUDE.md`.
