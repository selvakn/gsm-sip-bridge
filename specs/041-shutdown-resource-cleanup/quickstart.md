# Quickstart / Verification: 041-shutdown-resource-cleanup

**Feature**: 041-shutdown-resource-cleanup | **Date**: 2026-08-20

Kernel namespace and XFRM behaviour cannot be reproduced in CI, and this session's
sandbox has no root. Everything below runs on a real deployment host (the local Vodafone
box or the remote Pi), with `sudo` or from a privileged container. The `if_id` probe
additionally needs `--pid=host`, which the bridge container itself does not have.

## Phase A — before writing any code

### A1. Record both baselines

Every restart criterion is stated **relative to SC-000**, the known-good baseline, so
both numbers must come from the same host in the same session.

```sh
# SC-000 — the well-separated restart, where no previous run can still hold anything
docker compose -f docker/docker-compose.yml stop gsm-sip-bridge
sleep 180
docker compose -f docker/docker-compose.yml start gsm-sip-bridge

# the immediate restart — today's degraded case
docker compose -f docker/docker-compose.yml restart gsm-sip-bridge

# for both, per line, time until "registered, listening for inbound calls"
docker compose logs -f gsm-sip-bridge | grep -E 'registered|File exists|restarting in 5s'
```

Capture per line, for each of the two: seconds to registered, count of `restarting in
5s`, count of IKE_SA setups. Expect roughly 11s for SC-000 and 163-195s / ~12 / ~8 for
the immediate one — the 2026-07-31 numbers in `docs/operations.md`. The gap between them
is the restart penalty this feature removes. **If the gap no longer reproduces, stop and
re-scope**: the premise has changed.

### A2. The discriminating experiment (R2)

This is the one that decides whether the design's mechanism is right. Immediately after a
stop, watch the id and see what releases it:

```sh
# terminal 1 — poll the id every second from a host-scope container
watch -n1 'docker run --rm --privileged --pid=host --net=host --entrypoint sh \
  <bridge-image> -c "ip link add zz type xfrm if_id 23 && ip link del zz" && echo FREE'

# terminal 2 — stop, then intervene by hand, one variable at a time
docker compose stop gsm-sip-bridge
```

Run the stop four times, each time doing one extra thing by hand from a privileged
`--pid=host` container immediately after, and record when `FREE` appears:

| Run | Manual intervention after stop | Records |
|---|---|---|
| 1 | none (today's behaviour) | the ~150s baseline |
| 2 | `ip xfrm policy flush && ip xfrm state flush` | whether XFRM state is the dominant reference |
| 3 | `ip -n imsN link del tun23-N` (namespace still nameable from the host) | whether an explicit delete releases it at once |
| 4 | both, in order | the sequence the feature implements |

**Expected**: run 4 frees the id in seconds. **If run 4 still takes ~150s**, the
mechanism is wrong — record it in research.md R2 under the stated fallback, and bring the
premise back for review before implementing.

### A3. Pin the `timeout` applet form (R3)

```sh
docker run --rm --entrypoint sh <bridge-image> -c 'timeout 1 sleep 5; echo rc=$?'
```

Must exit non-zero after ~1s. If this build wants `-t`, the step's argv changes and the
plan tests change with it.

### A4. Prove mount propagation (R6)

```sh
sudo mkdir -p /var/run/netns
docker run --rm --privileged --net=host -v /var/run/netns:/var/run/netns:rshared \
  --entrypoint sh <bridge-image> -c 'ip netns add probe041; sleep 5'
# on the host, during those 5s:
ip netns list | grep probe041     # must appear
```

If it does not appear, slice 2 is dropped — say so explicitly rather than shipping a
bind mount that silently does nothing.

## Phase B — after each slice

Unit-testable without hardware, via `make test`:

- The full ordering set O-1 to O-11 (data-model.md), as position assertions over
  `build_shutdown_plan`'s output.
- Every blocking step carries a non-zero bound (O-8).
- **The budget fallback (O-10)**: dropping every abandonable step from the plan still
  leaves a sequence that releases every device and namespace. Asserted over the plan, so
  no clock is needed in the test.
- **The bearer unification (O-9, O-11)**: a VoLTE line emits the same veth `DeleteLink`,
  bounds and reporting as a VoWiFi one, while its own observable order — agent kill,
  confirmed exit, in-namespace `volte-cleanup`, namespace — is unchanged. The rewritten
  VoLTE ordering tests must assert the *same relative order* as the ones they replace; a
  rewrite that quietly relaxes an assertion is the specific failure mode to review for.
- `TerminateIke` names the line's own connection, never the bare `ims` — the existing
  `engines.rs` test's reasoning, applied to the plan.
- `FlushXfrm` is not emitted when the dump is foreign or the inventory half-failed
  (reuses the existing `classify_xfrm_dump` tests unchanged).
- A line with no `StartedLine` still gets its namespace deleted (O-7).
- Running the same plan twice produces no error and no extra steps (FR-008).

## Phase C — live acceptance

Each maps to a success criterion. Run on the real host with all lines registered.

| Check | How | Criterion |
|---|---|---|
| Baseline | restart after a 3-minute stop (A1) | SC-000, the number the rest are stated against |
| Restart cost | 10 consecutive immediate `restart`s; time each line to registered | SC-001 within 10s of SC-000; SC-002 no "already claimed" |
| Host is clean after stop | the four commands in contracts C4, within 5s of exit | SC-003 |
| Nothing force-killed | `docker inspect` exit code; teardown summary line present in logs | SC-004 |
| Restart-loop noise | count `restarting in 5s` per line in the first 5 min | SC-005 ≤1 |
| Carrier churn | count IKE_SA setups per line across a restart | SC-006 ≤2 |
| Force-kill recovery | `docker kill` (no grace), then start | SC-007 within 30s of SC-000 |
| No cost on a clean host | reboot, then first start; time to first registered | SC-008 within 5s of baseline |
| Report is legible | read the teardown summary; hold a namespace open from outside and re-read it | SC-009 |
| Budget fallback works | fault injection below, with a stopwatch on the allowance | SC-010 |

The fault injection for SC-009/SC-010: park a process in a line's namespace from a
privileged container (`ip netns exec ims0 sleep 600`) and stop the container. Two things
must hold — the teardown must still release both tunnel identifiers before the allowance
expires (SC-010, i.e. the wait was abandoned rather than the delete), and the report must
name what it could not release and which waits it dropped (SC-009). Confirm the identifier
is genuinely free with the `ip link add zz type xfrm if_id 23` probe from contracts C4,
not from the log alone.

## Rollback

Slices are independently revertible and there is no persisted state to migrate:

- Slice 2 (host-visible namespaces): remove the bind mount from compose. Namespaces go
  back to being container-private; behaviour returns to today's for the force-kill case.
- Slice 1/3 (teardown steps): reverting restores the old two-step teardown. Nothing else
  reads the new `StartedLine` fields, and no on-disk format changed.
- `stop_grace_period`: lowering it back to the default only risks a mid-teardown kill,
  which is today's behaviour.
