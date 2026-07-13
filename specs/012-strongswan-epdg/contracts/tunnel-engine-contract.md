# Contract: Tunnel Engine ↔ VoWiFi Bridge Agents

The interface every tunnel engine (`swu` today, `strongswan` new) must satisfy so the 011
agents (`vowifi-ims-agent`, `vowifi-sip-agent`) run unchanged (FR-006/FR-007). The
counterparty for most obligations is `docker/entrypoint.sh`'s shared (engine-independent) tail:
veth creation, agent supervision, keepalive.

## Obligations of the engine (+ its entrypoint branch)

1. **Namespace**: a network namespace named `$NETNS` (default `ims`) exists and contains the
   tunnel interface before the shared tail runs. `lo` is up inside it.
2. **Tunnel interface & routing**: an interface inside `$NETNS` carries the carrier-assigned
   inner address(es), with default route(s) (`0.0.0.0/0` and/or `::/0`) via the tunnel, such
   that the P-CSCF (and any carrier RTP address) is reachable from inside `$NETNS`.
   - `swu` engine: `tun1`, split-default routes, installed by the dialer.
   - `strongswan` engine: XFRM interface (`tun23`-style name, `if_id 23`) pre-created by the
     entrypoint, address installed by the `ims.updown` script, `disable_policy=1` set.
3. **P-CSCF publication**: after each successful (re)establishment, exactly one P-CSCF IP
   (IPv4 preferred; IPv6 only if no IPv4 was assigned) is written to `$PCSCF_FILE`
   (default `/tmp/pcscf`, one address + newline) **before** agents are (re)started — and
   refreshed on reconnect if the assignment changed.
4. **Readiness signal**: the entrypoint branch must not release the shared tail until the
   tunnel is verified up:
   - `swu`: `STATE CONNECTED` in `/tmp/swu.log` (existing behavior).
   - `strongswan`: `CHILD_SA … established` in the charon filelog (backstop: `swanctl
     --list-sas` shows the `ims` CHILD_SA), then P-CSCF extracted.
5. **Namespace stability (new invariant, strongSwan only — the point of this feature)**:
   rekey, re-auth, DPD-triggered reconnect, and WAN-outage recovery MUST NOT delete `$NETNS`
   or the tunnel interface; only SAs and interface addresses may change. (The `swu` engine is
   exempt — it violates this by design, which is why the shared tail keeps the veth
   half-pair rebuild check as long as `swu` remains selectable.)
6. **Unattended recovery**: the engine retries establishment indefinitely (supervised loop for
   `swu`; `keyingtries = 0` + re-initiate supervision for `strongswan`). Giving up permanently
   is a contract violation (FR-004).
7. **Idempotent startup** (FR-011): pre-existing namespace/interface/SA/xfrm state from a
   previous run must be absorbed or replaced, never a fatal error.
8. **Logs** (FR-010): state transitions (connecting / established / rekeyed / re-authenticated /
   disconnected / retrying) visible in `docker logs` output, with the underlying engine log
   accessible in the container (`/tmp/swu.log` / `/tmp/charon.log`).

## Obligations of the shared tail (unchanged from 011)

- Creates/repairs the veth pair (`$VETH_SIP` ↔ `$VETH_IMS` in `$NETNS`), addresses per
  `VETH_*_ADDR`, after readiness.
- Supervises `vowifi-ims-agent` (inside `$NETNS`) and `vowifi-sip-agent` (default netns),
  restart-on-exit with 5 s backoff.
- Runs the TCP keepalive to `$PCSCF:5060` from inside `$NETNS` every `$KEEPALIVE_INTERVAL`
  seconds (inner-path idle-timeout defense; independent of IKE-level DPD/NAT-T keepalives).

## Agent-visible guarantees (what the agents may assume — unchanged)

- `VowifiConfig.pcscf_source_path` (default `/tmp/pcscf`) contains a routable P-CSCF IP when
  the agent starts.
- The agent runs inside a namespace where that IP is reachable and the veth peer
  (`veth_peer_addr`) is reachable.
- Nothing else: agents never talk to charon, pcscd, vpcd, or the SWu dialer.

## Selection

- `TUNNEL_ENGINE=swu` (default during proving) | `strongswan`. Unknown value → fatal at
  startup. Selection changes require only a container env change + restart (SC-005).
