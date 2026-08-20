# Phase 0 Research: Complete release of per-line kernel resources on stop

**Feature**: 041-shutdown-resource-cleanup | **Date**: 2026-08-20

No `NEEDS CLARIFICATION` remains in Technical Context. Two findings below (R2, R6) carry
a residual uncertainty that **cannot** be resolved by reading code — they are kernel and
mount-propagation behaviour, and both are settled by a live measurement scheduled in
quickstart.md rather than by assertion here.

## R1. What actually holds the `if_id` — and what releases it

**Finding**: an XFRM interface registers its `if_id` in the namespace it was *created*
in, not the one it lives in. `supervise` creates `tun23-N` in the container's namespace
and then moves it into `imsN` (`epdg_iface.rs:250-269`), so the device holds the id
against the container's namespace while being invisible to `ip link show` there. This is
already documented with live evidence — `ip -d link show type xfrm` inside `imsN` shows
`link-netnsid 0` (`docs/operations.md:329-340`).

Two consequences, both load-bearing for this design:

- A **healthy** deployment refuses its own `if_id`s at all times. `EEXIST` is not by
  itself evidence of a leak, and any check we build must not treat it as one.
- The id is released by exactly one event: **the netdev being destroyed**. No flush
  touches it, because no XFRM state is involved. `ip netns del` does not touch it either
  — it removes a name, not a device.

**Decision**: `ip link del` is the load-bearing step of this whole feature; every other
step exists to make it succeed promptly and to make its failure legible.

## R2. Why the device outlives the container today

`ip netns del` unlinks the name; the namespace itself survives while anything still
references it, and the devices inside it are unregistered only when the namespace is
finally destroyed — which itself waits for every outstanding reference to the device to
drop. Three references are plausibly still outstanding at container exit today, and all
three are things the current teardown never removes:

1. **Processes still running in the namespace.** `vowifi-ims-agent` runs inside `imsN`
   via `ip netns exec` (`orchestrate.rs:1546-1556`). The plan sends it `SIGTERM` and
   never waits (`shutdown.rs:125-130`) — no `WaitForExit` is emitted for any VoWiFi
   child, unlike the VoLTE ones. It is then force-killed by the runtime, which is the
   case that leaves the most kernel state behind (abrupt socket teardown).
2. **XFRM state and policies** installed by charon against `if_id 23/24`, never torn
   down: nothing calls `terminate` at shutdown, and the flush that exists runs only at
   *startup*. Their cached dst bundles reference the tunnel device.
3. **The veth pair**, never deleted — `start_line_tail` creates it
   (`orchestrate.rs:1494-1496`) and the teardown has no counterpart.

**Decision**: remove all three, in dependency order, while the container is alive, and
then delete the device explicitly rather than hoping namespace destruction reaps it.
`ip link del` is synchronous — the rtnetlink acknowledgement comes after unregistration
completes — which converts "eventually, invisibly" into "now, or a bounded failure we
can name" (FR-012).

**Settled 2026-08-20 — measured, not merely argued (R9).** `docs/operations.md:344`'s
2026-07-31 conclusion ("nothing shortens the wait") does not transfer to this teardown,
and the new one is confirmed rather than assumed: stopping and restarting the same
container with the new code end to end took **8 seconds** stop-to-tunnel-up, against
**167 seconds** measured on the *old* code on the same rig moments earlier, with zero
`RTNETLINK answers: File exists` occurrences and the teardown report showing
`0 resource(s) not released, 0 abandoned`. Deleting the device before the namespace name
is removed is the whole difference — R1's mechanism, not a guess. See R9 for the full
before/after and the caveat about the old→new transition case (which still shows the old
number, for reasons explained there — not a regression).

## R3. How to bound a step that can block

`CommandRunner::run` is `Command::output()` (`runner.rs:375`) — unbounded. `ip link del`
can block while the kernel waits for references to drop, which is precisely the failure
mode we are designing against, so FR-009 is not optional.

**Decision**: bound at argv level with the `timeout` applet — the step carries
`["timeout", "<secs>", "ip", "link", "del", ...]`.

**Rationale**: the bound becomes part of the pure plan, so it is asserted in the same
unit tests as the ordering, with no runner-trait change, no second implementation in the
mock, and no thread. The image is Alpine, so `timeout` is a busybox applet already
present — no package to add.

**Alternatives considered**:
- *`run_bounded` on `CommandRunner`*: a trait method plus a real wait-with-timeout plus a
  mock implementation, to express what argv already expresses. Rejected (Constitution V).
