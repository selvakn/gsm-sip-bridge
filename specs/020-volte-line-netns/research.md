# Phase 0 Research: Per-Line Network Isolation for VoLTE

## R1: Isolation mechanism — network namespace, not socket discipline

**Decision**: An OS-level network namespace per VoLTE line, created and populated by
`docker/entrypoint.sh` exactly the way VoWiFi's already is, not `SO_BINDTODEVICE`/policy routing
(`ip rule`) applied at each carrier-facing socket call site.

**Status**: ✅ Decided (the spec's own Assumptions section already names this; this item confirms it
against the codebase's constraints rather than deciding it fresh).

**Rationale**: A namespace makes "which interface can this line's traffic use" a structural fact —
there is exactly one interface, and one routing table, inside a given line's namespace, so a
destination-only route lookup cannot resolve onto another line's interface no matter what address it
targets. The alternative was checked against the actual call sites and rejected:

- `ims/sip_client.rs:513-517` (`SipTransport::connect`) and `:539-548` (`connect_from`) both bind
  `0.0.0.0:0`/`[::]:0` today. Making these correct under policy routing would mean auditing every
  current and future socket construction in `sip_client.rs`, `volte/pcscf.rs` (its own
  `UdpSocket::bind`, line 359 and 548), and anything the RTP relay opens — and re-auditing on every
  change. A namespace needs zero changes to any of them: a process started via `ip netns exec`
  inherits the namespace for every socket and every `Command::new("ip"|"sysctl")` shell-out it
  makes, which is exactly what `netcfg.rs`'s and `pdn.rs`'s existing shell-outs already are. This is
  spec FR-002 verified against the actual code, not asserted.
- The zero-`unsafe` constraint rules out raw `setns()` FFI from within one process managing several
  lines' sockets on different threads — which policy routing wouldn't have needed anyway, since
  `SO_BINDTODEVICE` is a plain `setsockopt`, but the point is moot: `ip netns exec` (a subprocess
  launch, already how `docker/entrypoint.sh` runs `vowifi-ims-agent`) needs no FFI addition at all.

**Alternatives considered**:
- *`SO_BINDTODEVICE` per socket* — rejected: correct only as long as every call site remembers it;
  one future connection helper that forgets reproduces the exact bug this feature closes.
- *Policy routing (`ip rule add from <addr> table <n>`)* — rejected: works for unicast connect()
  calls with the source address already known, but VoLTE's link-local/multicast traffic during
  `netcfg.rs`'s router-solicitation dance (interface-scoped by name already, not by source address)
  and DHCPv6 (`pcscf.rs`) would need separate, interface-scoped rules anyway — at which point the
  namespace already had to exist conceptually; better to make it real.
- *`setns()` per thread* — rejected: raw syscall, needs `unsafe` FFI (violates the workspace's
  zero-`unsafe` policy), and Rust's standard socket/process APIs have no notion of "this thread's
  netns" the way a subprocess launched via `ip netns exec` does trivially.

---

## R2: The telephone-side half already supports this — it just isn't used that way yet

**Decision**: No new code for Agent B. `volte::bridge::run_inner` (`bridge.rs:153-163, 191-199`)
already constructs `crate::vowifi::RuntimeLine`s and calls `crate::vowifi::run_telephony_side` — the
identical function VoWiFi's Agent B uses to reach Agent A over a real veth pair — passing
`veth_local_addr`/`veth_peer_addr` as `LOOPBACK` for every line only because, today, every line's
carrier half is a thread in the same process/namespace. `run_telephony_side` takes an address, not a
literal loopback; VoLTE just hasn't had a reason to give it a different one until now.

**Status**: ✅ Confirmed by reading the code (not inferred from naming) — this is the load-bearing
fact this whole plan's low cost rests on.

**Rationale**: If Agent B needed new code, this feature would be inventing a second veth-bridging
mechanism next to VoWiFi's — the opposite of Constitution V. Because it doesn't, per-line isolation
for VoLTE is: derive real veth addresses per line (R4), extract the carrier-half thread body into a
subprocess entry point (R3), and change what `run_inner` passes to `run_telephony_side`. Everything
downstream of that call is already correct.

**Alternatives considered**: None — this is a discovery, not a choice.

---

## R3: Extracting the carrier half into its own process

**Decision**: `run_line`/`run_line_carrier` (`bridge.rs:240-399`) move, largely verbatim, into a new
`volte::carrier_agent` module reachable as a new CLI subcommand, `volte-carrier-agent --line N`,
mirroring `vowifi-ims-agent --line N`. `run_inner` stops spawning one thread per line; it spawns only
the shared telephony thread (Agent B, in-process, default namespace, as today) and returns —
`docker/entrypoint.sh` becomes responsible for starting one `volte-carrier-agent` subprocess per line
inside that line's namespace, the same division of responsibility already used for VoWiFi (Rust
process boundaries + namespaces are `entrypoint.sh`'s job; per-line business logic is the binary's).

**Status**: ✅ Decided.

