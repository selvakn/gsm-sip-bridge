# Quickstart: Multi-Carrier VoLTE P-CSCF Priming

## Trying it (once implemented)

1. Configure multi-line VoLTE with more than one modem attached, different
   carriers' SIMs in each:
   ```toml
   [volte]
   enabled = true
   bridge_inbound = true
   ```
   Leave `[vowifi]` at its defaults (`enabled = false`), set no
   `[[volte.line]].pcscf` override on any line, and make sure no stale
   `/tmp/pcscf-0*` files exist from a previous run.
2. Start the container (or run `supervise` directly against a modem-
   attached host — this repo's existing `test/` docker setup).
3. Watch supervise's own stdout for a priming pass starting, listing every
   line it's capturing for, each line's own success or failure logged
   individually — not one ambiguous "priming" line for the whole
   deployment.
4. Confirm each line got its own, distinct address:
   ```bash
   docker exec <bridge-ctr> sh -c 'for f in /tmp/pcscf-0-*; do echo "$f:"; cat "$f"; done'
   ```
   Two different-carrier lines must show two different addresses here.
5. Confirm every line actually registered (not just line 0):
   ```bash
   curl -s localhost:9091/metrics | grep volte_registered
   ```
6. Confirm nothing was left over from the transient capture:
   ```bash
   docker exec <bridge-ctr> ip netns list      # no leftover priming netns
   docker exec <bridge-ctr> ps aux | grep -E 'charon|vowifi-usim-bridge'
   ```
7. Restart the container without clearing any `/tmp/pcscf-0-*` file:
   priming must NOT run again for any line (SC-003) — every line registers
   immediately.
8. Delete every `/tmp/pcscf-0-*` file and restart (simulating a redeploy
   that wipes the whole cache, User Story 2): every line must re-prime
   automatically and recover without any manual, per-line step.
9. Pull one modem's SIM out mid-deployment (or otherwise make one line's
   carrier unreachable) while the rest keep working: confirm the other
   lines' registrations are unaffected and the failing line's retry
   attempts are individually identifiable in the logs (FR-005/FR-006).

## Real-hardware validation still needed

Unlike specs/080 (validated single-line on a real Vodafone SIM), this
feature's core new mechanism — **several lines' transient ePDG tunnels
sharing one charon instance concurrently, each then handing its modem
straight to VoLTE's own bring-up in the same process** — has not run
against real, simultaneous multi-carrier hardware. What specifically still
needs a live pass before full trust, per this project's memory of the
hardware actually available:

- A genuinely mixed-carrier rig (e.g. one Vodafone-carrying modem, one
  Jio-carrying modem, both attached to the same host at once) — the one
  scenario this whole feature exists for.
- Confirmation that concurrent per-line IKE_AUTH negotiations against two
  different ePDGs, sharing one charon/VICI socket, don't interfere with
  each other the way `start_vowifi_subsystem`'s real persistent-line case
  already proves they don't for steady-state connections — priming's
  negotiation phase (EAP-AKA over the vpcd/pcscd bridge, per line) is the
  one part of that shared-charon story this feature exercises that the
  persistent path's own hardware validation didn't specifically stress
  concurrently at negotiation time.
- That a fleet-topology change (adding/removing a modem) genuinely leaves
  every other line's already-`card_id`-keyed cache untouched, on a real
  USB re-enumeration (not just a unit test's synthetic modem list).
