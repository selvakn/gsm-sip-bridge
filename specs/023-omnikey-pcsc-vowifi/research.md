# Phase 0 Research: PC/SC Card-Reader-Backed VoWiFi Lines

No `NEEDS CLARIFICATION` markers remain in plan.md's Technical Context — the
questions that would otherwise have needed research here were already
resolved during specification (spec.md's Clarifications session) and the
prior architecture investigation this plan is based on. This document
consolidates those decisions in the Decision/Rationale/Alternatives format.

## 1. How pcscd sees a real USB reader

**Decision**: Add Alpine's `ccid` package (the generic USB CCID PC/SC driver)
to the runtime image (`docker/Dockerfile`), alongside the existing `vpcd`
virtual-reader driver.

**Rationale**: The archived osmocom "VoWiFi with Asterisk" wiki confirms
strongSwan's `eap-sim-pcsc` plugin "automatically detects" any reader pcscd
exposes — no plugin-side config needed. pcscd itself is already installed;
only the driver that lets it see a *real* USB CCID reader (as opposed to
vpcd's virtual one) is missing. `ccid` is Alpine's standard package for this,
self-registering via USB hotplug (unlike `vpcd`, which needs the static
`/etc/reader.conf.d/vpcd` entry already present).

**Alternatives considered**: Building `libccid` from source (rejected —
Alpine already packages a working build; this project already prefers apk
packages over source builds for pcsc-lite itself, `docker/Dockerfile:233`).

## 2. AID/ADF discovery and ATR for the real card

**Decision**: None needed — a real reader talking real ISO7816-4/T=0 to a
real card provides its own ATR and supports standard SELECT/GET RESPONSE
semantics natively.

**Rationale**: `vowifi-usim-bridge`'s `CANNED_ATR`, lazy EF_DIR/AID discovery,
and GET-RESPONSE-chaining emulation (`gsm-sip-bridge/src/vowifi/usim_bridge.rs`)
exist *specifically* to compensate for `AT+CSIM`'s lack of a real ATR and its
auto-chaining quirks — none of that applies once pcscd talks to a real CCID
reader. `eap-sim-pcsc`'s own AID/ADF selection (proven against real hardware
per the wiki) handles this without any of this project's code in the path.

**Alternatives considered**: Reusing/adapting `usim_bridge.rs` logic for the
real reader (rejected — solves a problem that doesn't exist for real
hardware; would be pure unnecessary complexity).

## 3. Where a card-reader line's network identity comes from

**Decision**: Mandatory `imsi_override`/`mcc`/`mnc` in a `[[vowifi.line]]`
entry with `pcsc_reader = true` — no live IMSI read from the card at startup.

> **Superseded (post-v8.1.0) — `mcc`/`mnc` only.** The premise below (that a
> card-reader line cannot read its identity at startup) turned out to be false
> for the PLMN: the MCC is the first three IMSI digits and the MNC length is in
> the card's own `EF_AD` (`6FAD`), both readable over PC/SC, and
> `PcscTransport::connect` was already doing a live `EF_IMSI` read to pick a
> reader. `mcc`/`mnc` are now optional and derived from the card via
> `plmn::derive_plmn_from_card` / `vowifi-plmn --pcsc-imsi`; only the legacy
> `AT+COPS` fallback is modem-only. `imsi_override` remains mandatory, but as
> the reader-to-line binding key (it must be known *before* any card session
> exists), not because the IMSI is unreadable. See the Unreleased section of
> `RELEASE_NOTES.md`.

**Rationale**: Every other line-identity field in this project already
supports an `Option`-based override convention (`config.toml.example`'s
"pin everything" pattern, also used operationally to sidestep a live
AT+CIMI race on a separate deployment — see prior incident notes). A
card-reader line has no modem to derive these from, and the SIM in question
is static (physically seated, not swapped in normal operation), so requiring
the operator to read it once (e.g. via `pySim-read.py`, per the osmocom
wiki's "Getting IMSI" section) and pin it costs nothing while avoiding a new
live PC/SC read path entirely.

**Alternatives considered**: A new Rust PC/SC client (e.g. the `pcsc` crate)
reading `EF_IMSI` directly at startup (rejected — new dependency, new live
SIM-read code path, no benefit for a SIM that doesn't change; would only be
justified if hot-swapping cards at runtime were in scope, which it isn't).

## 4. Auto-recovery when the reader/card becomes reachable again (FR-011)

**Decision**: No new recovery code — reuse the existing
`supervise::line_supervisor` establish/steady-state supervision loop
unchanged.

**Rationale**: Investigated `gsm-sip-bridge/src/supervise/orchestrate.rs`'s
`start_vowifi_line_strongswan` and `line_supervisor.rs` in detail: every
strongswan-engine line (modem or pcsc-backed alike) already runs under a
steady-state loop (`tick_steady_state`) that detects a dead charon process
or a broken vici socket (`SteadyOutcome::Recovered`) and restarts it, for
the life of the container — this is generic, keyed only by netns/if_id/
swanctl paths, with nothing modem-specific in it. Below that, strongSwan's
own IKE_SA layer (DPD, retransmission, re-authentication) is *why* this
project switched from the `swu` engine to `strongswan` as default in the
first place (specs/012-strongswan-epdg) — a transient EAP-AKA failure
because the card was briefly unseated is exactly the class of failure that
layer already retries without this project's code needing to intervene at
all. A pcsc line gets this for free the moment it runs through the same
`start_vowifi_line_strongswan` path with `vowifi-usim-bridge` simply skipped.

**Alternatives considered**: A bespoke reader-presence poll/reconnect loop
specific to pcsc lines (rejected — would duplicate logic that already exists
generically and is already proven in production for modem lines; violates
the constitution's Simplicity/YAGNI principle).

**Follow-up for implementation**: a live test (quickstart.md) should still
*confirm* this holds for a real reader (reseat the card mid-run, observe
recovery) rather than assume it from code reading alone — this project's own
history (docs/vowifi-epdg-research-notes.md) treats "verified against real
hardware" as the actual bar, not code inspection.

## 5. Status/metrics/alert visibility (FR-010)

**Decision**: No new status field or metric label — a pcsc line flows
through the exact same `LineResolutionEntry`/`vowifi-status` machinery every
modem line does, keyed by `index`/`card_id`, not by SIM source.

**Rationale**: Per the spec's Clarifications session, the operator explicitly
wants *no* visible distinction between SIM sources in monitoring — this falls
out naturally from not adding one, rather than requiring new work. The only
implementation-time check needed is that an empty `modem_port` string (this
plan's chosen representation for "no modem") doesn't render oddly in existing
status/metrics output — a verification task, not new feature code.

**Alternatives considered**: Adding a `sim_source` label to metrics/status
(rejected — the clarification explicitly asked for full parity/no
distinction, Option A over Option B).

## 6. Engine incompatibility (`swu` + `pcsc_reader`)

**Decision**: Validate at supervise startup (not deep in the swu-specific
code path) and fail the whole `supervise` invocation with a clear message
naming the offending line, before any line-specific process is spawned.

**Rationale**: `swu`'s Rust reimplementation only ever calls `AT+CSIM`
against a `modem_port` — there is no code path that could serve a pcsc line
under that engine, so this is a pure configuration-validation concern, not a
runtime one. Failing fast at startup matches spec FR-008/SC-003 exactly and
this project's existing pattern of validating engine/feature combinations
early (e.g. VoWiFi/VoLTE mutual exclusion is already checked similarly).

**Alternatives considered**: Silently skipping the pcsc line under `swu`
(rejected — spec FR-008 explicitly requires a hard failure, not a silent
skip, learning from this project's own past incident where a similar silent
skip on a dead GSM worker slot masked a real outage — see prior operational
history).