**Rationale**: A namespace is a process-table property (or, via `setns()`, a thread property this
project's zero-`unsafe` policy rules out — R1). Isolating a line's carrier half therefore requires it
to be its own process. `run_line_carrier`'s actual logic — attach, derive PLMN, register, call
`ims::agent::serve_inbound` — does not change; only its home (a spawned thread's closure vs. a
subcommand's `main`) does. This is the same move specs/013 made turning VoWiFi's single-line Agent A
into `vowifi-ims-agent --line N`.

**What carries over unchanged**: the per-line retry/backoff loop (`LINE_RETRY_BACKOFF`), the modem
lock shared between the carrier half and the SMS reader thread (still spun up from within the new
subcommand, same as today), the `pbx_registered` admission check — now read over the control channel
from Agent B instead of a shared `Arc` (this is exactly the mechanism VoWiFi's Agent A/B split
already uses; `run_line_carrier` today reads `pbx_registered.clone()` directly only because it is a
sibling thread — `ims::agent::serve_inbound`'s existing `pbx_registered: Option<...>` parameter is
already the abstraction to keep using, sourced the same way VoWiFi's Agent A sources it).

**Alternatives considered**:
- *Keep threads, `setns()` each thread individually* — rejected by R1 (zero-`unsafe`).
- *One subprocess per line that also contains a private copy of Agent B's logic* (full Agent A/B
  pair per line, N SIP registrations) — rejected: spec FR-006/FR-007 require the *one shared* PBX
  registration the prior multi-modem feature established to keep working unchanged; N registrations
  changes externally-observable behavior the compatibility requirements forbid.

---

## R4: Namespace and veth naming — derived, and provably distinct from VoWiFi's

**Decision**: `volte::discovery` gains a per-line derivation shaped exactly like
`vowifi::discovery::resolve_one_line` (`discovery.rs:224-241`): index `0` keeps unindexed defaults
(back-compat, FR-020-equivalent for this feature); index > 0 appends the index to a namespace/iface
base. The base itself is `volte`-prefixed (`netns = "volte"`, veth ifaces `veth-volte-sip`/
`veth-volte-ims`, or equivalent), never `ims`-prefixed, so a VoLTE line's identifiers cannot collide
with a VoWiFi line's by construction, not by convention.

**Status**: ✅ Decided; closes spec FR-004a.

**Rationale**: `docker-compose.yml` documents VoWiFi and VoLTE as two subsystems that can both be
enabled in the same container (though not on the *same SIM* — `guard.rs`'s existing
`check_no_vowifi_conflict` already refuses that at the registration level). Two different SIMs, one
running VoWiFi and one running VoLTE, in the same container, is a real and already-supported
deployment shape. `ip netns` names are container-global, not subsystem-scoped, so two subsystems
independently choosing `"ims0"` would collide. A distinct prefix makes the two subsystems' namespace
pools disjoint sets by construction; a unit test (mirroring `vowifi::discovery`'s own
`assert_ne!(l0.config.netns, l1.config.netns)` pattern) asserts a VoLTE line's derived namespace is
never equal to any VoWiFi namespace of the same index, closing the loop rather than trusting the
prefix choice alone.

**Alternatives considered**:
- *Share one namespace-naming scheme across both subsystems, coordinated by index alone* — rejected:
  would require the two subsystems' discovery/derivation code to know about each other's line counts,
  which today they deliberately don't (`vowifi::discovery` and `volte::discovery` are independent
  modules) — a coordination requirement Constitution V would reject as an unneeded coupling for a
  problem a distinct string prefix already solves for free.

---

## R5: Moving the physical interface into its namespace — simpler than VoWiFi's tunnel case

**Decision**: `docker/entrypoint.sh` moves each line's already-existing LTE interface
(`enx*`/`wwan*`) into its namespace with `ip link set <iface> netns <netns>`, idempotently (mirroring
`ensure_epdg_interface`'s reuse-if-already-there check), **before** launching that line's
`volte-carrier-agent` subcommand — not an XFRM interface *creation* dance like VoWiFi's, because there
is nothing to create: the interface already exists in the default namespace as soon as the modem's
kernel driver enumerates it, independent of PDN/attach state (`volte/pdn.rs`'s own doc comment:
`AT+QNETDEVCTL=1,<cid>,1` *binds* the host netdev to a context — it does not create the netdev).

**Status**: ✅ Decided.

**Rationale**: `attach()` (`volte::attach`, called from inside the new subcommand) only touches the
modem's AT serial port, which is a character device under `/dev`, unaffected by which network
namespace the LTE interface sits in. So the move can happen first, unconditionally, and everything
downstream — `attach()`'s `AT+QNETDEVCTL` bind, `netcfg::configure()`'s `ip`/sysctl calls
(`netcfg.rs`), `wait_for_carrier`/`wait_for_router` — runs correctly with zero code change, because
the whole `volte-carrier-agent` process (and therefore every `Command::new("ip"|"sysctl")` it spawns,
and every socket it opens) is already inside that namespace by the time it starts.

**Idempotency** (FR-011): the move function checks, in order: is the interface already in the target
namespace (reuse — the common case on a warm restart where a prior clean shutdown didn't move it
back, or where teardown intentionally left it, see R6) → is it in the default namespace (move it) →
neither (log and wait/retry, matching the existing modem-port-presence check's shape in
`start_line_strongswan`/`start_line_swu`). This is the same three-way idempotency
`ensure_epdg_interface` already implements for the XFRM interface case, adapted to "move" instead of
"create-then-move".

**Alternatives considered**:
- *Move the interface only after `attach()` succeeds* — rejected: would require `attach()`'s AT
  commands to run from the default namespace (fine, they don't touch the network stack) but then a
  second cross-namespace step mid-subcommand to move the interface before `netcfg::configure()` runs
  — more moving parts than doing the move once, up front, in `entrypoint.sh`, before the subcommand
  that needs it even starts.

---

## R6: Teardown must run interface-scoped cleanup *inside* the namespace, before deleting it

**Decision**: The line's `volte-pdn down`/displaced-context restoration (`volte::mod::tear_down`,
which calls `netcfg::teardown(iface)` — `mod.rs:291-292`) MUST run while the interface is still
inside that line's namespace — i.e. invoked as `ip netns exec volteN ... volte-pdn --action down ...`
— not from the default namespace after the fact. Only once that has run does `entrypoint.sh`'s
cleanup trap delete the now-empty namespace (`ip netns del`), extending the existing
`STARTED_NETNS` array/loop VoWiFi's cleanup already uses.

**Status**: ✅ Decided; this is the one genuinely new failure mode this feature introduces if
missed, so it is called out explicitly rather than left implicit in "teardown is reused as-is".

**Rationale**: `netcfg::teardown()` issues `ip -6 addr flush dev <iface>`, sysctl writes to
`/proc/sys/net/ipv6/conf/<iface>/...`, and `ip link set <iface> down` (`netcfg.rs:109-126`) — all
interface-scoped commands that only succeed if run in the namespace the interface currently lives
in. Today (single shared namespace) this distinction does not exist; after this feature, running
these commands from the default namespace after the interface has been moved into `volteN` would
silently no-op or error on "interface not found", leaving the modem's data-path binding
un-restored — exactly the failure class `e50ddca` ("restore the displaced data context on
inbound-bridge teardown") already fixed once for the single-namespace case. This feature must not
reopen it. The container's existing `cleanup()` trap already special-cases VoLTE teardown
(`entrypoint.sh:91-134`) and already knows each line's `restore_cid_path` from the manifest — it
gains one more piece of per-line knowledge (its namespace) and one more per-line step (run the
teardown command through `ip netns exec` for that namespace) before the final `ip netns del` loop.

**A secondary, deliberately *not* relied upon, kernel behavior**: deleting a namespace that still
contains a physical (non-veth) interface moves that interface back to the default namespace rather
than destroying it. This plan does not depend on that behavior for correctness — the explicit
in-namespace teardown in R6 must still run first, exactly as `netcfg.rs`'s own teardown already
prefers explicit steps (`addr_gen_mode` reset) over assuming the kernel restores defaults on its
own — but it is a useful safety net for the unclean-shutdown case (FR-011): if the container is
`SIGKILL`ed before any trap runs, the next startup's R5 idempotency check finds the interface back in
the default namespace (kernel-restored) or still in the stale namespace (if the kernel version in use
does not restore it) — both cases are already covered by R5's three-way check.

**Alternatives considered**:
- *Rely solely on the kernel's move-back-on-delete behavior, skip the explicit in-namespace
  teardown* — rejected: makes correctness depend on kernel version/behavior for real device classes
  (documented but not something this project's own `netcfg.rs` otherwise trusts implicitly anywhere
  else), and skips restoring the *IPv6 configuration* the modem's host data path is left in
  (`addr_gen_mode`, addresses) — the namespace move-back only relocates the interface, it does not
  run `netcfg::teardown()`'s cleanup steps.

---

## R7: Scope boundary — which VoLTE entry points this feature touches

**Decision**: This feature isolates `volte-bridge`'s line table only (the inbound-bridging multi-line
service from specs/017/018). `volte-register`, `volte-call`, and `volte-listen` — single-modem,
single-invocation CLI verbs with no multi-line concept — are untouched.

**Status**: ✅ Decided; matches the spec's own Assumptions section, confirmed against `main.rs`'s
subcommand list.

**Rationale**: There is no second line for these commands' traffic to collide with — they are
single-shot, single-modem tools, mostly used for manual registration/outbound-call testing per
specs/015/016's quickstarts. Namespacing a command that never runs two lines at once adds isolation
with nothing to isolate from, which Constitution V (YAGNI) rejects outright.

**Alternatives considered**: *Namespace every VoLTE entry point uniformly, "for consistency"* —
rejected as complexity with no corresponding defect to close.
