# Quickstart: Verifying Per-Line Network Isolation for VoLTE

Namespace/veth-name derivation and the FR-004a non-collision property are covered by unit tests
without hardware. The steps below are the **operator-run** live verification the spec's own
Assumptions section calls for — the boundary every VoLTE/VoWiFi multi-line feature to date has drawn
— and they are the direct test of the defect this feature exists to close (spec User Story 1).

## Prerequisites

- Two LTE modems (e.g. two EC20-class modules), each with an activated SIM. **Ideally on the same
  carrier** — that is the worst case this feature is motivated by (docs/todo.md's own open question),
  and the case a shared routing table cannot tell apart.
- `config.toml` with `[volte].enabled = true`, `[volte].bridge_inbound = true`, and no
  `[volte].modem_port` set (auto-discover both lines).
- Host capabilities unchanged from VoWiFi's (`privileged: true`, `network_mode: host` — the
  container already has everything `ip netns`/`ip link ... netns` needs).
- `tcpdump` available in the container (or run captures from the host against the container's
  network namespace via `nsenter`/`ip netns exec` from outside).

## 1. Startup — two isolated namespaces, one shared registration (SC-002-equivalent, FR-004a)

Start the container. Expect, in `docker logs`:

```text
[entrypoint] line 0 (<card_id_0>): moving <iface_0> into netns volte ...
[entrypoint] line 0: veth ready: veth-volte-sip=... (default netns), veth-volte-ims=... (netns volte)
[entrypoint] line 0: starting volte-carrier-agent (netns volte), supervised...
[entrypoint] line 1 (<card_id_1>): moving <iface_1> into netns volte1 ...
[entrypoint] line 1: veth ready: veth-volte-sip1=... (default netns), veth-volte-ims1=... (netns volte1)
[entrypoint] line 1: starting volte-carrier-agent (netns volte1), supervised...
[entrypoint] starting volte-bridge (default netns, one shared process for all lines), supervised...
```

Confirm both lines reach a registered state (`volte-status` reports both `card_id`s with
`registered=true`), and confirm the namespaces exist and are distinct:

```sh
docker exec <container> ip netns list                       # expect: volte, volte1 (and ims*, if VoWiFi also runs)
docker exec <container> ip netns exec volte  ip addr show <iface_0>   # line 0's interface, only here
docker exec <container> ip netns exec volte1 ip addr show <iface_1>   # line 1's interface, only here
```

## 2. The actual defect this feature closes (SC-001, User Story 1)

With both lines registered, capture on each interface independently while placing a call on each:

```sh
docker exec <container> ip netns exec volte  tcpdump -i <iface_0> -w /tmp/line0.pcap &
docker exec <container> ip netns exec volte1 tcpdump -i <iface_1> -w /tmp/line1.pcap &
# place a call to line 0's number, let it ring/answer/hang up
# place a call to line 1's number, let it ring/answer/hang up
# stop both captures
```

**Pass**: `line0.pcap` contains only line 0's REGISTER/INVITE/RTP (source/destination addresses
matching line 0's carrier-assigned address); `line1.pcap` contains only line 1's. Neither capture
contains a single packet whose SIP `Call-ID` or media SSRC belongs to the other line's call.

**This is the scenario that could not be verified before this feature**: on the pre-namespace
in-process arrangement, both lines shared one routing table, so this same test could show either
line's traffic on either interface depending on route metric ordering — invisibly, since both
lines' software believed it was using its own connection throughout (see the feature's Context).

## 3. Concurrent calls, no cross-talk (SC-001 continued)

Call both lines' numbers within a few seconds of each other. Confirm both are answered and bridged
with intelligible two-way audio, and that the per-interface captures from step 2 (repeated for this
overlapping-call case) still show no cross-line packets during the overlap window specifically —
the case most likely to expose a shared-route mistake.

## 4. Fault isolation (SC-004, User Story 3)

```sh
docker exec <container> ip netns exec volte1 ip link set <iface_1> down   # simulate line 1 losing carrier
```

Expect: line 1's registration drops and is reported against `card_id_1` in logs/`volte-status`; line
0's registration and any in-progress call on line 0 are **unaffected**. Bring `<iface_1>` back up
(or restart the container) and confirm line 1 recovers independently.

## 5. Two subsystems, same container (User Story 1 scenario 5, FR-004a live check)

With `[vowifi].enabled = true` also set (a different SIM than either VoLTE line), start the
container and confirm:

```sh
docker exec <container> ip netns list   # expect: ims (or ims0/ims1, ...) AND volte (or volte0/volte1, ...) — no name in common
```

Both subsystems' lines register and carry calls concurrently with no interference — this is the
scenario spec Edge Cases explicitly calls out as the documented normal deployment shape, not a rare
combination.

## 6. Unclean shutdown / restart idempotency (SC-005, FR-011)

```sh
docker kill -s KILL <container>     # no trap runs — simulates a crash, not a clean stop
docker start <container>
```

Expect every line to come back up cleanly with no manual `ip netns del`/`ip link` intervention —
`docker logs` shows the "already exists, reusing" / "already in target netns, reusing" idempotency
log lines (research.md R5) rather than errors.

## 7. Compatibility check (SC-003, User Story 2)

Re-run the existing single-line and multi-line VoLTE test/quickstart procedures from
specs/017-volte-inbound-bridge and the prior multi-modem work unmodified. Every previously-passing
criterion — call-answer latency, attachment-loss-during-call protection (the carrier's ~2-hour
detach/reattach must still not drop a live call), per-line status/SMS attribution — must still pass
with no behavioral difference an operator would notice.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `volte-carrier-agent` for a line never reaches "registered" | Veth pair not up before the carrier agent started — check `entrypoint.sh` ordering (contract: namespace/veth setup completes before the carrier agent launches). |
| Both lines' traffic appears on one interface in step 2 | The interface move (R5) did not happen, or happened after — not before — `attach()`/`netcfg::configure()` ran; check `docker logs` for the "moving `<iface>` into netns" line preceding that line's attach. |
| A line's displaced data context isn't restored after shutdown | Teardown ran from the default namespace after the line's namespace/interface had already moved (research.md R6) — check that cleanup runs `ip netns exec <netns> ... volte-pdn down` before `ip netns del <netns>`, not after. |
| `ip netns list` shows a VoLTE namespace with the same name as a VoWiFi one | FR-004a violated — check the namespace base prefix in `config.toml`'s (or the built-in default) `[volte]` vs `[vowifi]` sections; they must differ. |