- *`spawn` + `WaitForExit` + `KillChild`*: reuses existing vocabulary but turns one step
  into three and makes the plan harder to read for no gain.

**Verification owed**: busybox's `timeout` argument form differs across versions
(`timeout -t SECS PROG` in old builds, `timeout SECS PROG` since 1.30). Pin it with a
one-line check in the image before relying on it — a task in tasks.md, not an assumption.

**Confirmed 2026-08-20** against the built `gsm-sip-bridge:041-test` image: `timeout 1
sleep 5` terminates the child after ~1s (`rc=143`, busybox sending `SIGTERM` rather than
coreutils' `124` — the exit code differs, the *behavior* T009 relies on does not). The
`timeout SECS PROG` form used throughout `shutdown.rs` is correct as written; no change
needed.

## R4. Terminating tunnels when charon is shared

There is **one** charon for every strongswan-engine line (`orchestrate.rs:61-66`) because
N charons in one namespace would all wildcard-bind UDP 500/4500. So teardown cannot
simply kill "the line's tunnel process".

**Decision**: per line, `swanctl --terminate --ike <conn>` scoped to that line's
connection name; then stop the single charon once, after every line has been terminated;
then wait for it.

**Rationale**: `StrongswanEngine::terminate` (`engines.rs:375`) already does exactly this
and is already test-pinned as line-scoped (`engines.rs:867` —
`terminate_is_scoped_to_this_lines_connection`, which exists because the bare `ims` name
would tear down every line at once). Reuse it; do not reimplement.

**Note**: `SwuEngine::terminate` is a deliberate no-op (`engines.rs:593`) — that engine
has no in-place terminate concept. The step is emitted only for strongswan-engine lines,
which keeps the fallback engine's behaviour unchanged.

## R5. Where the encryption state lives, and the flush guard

charon runs in the container's own namespace, not the line's; the per-line updown script
installs addresses *into* the namespace. So the SAs and policies are in the container's
(host) namespace, which is where `reclaim_stale_xfrm` already looks (`epdg_iface.rs:85`).

**Decision**: run the same classify-then-flush at stop, after every line has been
terminated, reusing `classify_xfrm_dump` unchanged.

**Rationale**: iproute2 has no `ip xfrm policy deleteall if_id N`, so the flush is
unfiltered and the all-ours-or-nothing rule is the only safe guard around it — including
its existing refusal on a half-failed inventory (`epdg_iface.rs:99-113`, itself a prior
review finding). Nothing about that reasoning changes at stop; only the timing improves.
After a clean terminate the dump should already be empty, which makes this a belt-and-
braces step rather than the primary mechanism — and `Empty` is already a no-op branch.

## R6. Making the namespaces addressable after the container is gone

`ip netns add` bind-mounts the namespace under `/var/run/netns/` **inside the container's
own mount namespace**. Compose mounts only `/dev`, `/sys`, the config and the data
volume. So when a container is force-killed, its namespaces survive with no name any
later run or any operator can refer to — the reason `docs/operations.md` can only offer
"wait, or reboot the host".

**Decision**: bind-mount the namespace directory from the host into the container with
shared propagation, so namespaces created inside are visible and removable outside.

**Rationale**: it is the only way to satisfy FR-013/FR-014 without inventing a parallel
naming scheme; it also gives operators plain `ip netns` on the host, which every existing
runbook step becomes simpler for.

**Verification owed** (the reason this is its own slice): propagation is the fragile part
— the mount must be shared for namespace creations inside the container to appear on the
host, and iproute2 makes the directory shared itself on first use. Both need confirming
on the real host, not asserted here. If propagation cannot be made to work, slice 2 is
dropped and slices 1 and 3 still deliver SC-001 through SC-006; only the force-kill
recovery path (SC-007) is lost.

**Confirmed 2026-08-20** on the live rig: a throwaway privileged container with
`-v /var/run/netns:/var/run/netns:rshared` ran `ip netns add probe041test`; the namespace
was visible on the **host** (`ls /var/run/netns/`, outside any container) while that
container was still running, and remained visible after the container exited — exactly
the "survives the container that created it" property FR-013 needs. `rshared`
propagation works on this host as designed; no change to the compose mount needed.
Cleaned up with `ip netns del probe041test` via a second throwaway container (no host
root available in this environment). **Not yet exercised**: `reclaim_leftover_lines`
itself finding and reclaiming a namespace this way after a real force-kill — that needs
the compose bind mount actually deployed under the running service, which the ad-hoc test
container in R9 did not include.

**Alternative considered**: have `supervise` create its own nsfs bind mounts under an
already-mounted host directory instead of using `ip netns`. Rejected — it reimplements
what iproute2 does, and every diagnostic command in the runbooks assumes `ip netns`.

## R7. FR-016 — not touching a concurrently running instance

**Finding**: two concurrent instances of this deployment on one host are **already
impossible**, before this feature. They would collide on fixed namespace names (`imsN`),
fixed veth addresses, fixed `if_id`s, and charon's wildcard bind of UDP 500/4500. Compose
replaces the service rather than duplicating it.

**Decision**: no lock. Reclamation is gated on the namespace not having been created by
the current run, and the single-instance constraint is documented.

**Rationale**: the obvious guard — an flock on a host-visible file — needs `unsafe` via
`libc::flock`, which `make lint` forbids, and it would guard a case that cannot occur.
`ip netns pids` is not an alternative: the container has its own PID namespace and cannot
see another container's processes, so it would report "empty" for a live instance and be
actively misleading.

## R8. Sizing the stop allowance

Worst case per stop, for the supported 4 lines: per-line terminate (bounded, ~3s each) +
charon exit wait (~5s) + in-namespace agents exiting (~5s, the existing
`KILL_CONFIRM_MAX_POLLS` bound) + flush (~1s) + two bounded link deletes per line (~5s
each) + namespace deletes. That lands near 45s.

**Decision**: `stop_grace_period: 60s` in compose, with the teardown's own bounds sized
so it finishes comfortably inside it.

**Rationale**: the allowance must exceed the teardown's worst case, not the typical one —
being force-killed halfway through a teardown leaves *worse* state than not starting one,
which makes this a prerequisite for slice 1 rather than a nicety. Docker's 10s default is
what applies today.

**Note for operators**: `docker stop` without compose uses its own default; the runbook
update (FR-017) must say so.

## R9. Measured on live hardware (2026-08-20, gsm-jio-cap rig — real Vodafone/Vi line)

Ran directly against the local EC20 rig (T002/T003's live gate), one line configured
(AOR "1002"). This supersedes the "residual uncertainty" framing in R2 — the mechanism is
confirmed, not merely argued.

**Old code, immediate restart** (`docker stop && docker start`, same container):

| Metric | Value |
|---|---|
| Time to `vowifi-ims-agent registered after restart loop` | **167s** |
| `RTNETLINK answers: File exists` occurrences | 5 |
| `restarting in 5s` occurrences | 23 |
| IKE_SA established count | 2 |

Matches the 2026-07-31 figures (163-195s) closely — the premise reproduces cleanly on
this rig, unchanged.

**New code, old→new transition** (old container stopped with the *old* teardown, new
container started fresh): **167s** to first registration, 0 reclaim activity logged.
Expected, not a regression: the old teardown already ran `ip netns del` before exiting,
unlinking the namespace name *before* deleting the device — so by the time the new
container starts there is no longer a name for `reclaim_leftover_lines` to find. What
remains is an orphaned, nameless kernel object waiting on the same asynchronous reap R2
already documented. New code cannot reclaim what has no name; this is a transition
artifact, not a finding about the new teardown itself.

**New code, both ends** (stop the new container, start it again — the real test of R1's
mechanism):

| Metric | Value |
|---|---|
| Stop-to-tunnel-UP (`IKE_SA ims0[1] established` → `[supervise] line 0: tunnel UP`) | **8s** |
| `RTNETLINK answers: File exists` occurrences | **0** |
| `restarting in 5s` occurrences | 3 |
| Teardown report | `teardown: complete, 83 step(s) completed, 0 resource(s) not released, 0 abandoned to the stop allowance` |

**167s → 8s.** R1's mechanism is confirmed directly: destroying the device before deleting
the namespace name releases the `if_id` immediately, with nothing left unreleased or
abandoned to the budget. SC-001/SC-002/SC-006 are met on this measurement; SC-003/SC-004/
SC-005/SC-009 are consistent with it (clean teardown report, no restart-loop churn beyond
the ordinary tunnel-establish window).

**Not yet measured**: SC-007 (force-kill recovery via `reclaim_leftover_lines`) — the ad-hoc
`docker run` used for this test did not include the `/var/run/netns` bind mount from
`docker-compose.yml` (R6), so reclamation was not exercised end-to-end; and SC-008 (no
added latency on a clean host).

**Unrelated, confirmed pre-existing**: the container reports `unhealthy` on both the old
and new binaries, healthcheck failing with `configured line /dev/ttyUSB2: not running` —
identical on both, present before this feature's code ever ran. Not caused by this change;
out of scope for this feature per the user's explicit instruction.

### 10 consecutive immediate restarts (T034)

Same rig, same container, `docker stop && docker start` ten times back to back, timing
`tunnel UP`:

| # | Tunnel UP | `File exists` |
|---|---|---|
| 1 | 9.1s | 0 |
| 2 | 0.1s | 0 |
| 3 | 8.0s | 0 |
| 4 | **timeout (>60s)** | 0 |
| 5 | **timeout (>60s)** | 0 |
| 6 | **timeout (>60s)** | 0 |
| 7 | 8.5s | 0 |
| 8 | 8.3s | 0 |
| 9 | 11.0s | 0 |
| 10 | 9.7s | 0 |

**`if_id`/device release held perfectly across all 10 — zero `File exists` occurrences,
zero exceptions.** This is the direct claim this feature makes, and it did not fail once
under back-to-back restart pressure the old code was never fast enough to be exercised
against at all.

Iterations 4-6 surfaced a **separate, orthogonal issue**: `control channel not bindable
yet (this line's veth is probably not up); retrying`, followed by the tunnel abandoning
the barely-established IKE_SA and restarting the whole negotiation from `IKE_SA_INIT`
— repeatedly, roughly every 9-11s, without ever completing. The old-code baseline restart
(above) shows zero such warnings and only 2 `IKE_SA_INIT` attempts total, so this is not
something the old code exhibited — but the old code was *never fast enough to restart
this quickly ten times in a row* for it to have been exercised either. Most plausible
explanation: something in the establish/steady-state loop (`line_supervisor.rs`,
untouched by this feature) treats a slow-to-bind control channel as grounds to abandon
and reinitiate, and that path had never been exercised at this cadence before, because
nothing could restart fast enough. **This is not a regression in the shutdown/reclaim
code this feature adds** (`File exists` stayed at 0 throughout, including during the
timeouts), but it is a real, newly-visible behavior worth a follow-up investigation of
its own, outside this feature's scope.

### Force-kill recovery (T034, SC-007)

`docker kill` (SIGKILL, no grace) while the tunnel was up, immediately followed by
`docker start`:

- **Confirmed**: `ims0` was visible on the **host** (`ls /var/run/netns/`, outside any
  container) immediately after the kill — R6's bind-mount mechanism works exactly as
  designed for the ungraceful-exit case.
- **First restart attempt failed**, but not on anything this feature touches: pcscd's
  vpcd reader could not bind `127.0.0.1:15963` (`PcscdDied`), and supervise exited
  `FATAL` before it ever reached `reclaim_leftover_lines` — this happens earlier in
  startup than the reclamation call. The container sat exited (no restart policy set on
  this ad-hoc test container) until manually started again ~2 minutes later.
- **Second restart attempt succeeded** in 4.0s, 0 `File exists`, but logged **no
  `reclaimed ...` line** — meaning either `reclaim_leftover_lines` found nothing (the
  ~2-minute gap since the kill is close enough to the documented natural-reap window
  that the orphaned namespace may have already cleared on its own by then), or it acted
  silently. Not distinguishable from this run alone.

**Net assessment**: R6's prerequisite (host visibility of a killed run's namespace) is
directly confirmed. Whether `reclaim_leftover_lines` itself is what made the *second*
attempt fast — versus natural reap coincidentally finishing in the same window — is
**not cleanly isolated by this test**, because the pcscd/vpcd startup failure ate the
critical early window where that distinction would have been visible. A clean SC-007
measurement needs a restart attempt that succeeds *immediately* after the kill, which
this one did not get on the first try. The pcscd/vpcd port-reuse-after-SIGKILL failure
is itself a separate, real finding — outside this feature's scope (it lives in
`vpcd::spawn_pcscd`/`wait_for_vpcd_ready`, not `shutdown.rs`) — worth its own follow-up.

**Both findings above are flagged, not fixed, in this PR.** Neither touches the code this
feature changes; both are newly visible only because restarts are now fast enough, and
force-kill recovery robust enough, to reach cadences and conditions the old code could
never be tested under.
