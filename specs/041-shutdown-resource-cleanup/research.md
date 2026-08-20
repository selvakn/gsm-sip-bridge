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

**Residual uncertainty, stated plainly**: which of the three references actually
dominates the 2.5 minutes is not knowable from the source. `docs/operations.md:344`
records a 2026-07-31 measurement concluding nothing shortens the wait — that measurement
covered the *current* teardown only (signal-without-waiting plus `ip netns del`) and none
of the steps above. The conclusion therefore does not transfer, but neither is it
disproved. quickstart.md prescribes the discriminating experiment, run before the code
lands, so the fix is not built on an assumption.

**If the experiment shows the delete still blocks**: FR-001 to FR-012 remain worth
shipping for correctness and diagnosability, but SC-001 would be unmet and the premise
returns for review rather than the requirements being quietly relaxed.

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
