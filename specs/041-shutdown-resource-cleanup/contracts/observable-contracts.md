# Phase 1 Observable Contracts

**Feature**: 041-shutdown-resource-cleanup | **Date**: 2026-08-20

What this feature makes observable from outside the process. These are the surfaces the
tests pin and the runbook refers to.

## C1. The stop sequence

For a two-line strongswan deployment, the emitted plan is (VoLTE steps interleave at
their existing positions and are unchanged):

```text
 1  TerminateIke   ims-0                      bounded
 2  TerminateIke   ims-1                      bounded
 3  KillChild      daemon supervisor          TERM
 4  KillChild      sip-agent supervisor       TERM
 5  KillChild      ims-agent line 0           TERM
 6  KillChild      ims-agent line 1           TERM
 7  KillChild      usim-bridge line 0/1       TERM
 8  KillChild      charon                     TERM
 9  WaitForExit    each of the above          bounded, then KILL escalation
10  FlushXfrm      {23, 24}                   all-ours-or-nothing
11  DeleteLink     tun23-0  in netns ims0     bounded   <- releases if_id 23
12  DeleteLink     veth-sip0 in container ns  bounded
13  DeleteLink     tun23-1  in netns ims1     bounded   <- releases if_id 24
14  DeleteLink     veth-sip1 in container ns  bounded
15  DeleteNetns    ims0
16  DeleteNetns    ims1
```

Steps 1-2 and 11-16 are new; 9 is new for VoWiFi children (it exists today only for
VoLTE); 10 is new at stop. A VoLTE line contributes the same shape minus steps 1-2 and
10-11 (no tunnel, no `if_id`), plus its existing in-namespace `volte-cleanup` between its
`WaitForExit` and its `DeleteLink`.

**Contract**: the *relative order* is what is guaranteed and tested (data-model.md O-1 to
O-11), not the absolute positions above.

**Budget fallback** (FR-019): steps 1-10 are abandonable, steps 11-16 are not. If the
remaining allowance drops below what 11-16 need, the executor skips straight to 11 and
reports which steps it dropped. Dropping every abandonable step must still leave a
sequence that releases every device and namespace (O-10).

## C2. Log markers

Stable prefixes, so the runbook and any future log-based check can key on them.

| Marker | When |
|---|---|
| `[supervise] shutting down ...` | unchanged, already emitted |
| `[supervise] teardown: released line <N> (<tun>, <veth>, netns <ns>)` | that line gave everything back |
| `[supervise] teardown: could not release <resource>: <reason>` | one resource failed; teardown continues (FR-012) |
| `[supervise] teardown: complete, <n> resources released, <m> not released` | final summary; `<m> = 0` is the clean case |
| `[supervise] teardown: out of time, skipping <n> remaining waits to release devices` | the budget fallback fired (FR-019) |
| `[supervise] reclaimed <resource> left by a previous run` | start-side reclamation acted (FR-014) |
| `[supervise] could not reclaim <resource> left by a previous run: <reason>` | reclamation failed; startup continues (FR-012) |
| `[supervise] found XFRM state that is not this deployment's ...` | unchanged wording, unchanged behaviour (FR-011/FR-015) |

Per FR-020 these are the **entire** escalation surface: no critical alert is raised and
the exit code is unaffected, on either path.

The existing `could not create <tun> (xfrm if_id <id>)` message keeps its wording but
loses its current advice ("it clears itself ... waiting is the whole remedy"), which
stops being true — see FR-017.

## C3. Compose surface

```yaml
services:
  gsm-sip-bridge:
    stop_grace_period: 60s          # NEW — must exceed the teardown's worst case (R8)
    volumes:
      - /var/run/netns:/var/run/netns:rshared   # NEW — host-visible namespaces (R6)
```

**Contract**: the container must not require either of these to *function*. Without the
grace period it is force-killed mid-teardown and behaves as it does today; without the
bind mount, slices 1 and 3 still work and only force-kill recovery is lost. Neither is a
hard dependency of the binary, which is what keeps them independently revertible.

## C4. Operator-visible host state

After a clean stop, on the host:

```sh
ip netns list | grep -c '^ims'                  # 0
ip link show type xfrm | grep -c tun23-         # 0
ip link show | grep -c veth-sip                 # 0
ip link add zz type xfrm if_id 23 && ip link del zz   # succeeds
```

The last line is the direct test of the feature's premise: today it is refused for ~2.5
minutes after a stop. Note that while the deployment is *running* it is expected to be
refused (R1) — refusal is only a fault after a completed stop.

With the bind mount in place, `ip netns list` on the host also shows the namespaces of a
**running** deployment. That is a deliberate visibility change and the runbooks should
say so, so that seeing `ims0` on the host is not mistaken for a leak.

## C5. What does not change

- The all-ours-or-nothing rule for foreign XFRM state, including its refusal on a
  half-failed inventory.
- The VoLTE teardown's **observable** order: carrier-agent kill, confirmed exit, its
  own `volte-cleanup` inside its own namespace, then the namespace. Per FR-018 that
  sequence is now expressed in the shared step vocabulary rather than built by a
  VoLTE-specific branch, so the existing ordering tests are rewritten against the new
  representation while asserting the same relative order. A VoLTE line additionally gains
  a `DeleteLink` for its veth, which it never had.
- `ensure_epdg_interface`'s idempotent-restart behaviour, including absorbing an
  interface left in the container's own namespace.
- The `swu` engine's behaviour: it has no terminate concept, so it emits no
  `TerminateIke`, and its namespace/device steps are otherwise identical.
