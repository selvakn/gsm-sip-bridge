# Quickstart: Inbound VoWiFi-to-SIP Call Bridge

**Feature**: 011-vowifi-sip-bridge | **Date**: 2026-07-12

This walks through bringing the feature up end-to-end once implemented, building on the existing
`docker/epdg/` tunnel setup and the existing `[sip]`/`[bridge]` configuration already used by the
circuit-switched bridge.

## Prerequisites

- Quectel EC200U modem on `/dev/ttyUSB6`, SIM provisioned for VoWiFi/IMS by the carrier, as already
  required by the existing `ims-register`/`ims-call` CLI tools.
- A network path to the carrier's ePDG (see `docker/epdg/README.md` for the known-working carrier;
  the other carrier tested is currently blocked by carrier-side policy — see this feature's spec
  Assumptions).
- An existing, reachable SIP/PBX destination — the same one already configured for the
  circuit-switched GSM-to-SIP bridge (`[sip]` / `[bridge]` in `config.toml`).

## 1. Bring up the tunnel and both agents

```bash
docker compose -f docker/epdg/docker-compose.epdg.yml up --build
```

`entrypoint.sh` now, after the SWu tunnel is confirmed up and `/tmp/pcscf` is written:
1. creates the `veth` pair (one end in netns `ims`, one end in the container's default namespace)
2. launches Agent A (`gsm-sip-bridge vowifi-ims-agent ...`) inside `ip netns exec ims`
3. launches Agent B (`gsm-sip-bridge vowifi-sip-agent ...`) in the default namespace
4. supervises both, restarting either on unexpected exit

## 2. Confirm VoWiFi registration is up

```bash
docker exec epdg-tunnel gsm-sip-bridge vowifi-status
```

Expected: `state: Registered`, an `expires_at` in the future, no `last_failure`. (Exact CLI surface
is an implementation detail decided during `/speckit.tasks`; this confirms the User Story 3
capability — checking line health through existing-style tooling.)

## 3. Place a real inbound test call (User Story 1)

From an external phone, call the SIM's number. Expect, within a few seconds:
- The call is answered.
- Audio is bridged to the configured SIP/PBX destination; whoever picks up there can talk to the
  caller and vice versa.
- Hanging up from either side ends both legs promptly.

This is the feature's core acceptance test (spec User Story 1, SC-001/SC-002).

## 4. Exercise resiliency (User Story 2)

```bash
# simulate a WAN interruption to the underlying network path, then restore it
```

Expect VoWiFi reachability to restore automatically (no commands run against the agents) within
the SC-003 window (90 seconds of the network path being restored), after which a subsequent test
call (step 3) succeeds again.

## 5. Exercise the decline path

- Place a second inbound call while the first is still bridged (step 3): expect an immediate busy
  signal to the second caller (FR-009), not silence or unanswered ringing.
- Temporarily point the SIP/PBX destination at an unreachable address, then place a call: expect an
  immediate busy signal (FR-010) rather than the call being answered into dead air.

## 6. Confirm the existing CS-GSM bridge is unaffected

While the above is running, place a normal circuit-switched call to a *different* line/modem still
running the existing bridge and confirm it still answers and bridges normally (FR-006) — the two
paths are independent.

## Notes for implementers

- Steps 3–6 require live hardware, a live SIM, and a live carrier network; they cannot run in
  `cargo test --workspace` and are not part of the `make lint`/`make test` pre-commit gate. They
  are the manual verification pass referenced in this feature's plan.
- Everything else (SIP UAS parsing, SDP offer/answer, registration renewal scheduling, the Agent
  A↔B control protocol, `pjsua-safe`'s two-call conference bridging) should be exercised by
  `cargo test --workspace` without needing any of the above hardware, per `research.md` item 8.
