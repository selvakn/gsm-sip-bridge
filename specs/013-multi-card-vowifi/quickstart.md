# Quickstart: Verifying Multi-Card VoWiFi

Automated tests cover discovery, role assignment, line-table resolution, and resource derivation
without hardware (research.md item 8). The steps below are the **operator-run** live verification
this feature's Assumptions section calls for — the same boundary features 003–012 already draw.

## Prerequisites

- Two VoWiFi-capable modems (e.g. two EC200U-class modules), each with an activated SIM on a
  carrier that supports VoWiFi/ePDG (Airtel and/or Vi/Vodafone India, per features 011/012).
- Host capabilities: `cap_add: [SYS_ADMIN, NET_ADMIN]`, `privileged: true` (unchanged from 011/012).
- `config.toml` with `[vowifi].enabled = true` and no `modem_port`/`mcc`/`mnc` set (let discovery
  and per-line PLMN auto-derivation do the work — this is the point of the feature).

## 0. One thing to verify first (strongswan engine, >1 line)

The multi-line PC/SC design (research.md item 4) uses **one shared `pcscd`** exposing one `vpcd`
reader with N **slots** — one listening port per line (35963, 35964, …), from
`--enable-vpcdslots=8` in `docker/Dockerfile`. Each line's `charon` runs `eap-sim-pcsc`, which
scans all slots and picks that line's SIM by IMSI (verified in the plugin source). The single
piece not confirmable without hardware: that `pcscd` enumerates the vpcd reader's slots as
**separate `SCardListReaders` entries** so the IMSI scan sees each SIM. Check it directly with
two lines up:

```sh
docker exec <container> sh -c 'pcsc_scan -r 2>/dev/null || opensc-tool -l'   # expect one entry per slot
docker logs <container> 2>&1 | grep -i "Not the SIM we're looking for"       # normal: each charon skips non-matching slots
```

If only one slot/reader ever appears, fall back to the per-line-mount-namespace approach noted in
research.md item 4. (The `swu` engine has no pcscd/vpcd at all, and the strongswan engine's other
per-line isolation — netns, XFRM `if_id`, veth, each charon's own vici socket/log via a rendered
per-line `strongswan.conf`/`STRONGSWAN_CONF` — is independent of this.)

## 1. Discovery (SC-001)

```sh
gsm-sip-bridge --config config.toml discover --shell-env
```

Expect: `LINE_COUNT=2`, distinct `LINE_CARD_ID`, `LINE_MODEM_PORT` entries — one per attached
modem — with no device path hand-typed anywhere in `config.toml`. Unplug one modem's SIM and
re-run: expect `LINE_COUNT=1` plus a logged failure reason for the excluded modem (FR-006).

## 2. Two independent tunnels (SC-002)

Start the container. Expect, in `docker logs`, two full per-line startup sequences (one per
`LINE_CARD_ID`): netns creation (`ims0`, `ims1`), XFRM interface, own `pcscd`/`charon`/
`vowifi-usim-bridge`, own PLMN auto-derivation, own swanctl connection reaching `CHILD_SA
established` with a P-CSCF assigned. Confirm both:

```sh
docker exec <container> gsm-sip-bridge --config config.toml vowifi-status
```

shows both lines' Agent A registration state as `Registered` and both card ids listed.

## 3. Concurrent calls (SC-003, SC-004)

Call each SIM's number from an outside line, one after another: each should be answered and
bridged to the PBX within 5 seconds, with the caller landing on the DID-passthrough number (or the
shared fixed extension if `[bridge].sip_destination` is set) per line — same as single-line
today. Then call **both SIMs at the same time** (two phones, two callers): confirm both are
bridged concurrently with intelligible two-way audio on each and no audio bleeding between them.
Check `docker logs` for both calls' `card_id` tags to confirm correct per-line attribution
(FR-017).

## 4. Fault isolation (SC-005)

While both lines are up and one has an in-progress call, force line 0's tunnel down (e.g.
`ip netns exec ims0 ip link set tun23-0 down` or block its ePDG IP at the host firewall). Confirm:
- Line 1's registration and in-progress call are completely unaffected.
- Line 0 recovers on its own (strongSwan reliability supervisor, replicated per line) within 90s
  of the fault clearing, without a container restart.

## 5. Soak (SC-006)

Leave both lines running ≥ 24h spanning at least one carrier rekey on each. Confirm zero agent
restarts via `docker logs | grep -c 'restarting in 5s'` staying flat, and both lines still
`Registered` in `vowifi-status` at the end.

## 6. Attribution (SC-007)

With both lines active, trigger an SMS to each SIM. Confirm each forwarded Discord message and
each `sms` store row names the correct `card_id` — not a shared generic `"vowifi"` label (today's
single-line placeholder, `VOWIFI_SMS_MODULE_ID`).

## 7. Backward compatibility (SC-008, FR-020)

Revert to a single-SIM `config.toml` (either one modem physically attached, or an explicit
`[vowifi].modem_port` naming exactly one). Confirm `discover` resolves `LINE_COUNT=1` with
`netns=ims`, `strongswan_tun_iface=tun23`, `pcscf_source_path=/tmp/pcscf` — i.e. every path/name
matches pre-multi-card defaults exactly, with no migration step and no operator-visible change in
`vowifi-status` output shape (still the "one line" case, structurally the N=1 instance of the
same command).
