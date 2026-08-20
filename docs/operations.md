# Operations Guide

## CLI Card Management

The `card` subcommands talk to the running daemon over its control socket (default: `/tmp/gsm-sip-bridge.sock`). The daemon must be running.

```bash
# Show all known slots: state, phone number, network type
gsm-sip-bridge --config config.toml card list

# Restart a slot (safe to run while other cards are active; resets give-up state)
# --mode: "full" (default, AT+CFUN=1,1 — a complete module reset, can move
# the card's ttyUSB path) or "radio" (AT+CFUN=0 -> AT+CFUN=1 — drops and
# re-acquires network registration without power-cycling the module or
# re-enumerating USB). Same two values as [scheduled_restart].restart_mode.
gsm-sip-bridge --config config.toml card restart --slot 0
gsm-sip-bridge --config config.toml card restart --slot 0 --mode radio

# Set network preference for a slot and persist it
# Valid modes: 2g, 3g, 4g, auto
gsm-sip-bridge --config config.toml card set-mode --slot 0 --mode 4g

# Query the stored network mode preference
gsm-sip-bridge --config config.toml card get-mode --slot 0
```

Network mode preferences survive daemon restarts and are re-applied automatically whenever a card initialises (cold start or after recovery).

## Querying the Store

Connect to the SQLite store directly:

```bash
sqlite3 /var/lib/gsm-sip-bridge/store.db
```

Useful queries:

```sql
-- Recent calls
SELECT * FROM recent_calls;

-- Recent SMS
SELECT * FROM recent_sms;

-- Calls by module
SELECT * FROM calls WHERE module_id = 'ec20-A1B2C3' ORDER BY id DESC LIMIT 20;

-- Failed SMS forwards
SELECT * FROM sms WHERE forwarding_status = 'failed';

-- IMEI → slot assignments
SELECT slot, imei, assigned_at FROM card_slots ORDER BY slot;

-- Stored network mode preferences
SELECT slot, mode, updated_at FROM card_mode_prefs ORDER BY slot;
```

## Manual Prune

The bridge does not auto-prune. Run periodically:

```sql
DELETE FROM calls WHERE started_at < datetime('now', '-365 days');
DELETE FROM sms WHERE received_at < datetime('now', '-365 days');
VACUUM;
```

## WAL Checkpoint

SQLite WAL files grow during writes. Force a checkpoint:

```sql
PRAGMA wal_checkpoint(TRUNCATE);
```

## Backup

```bash
sqlite3 /var/lib/gsm-sip-bridge/store.db ".backup /backup/store-$(date +%Y%m%d).db"
```

## Troubleshooting

### A line went unreachable and something restarted it

Look for this line in the agent's log (`/tmp/ims-agent-<line>.out`, also in
`docker logs`):

```
watchdog: the dispatch loop has made no progress
```

It carries `activity`, `phase`, `stalled_secs`, `budget_secs` and — most
usefully — `last_at_command`, the modem command the line was waiting on when it
stopped moving. The agent exits `70` and the supervisor restarts that line
within ~5s; a restarted line has been observed re-registering in ~150s.

This exists because of a 2026-08-16 incident in which a line was unreachable for
2h45m while every health signal reported it healthy. A modem had stopped
answering, and a re-registration blocked forever in `read(2)` on the serial port
— on the same thread that answers calls.

**Repeated stalls** escalate through the same ladder as a dropped USIM: three
strikes trigger an `AT+CFUN=0`→`1` SIM reset, and after five resets the line
alerts once and drops to a 15-minute retry cadence rather than restarting in a
tight loop. It keeps retrying deliberately — the common causes (a power blip, a
USB re-enumeration, a SIM reseating itself) clear on their own, and the line
then returns to service unattended.

**To keep a wedged line for diagnosis**, set `[vowifi].watchdog_recovery_enabled
= false`. The stall is still detected, logged and reported — only the restart is
suppressed. Do not leave this off in production: it reinstates exactly the
silent outage the watchdog exists to prevent.

### Is this line actually reachable right now?

Three surfaces, which must now agree:

```bash
gsm-sip-bridge -c /etc/gsm-sip-bridge/config.toml vowifi-status
#   expires_in: -9752s (LAPSED)
#   can_answer: false
#   blocked_reason: the registration has expired

wget -qO- http://127.0.0.1:9091/metrics | grep -E 'registration_expires_in_seconds|agent_up'
#   negative value = the binding has already lapsed

docker ps            # reports (unhealthy) while a resolved line's registration is expired
```

Before this feature, all three reported healthy for the whole outage:
`expires_at` was recorded but read by nothing, the Prometheus heartbeat came
from a thread independent of the dispatch loop, and the healthcheck only proved
the metrics port accepted connections.

### No `/dev/ttyUSB*` devices

Check `dmesg | grep ttyUSB`. Ensure the `option` and `qcserial` kernel
modules are loaded, and see [supported-hardware.md](supported-hardware.md).

### No audio device in `arecord -l`

USB Audio Class is not enabled on the module — follow the one-time UAC
setup in [supported-hardware.md](supported-hardware.md).

### ModemManager interfering with AT sessions

ModemManager probes `ttyUSB*` ports, corrupting AT sessions (the bridge
warns at startup if it is active):

```bash
sudo systemctl stop ModemManager
sudo systemctl disable ModemManager
```

### Permission denied on serial/audio devices

Add the user to the `dialout` and `audio` groups:

```bash
sudo usermod -aG dialout,audio $USER
```

### Module shows FAILED at startup

Check:
1. USB device connected: `lsusb | grep 2c7c:0125`
2. Serial port accessible: `ls -la /dev/ttyUSB*`
3. ModemManager not interfering: `systemctl is-active ModemManager`
4. Permissions: user must be in `dialout` group

### Card is in GivenUp state (stopped retrying)

A slot stops retrying after `[resilience] max_retries` consecutive failures and emits a `CRITICAL` log. To re-enable it:

```bash
gsm-sip-bridge --config config.toml card restart --slot <N>
```

This resets the give-up counter and triggers a fresh initialization attempt.

### Card recovery not triggering after USB re-plug

The bridge detects USB disconnect via a serial read error on the AT port. If the device re-enumerates but the slot stays in `Recovering`:
1. Check that the IMEI in `card_slots` matches the re-plugged modem (`sqlite3 store.db "SELECT * FROM card_slots;"`).
2. Verify no other process holds the ttyUSB port: `fuser /dev/ttyUSB*`
3. Force a restart: `gsm-sip-bridge --config config.toml card restart --slot <N>`

### Control socket not reachable

```
error: daemon not running or socket unreachable: /tmp/gsm-sip-bridge.sock
```

1. Verify the daemon is running: `ps aux | grep gsm-sip-bridge`
2. Check the configured socket path matches: `[control] socket_path` in `config.toml`
3. Check filesystem permissions on the socket directory

### SIP registration failing

Check:
1. PBX reachable: `nc -zuv <server> <port>`
2. Credentials correct in config.toml
3. Transport matches PBX (udp/tcp/tls)
4. If TLS: check `tls_verify` setting

### SIP call fails / busy

Verify `[bridge].sip_destination` is a valid, reachable extension on the
PBX (or empty for DID passthrough, with a matching PBX inbound route).

### No audio after SIP answers

Check logs for `call media active, audio connected to sound device`.
Verify the ALSA device is accessible and not claimed by another process
(`fuser /dev/snd/*`).

### Audio clicks / dropouts / choppy GSM audio

1. Ensure no other process claims the ALSA device: `fuser /dev/snd/*`
2. Watch for `alsa_capture_overrun` / `alsa_playback_underrun` warnings in
   the logs — raise `[audio] snd_rec_latency_ms` / `snd_play_latency_ms`
   and consider enabling `rt_audio_prio` (see
   [configuration.md](configuration.md#audio)).

### Docker container not finding USB/audio devices

The container must run with `privileged` and the `/dev` bind-mount to
access USB devices and ALSA (the shipped `docker/docker-compose.yml`
already does). Note this is *not* what `network_mode: host` is for —
device and audio access work the same in any network mode.

### VoWiFi: "failed to reach Agent B control channel: connection timed out"

The tunnel is up, the IMS registration succeeded and the carrier's `INVITE`
arrived — but Agent A (inside netns `ims0`, `ims1`, ... one per line) cannot
reach Agent B across the veth pair, so every inbound call fails.

Under `network_mode: host` (the shipped default) the veth's Agent B end —
`veth-sip0` for line 0, `veth-sip1` for line 1, etc. (`[vowifi].veth_sip_iface`
+ line index, always suffixed, even for line 0) — lives in the **host's**
network namespace, so Agent A's traffic arrives as *inbound host traffic* and
is filtered by the host firewall. A default-deny firewall (ufw, firewalld)
drops it. The giveaway is that ICMP still works —
`ip netns exec ims0 ping 10.99.0.2` succeeds while TCP to `10.99.0.2:7050`
times out.

With ufw, one rule per line's interface:

```bash
sudo ufw allow in on veth-sip0 from 10.99.0.1 comment 'gsm-sip-bridge VoWiFi agents'
```

Or one rule covering every line at once with ufw's interface wildcard
(`+` matches any suffix):

```bash
sudo ufw allow in on veth-sip+ comment 'gsm-sip-bridge VoWiFi agents, all lines'
```

Allow the **whole interface**, not just the control port: the call's RTP
audio crosses the same veth on PJSUA-allocated media ports (base 4000,
incrementing per call), so a rule for TCP/7050 alone yields a connected call
with no audio — a more confusing failure than no call at all. Each `veth-sipN`
is a private /30 whose only peer is that line's own netns. The rule keys on
the interface name, which survives the tunnel reconnects that delete and
recreate the pair.

VoLTE's per-line carrier-agent isolation (`specs/020-volte-line-netns`, netns
`volte0`/`volte1`/..., veth `veth-volte-sip`+index) hits the identical class
of failure for the identical reason — same fix, different interface prefix.

Not an issue under bridge networking, where the veth's host end sits in the
container's own namespace, out of the host firewall's reach.

### VoWiFi: registration is granted, then torn down seconds later

Symptom: `REGISTER response status=200`, immediately followed by
`NOTIFY reports a terminated state` carrying `event="deactivated"` and
`reason=noresource` for our own contact — after which terminating calls never
arrive.

The modem's own IMS/VoLTE stack is registered too. Our `REGISTER` carries
`+sip.instance="<urn:gsma:imei:$IMEI>"` — the modem's IMEI — so a
VoLTE-registered modem claims the same IMPU with the same instance-id, and per
RFC 5626 the network treats one registration as a re-registration of the other
and deactivates the older binding. The modem wins, and the bridge can never
receive a call.

Since v6.2.0 the entrypoint reconciles this automatically on boot
(`AT+QCFG="ims"` must be `2` when `[vowifi].enabled`), rebooting the module if
it was wrong. If it fails, check the modem supports `AT+QCFG="ims"` at all —
`ims_conf=1` with `volte_cap=1` is the state that causes this.

### VoWiFi: a line re-establishes its tunnel every ~30 seconds

Symptom: one line never stays up. Its supervisor logs
`tunN missing from netns imsN; recreating and forcing reinitiate` on every
steady-state tick, `swanctl --list-sas` shows the IKE_SA repeatedly
ESTABLISHED with a fresh CHILD_SA, and the charon log alternates
`installing <addr> on tunN` / `removing <addr> from tunN`. The line's Agent A
fails with `Network unreachable` reaching its P-CSCF, because the tunnel
interface it should route over is not there.

Cause: the line's XFRM `if_id` is still claimed, so the interface cannot be
created:

```
$ ip link add tun23-1 type xfrm if_id 24
RTNETLINK answers: File exists
```

An XFRM interface registers its `if_id` in the namespace it was **created**
in, not the one it lives in. `supervise` creates `tun23-N` in the host
namespace and then moves it into `imsN`, so the device holds the id against
the host while being invisible to `ip link show` there. `ip -d link show type
xfrm` run *inside* `imsN` shows it, with `link-netnsid 0` pointing back at the
host namespace it is registered in:

```
$ docker exec <container> ip netns exec ims0 ip -d link show type xfrm
tun23-0@NONE: ... link-netnsid 0
    xfrm if_id 0x17 ...
```

So a healthy, fully registered deployment refuses `if_id 23` and `24` in the
host namespace *while it is running* — that refusal, by itself, is not
evidence of a problem.

**Before specs/041-shutdown-resource-cleanup (any release before this one),
this symptom after a restart was expected and self-clearing** — measured
2026-07-31: 163s/195s for a container replaced immediately, against 11s for
one restarted after a 3-minute stop. The stop back then only sent every
process a signal and removed the *name* of each namespace (`ip netns del`);
it never deleted the device itself, so the id stayed claimed until the old
container's leftover mount namespace was reaped on its own, typically ~2.5
minutes later.

**Since specs/041-shutdown-resource-cleanup, a graceful stop deletes the
device explicitly** — waiting for every child to exit, terminating each
line's IKE_SA, flushing this deployment's own XFRM state, then `ip link del`
on the tunnel interface and the veth pair before deleting the namespace (see
`supervise::shutdown` and the log markers below). Restarting after a normal
`docker compose stop`/`restart` should no longer show this at all; if it
does, something in that sequence did not complete, and where to look depends
on how the previous run ended:

- **The previous run's stop is still in progress.** `stop_grace_period` in
  `docker-compose.yml` (60s by default) should be long enough; if the
  supervisor's log shows `[supervise] teardown: out of time, skipping ...`,
  the stop allowance ran out mid-teardown and the fallback prioritised
  releasing devices over waiting — check what it reported it could not
  release, named in a `[supervise] teardown: could not release ...` line.
- **The previous run was force-killed, or the machine lost power.** No
  graceful stop ran at all. On start, `supervise` looks for exactly this —
  a namespace matching this deployment's own naming that it did not just
  create — and reclaims it (`[supervise] reclaimed netns ... left by a
  previous run` in the log). This needs the per-line namespace directory to
  be visible from the host, which is what `docker-compose.yml`'s
  `/var/run/netns` bind mount (`rshared` propagation) is for. If that mount
  is missing, misconfigured, or the host's Docker version does not propagate
  it the way this deployment assumes, reclamation silently finds nothing —
  confirm with `ip netns list` **on the host** (not inside the container):
  a namespace from a killed run should be visible there.
- **Something outside this deployment is holding it.** Genuinely rare, but
  possible — see the "held open by something a process scan misses" checks
  below.

To confirm which case you're in, watch the id from a throwaway privileged
container that can see every namespace on the host (`--pid=host`, which the
bridge container itself cannot do):

```
$ docker run --rm --privileged --pid=host --net=host --entrypoint sh \
    <bridge-image> -c 'ip link add zz type xfrm if_id 24 && ip link del zz'
```

If it is still refused more than a few seconds after the previous run
actually exited (`docker inspect -f '{{.State.Status}}' <old container>` —
confirm it is not still stopping), look for a namespace held open by
something a process scan misses, which would keep its interfaces and their
if_ids alive with nothing in `ip netns list` to show for it:

```
$ find /proc/*/fd -lname 'net:*' 2>/dev/null    # netns held open by an fd
$ grep -l nsfs /proc/*/mountinfo                # netns kept by a bind mount
$ dmesg | grep unregister_netdevice             # a device stuck unregistering
```

A host reboot is the reliable clear if it ever comes to that.

Separately, if `supervise` logged that it found XFRM state which is *not* this
deployment's and left it alone, that is a real (and different) condition —
stale SAs and policies from something else on the host, which it will not flush
because the flush is unfiltered and iproute2 has no
`ip xfrm policy deleteall if_id N`. That guard applies identically at start
(`reclaim_stale_xfrm`) and at stop (the `FlushXfrm` teardown step) — overriding
it is yours to do, not the supervisor's:

```
$ docker stop <container>
$ ip xfrm policy flush && ip xfrm state flush
$ ip link del veth-sip0; ip link del veth-sip1     # any that remain
$ docker start <container>
```

Run that only when you know nothing else on the host uses IPsec.

### VoWiFi: "no smart card reader" / vpcd connection refused

Symptom: charon logs `SCardListReaders: Cannot find a smart card reader`
and `no USIM found with quintuplets ...`, while `vowifi-usim-bridge`
restarts forever on `failed to connect to vpcd ... Connection refused`.

Both are the same fault: pcscd never registered the vpcd virtual reader,
so nothing listens on `[vowifi].vpcd_port`. Check the `[pcscd]` lines in
`docker compose logs`. If they say:

```
Address in use
ifd-vpcd.c:130:IFDHCreateChannel() Could not initialize connection to virtual ICC
```

then something already holds the port when pcscd starts. This bites
specifically under `network_mode: host`, where the container shares the
host's network namespace: vsmartcard's upstream default (35963) sits
inside the kernel's ephemeral port range
(`cat /proc/sys/net/ipv4/ip_local_port_range`, typically 32768-60999), so
any outbound connection on the host can randomly squat it — an
intermittent failure that looks like a modem or SIM problem but is not.

The default `vpcd_port` (15963) is below that range and is therefore safe.
If you override it, keep it below the ephemeral range too. Reserving the
port instead (`net.ipv4.ip_local_reserved_ports`) also works, but it is a
host-wide kernel setting and will not evict a connection already holding
the port.

### VoWiFi modem line silently drops out of `resolved lines` after a replug

Two symptoms, usually together, after physically unplugging and
replugging a modem used as a VoWiFi line:

**The pinned `[[vowifi.line]] modem_port` stops matching anything.** A USB
serial device's `ttyUSB*` number is assigned by enumeration order, not
tied to the physical device — a replug can hand it a different number
than it had before. `discover: LINE_COUNT=N` drops by one, and the
supervise log's `line N (...)` list is missing the modem entry, with no
error explaining why: the override just never matches a probed device.
Fix: drop the `modem_port` pin and let full auto-discovery find the modem
on whichever port it lands on this boot (omit `[[vowifi.line]]` for that
modem entirely, or keep the block for other fields but remove
`modem_port`) — see `sample_configs/vowifi-only-cs-disabled.toml`. Pinning
a specific `ttyUSB` number is only safe on hardware that's never
physically replugged.

**The SIM reads as unusable even though the modem answers plain `AT`
fine.** `gsm-sip-bridge discover` (or the daemon's own scan) logs
`modem's SIM is not usable ... reason=Unreadable("13")` /
`sim_unreadable: 13` — `CME ERROR: 13` is 3GPP's "SIM failure". Confirm
directly against the modem's AT port with a terminal that both sends and
reads back (`minicom -D /dev/ttyUSBn -b 115200`, `screen /dev/ttyUSBn
115200`, or any single-command AT-probe tool you have — a plain `echo >
device` only writes, it won't show the reply):

```
AT+CPIN?
+CME ERROR: 13
```

This is the SIM having electrically "fallen off the bus" — common right
after a replug, before the module has fully reseated it. A radio
power-cycle over AT, not a container or modem restart, usually clears it:

```
AT+CFUN=0    -> OK
AT+CFUN=1    -> OK, wait ~10s
AT+CPIN?     -> +CPIN: READY
```

Once `AT+CIMI` returns a valid IMSI again, restart the bridge (or
`supervise`'s container) so `discover` re-resolves the line — it does not
re-scan on its own mid-run, **unless** this was the *only* configured
VoWiFi line and it was still missing when the container started — see the
next section, which covers that case without a restart.

### VoWiFi: a configured line was missing at startup ("NOT RUNNING" / `not_found`)

specs/027-discover-retry-health closes the gap the previous section still
describes for the general case: `discover`'s single startup scan can miss
a configured `[[vowifi.line]] modem_port`/`modem_serial` line simply
because the USB device hadn't finished enumerating yet (a real incident:
an EC20 modem present on the bus the whole time, just not yet visible to
`/sys/bus/usb/devices` at the exact moment `discover` ran). This used to
be completely silent — no log line, `vowifi-status` didn't mention it,
`healthcheck`/`docker ps` reported healthy regardless. It no longer is:

- `gsm-sip-bridge vowifi-status` prints a `Configured line <id> (from
  config.toml): NOT RUNNING` / `reason: not_found` block for it, right
  after the lines that did resolve.
- The container's `HEALTHCHECK` (and `docker ps`) reports unhealthy while
  it's in that state.
- If `[alerts.line_discovery_failed].enabled = true`, a Discord
  notification fires once, and a paired recovery notification fires if it
  later self-heals.

**What actually happens automatically, and what still needs a restart:**

- If this was the *only* configured VoWiFi line (so `discover`'s first
  pass resolved zero lines at all), the bridge retries `discover` every
  ~10s for up to ~3 minutes on its own — watch for `[supervise] line
  discovery: ... previously-missing configured line(s) now found ...` in
  the container logs, or just re-run `vowifi-status` a little later. No
  restart needed; this is the common case for a single-modem deployment
  and is exactly what fixes a slow-enumerating modem.
- If *another* configured line already resolved and started (a
  multi-line deployment where only one line was missing), this specific
  line will **not** self-heal even after the underlying hardware becomes
  reachable — the shared tunnel daemon's P-CSCF plugin only reads its
  enabled-connections list once at its own process start, and restarting
  it to pick up a late line would drop every other line's already-live
  calls. Confirm the modem now answers plain `AT` (see the previous
  section), then restart the bridge/container to pick it up.
- If it's still `NOT RUNNING`/`not_found` well past ~3 minutes after
  startup with *no* other line configured, the retry window has already
  elapsed and given up for this run (that's the Discord alert firing) —
  confirm the hardware, fix it, and restart.

### Discord forwarding failing

Check:
1. Webhook URL valid (test with curl)
2. Network connectivity from bridge host
3. Check `sms` table for `forwarding_status = 'failed'` with `discord_status_code`

### Discord alerts/forwarding fail only *inside* the container (host is fine)

Symptom: `curl https://discord.com/` works on the host, but nothing is ever
delivered and the daemon reports DNS errors. The give-away is that the
container's resolver has been replaced by carrier addresses:

```bash
docker exec <bridge-ctr> cat /etc/resolv.conf
#   nameserver 2405:200:800::1   # by strongSwan   <-- carrier's, replacing yours
docker exec <bridge-ctr> getent ahostsv4 discord.com   # exits 2, no output
```

Cause: strongSwan's `resolve` plugin writing the ePDG's assigned
`INTERNAL_IP4_DNS`/`INTERNAL_IP6_DNS` into `/etc/resolv.conf`. It *replaces*
the resolvers Docker put there at container start rather than adding to them,
so the container is left with a single carrier-controlled nameserver and no
fallback. It recurs on every IKE re-auth, so restarting only helps until the
next one. Confirm from the tunnel log:

```bash
docker exec <bridge-ctr> grep "installing DNS server" /tmp/charon.log
```

Whether this presents as a hard outage depends on whether the host has a route
to the assigned server, which is not a property you control:

- No IPv6 default route → the assigned v6 resolver is unreachable and
  *nothing* resolves (`ip route get <addr>` says `Network unreachable`).
- A v6 default route present (e.g. the cellular bearer came up dual-stack) →
  it resolves, but every lookup now leaves over that bearer instead of your
  LAN, and one `REFUSED` from the carrier takes out all alerting with no
  second nameserver to fall back to. The assigned *v4* resolver has been
  observed returning `REFUSED` while its v6 sibling answered fine.

The same host can flip between these without any config change. Fixed in the
image by `docker/strongswan/resolve.conf`, which points the plugin at a scratch
file (`/run/ims-resolv.conf`) and leaves the system resolver alone. If you see
this, you are running an image from before that change — rebuild/pull and
recreate the container. To confirm the fix took:

```bash
docker exec <bridge-ctr> cat /etc/resolv.conf        # your resolvers, no strongSwan line
docker exec <bridge-ctr> cat /run/ims-resolv.conf    # carrier IMS DNS lands here instead
```

### Metrics endpoint returns 5xx

Check:
1. Port not in use: `ss -tlnp | grep 9091`
2. Bridge process running: `ps aux | grep gsm-sip-bridge`

### Store.db corrupt

1. Stop the bridge
2. Run: `sqlite3 /var/lib/gsm-sip-bridge/store.db "PRAGMA integrity_check;"`
3. If corrupt, restore from backup
4. Restart the bridge (it will create a fresh DB if needed)

## Host-side IMS over LTE (VoLTE)

`specs/015-volte-host-ims`. The bridge runs **its own** IMS registration over
an LTE IMS PDN, instead of delegating to the modem's internal IMS stack and
re-bridging its decoded audio. Opt in with `[volte].enabled`; the `volte-*`
subcommands work as standalone diagnostics without it.

All of them need `CAP_NET_ADMIN` — run them inside the container.

```bash
gsm-sip-bridge volte-pdn --action up --iface <ifname>   # attach the IMS PDN
gsm-sip-bridge volte-discover --iface <ifname>          # what does the carrier publish?
gsm-sip-bridge volte-register                           # register, then keep it alive
gsm-sip-bridge volte-status --iface <ifname>            # attachment + registration state
gsm-sip-bridge volte-pdn --action down --iface <ifname> # release, restoring the previous binding
```

### Never enable VoWiFi and VoLTE on the same SIM

Both register the same IMPU with the same IMEI-derived `+sip.instance`, so per
RFC 5626 the network treats one registration as a re-registration of the other
and deactivates the older binding — the same failure documented above for the
modem's internal IMS stack. `volte-register` refuses to start while a
`vowifi-ims-agent` is running (override with `--force` only when deliberately
testing this), and `supervise` refuses to start at all if both sections are
enabled.

### The P-CSCF usually has to be captured, not discovered

On the tested carrier (Vodafone India) **no automatic mechanism yields a
P-CSCF**: DHCPv6 replies but carries no RFC 3319 SIP-server options, the router
advertisement carries none, and no usable resolver is offered. `volte-discover`
reports this per-method rather than failing opaquely — an empty result there is
the expected outcome, not a fault.

The working route is to let the VoWiFi/ePDG path capture one: each line writes
the address it learned from the IKEv2 config payload to
`[vowifi].pcscf_source_path` with its line index appended (`/tmp/pcscf-0`,
`/tmp/pcscf-1`, ...). `volte-register` reads whichever file
`[volte].pcscf_source_path` names — `/tmp/pcscf-0` by default, i.e. VoWiFi line
0. So running VoWiFi once on the SIM primes the LTE path; with several VoWiFi
lines, point it at the one whose carrier you want, since each line's P-CSCF
comes from its own network. `--pcscf` overrides everything.

### Symptom: attached but nothing works

`volte-pdn --action up` reports `routable: NO — no default route`.

The carrier **unicasts its router advertisements to the link-local form of the
interface identifier it assigned**, not to `ff02::1`. If the host uses its own
generated link-local, every RA is silently discarded and the PDN looks dead
while the RAs are arriving the whole time. The bridge handles this
(`addr_gen_mode=none` plus the identifier from `AT+CGPADDR`), so seeing this
means something upstream failed — check that the interface has carrier and that
`AT+QNETDEVCTL?` reports the IMS context bound.

Note that "attached" and "usable" are different states: the assigned address is
installed by the bridge regardless, so **the default route — not the presence
of an address — is what proves the RA was accepted**.

### Symptom: general connectivity through the modem disappears

Expected. The module exposes a single host-facing data path, so binding the IMS
PDN displaces whatever it carried before. `volte-pdn --action down` restores the
previous binding, and the container does the same on shutdown.

### Metrics

| Metric | Meaning |
|---|---|
| `gsm_sip_bridge_volte_registered` | 1 when the host-side LTE registration is accepted |
| `gsm_sip_bridge_volte_pdn_up` | 1 when the IMS PDN is attached **and routable** |
| `gsm_sip_bridge_volte_registrations_total{outcome}` | `accepted` / `renewed` / `rejected` / `renewal_failed` |

Deliberately separate from `gsm_bridge_sip_registered` (the PBX side) and from
the VoWiFi agent's gauges — when something is down you need to know *which*
registration, not that one of them is.

### Bridging incoming calls (`[volte].bridge_inbound`)

`specs/017-volte-inbound-bridge`. With `enabled` alone, the LTE registration is
held open and nothing more. Adding `bridge_inbound = true` makes the bridge
**answer incoming calls on it** and connect them through to the PBX:

```toml
[volte]
enabled = true
bridge_inbound = true
```

`supervise::orchestrate_volte` then supervises `volte-bridge` in place of
`volte-register`.
Run it by hand the same way:

```bash
gsm-sip-bridge volte-bridge --iface <ifname>
```

**This is opt-in, and unset means unchanged.** A config written before this
feature keeps behaving exactly as it did, with the modem-internal path still
available.

#### What it costs: the card becomes exclusive

A card assigned here belongs to this service alone. The circuit-switched daemon
will not drive it, so **while this path is down, that card takes no calls at
all** — there is no fallback. That makes the health signals load-bearing rather
than decorative:

| Watch | Because |
|---|---|
| `gsm_sip_bridge_volte_registered` | 0 means calls are being missed, not merely delayed |
| `gsm_sip_bridge_volte_pdn_up` | attached-but-unrouted is a real, observed state |
| `gsm_sip_bridge_active_calls{transport="volte"}` | this path's calls, distinct from `vowifi` and `cs` |

Call and message records carry `transport="volte"`, a third value on the
*existing* label rather than a new metric — existing dashboard queries keep
matching unchanged. A panel that explicitly *groups by* transport will gain a
series.

#### One call at a time

The bridge fronts a single subscriber line. A second concurrent call is refused
as busy rather than queued, and the refusal does not disturb the call in
progress.

#### Maintenance yields to a call

The carrier tears the LTE attachment down roughly every two hours and the
service re-attaches automatically. Both that re-attachment and registration
renewal are **deferred while a call is in progress** — either one mid-call
would take the call down with it. A call is deliberately allowed to outlive its
registration rather than be cut short.

Consequence worth knowing before it surprises you: a long call can leave the
registration lapsing slightly late. That is the intended trade, not a fault.

#### Text messages still arrive

Holding this registration means the network delivers the subscriber's texts
here. Both delivery routes are handled — over the registration, and via the
modem's own storage — converging on the same record-and-forward path, recorded
exactly once even if both routes deliver the same message.

Messages are acknowledged **after** being recorded, never before, so an
ill-timed crash costs a retransmission (which is de-duplicated) rather than a
silently lost text.

## SIP server mode

`specs/024-sip-server-mode`. The bridge **is** the SIP server: IP phones
REGISTER directly to it and inbound calls (from any of the three carrier paths)
ring one configured account. No PBX in the deployment. Opt in with
`[sip_server].enabled`.

### Enabling it

```toml
[sip]
# The bridge's own calling port. MUST differ from listen_port below — they are
# two separate SIP endpoints and cannot share one UDP socket. Both default to
# 5060, so this line is not optional.
local_port = 5062
transport  = "udp"
# server / username / password are NOT set: there is no PBX. Leaving them in
# is a startup error, not a warning.

[sip_server]
enabled     = true
listen_port = 5060          # what the phones point at
realm       = "gsm-sip-bridge"
ring_aor    = "1001"        # which account rings; must match an account below

[[sip_server.account]]
username = "1001"
password = "env:PHONE_1001_PASSWORD"
```

Every mistake in that set is refused at startup with a message naming the fix —
a missing account, a `ring_aor` matching none of them, a leftover PBX address,
or both ports left at 5060. None of them fail silently at call time.

Expected on start:

```
INFO sip_server: registrar listening addr=0.0.0.0:5060 realm=gsm-sip-bridge accounts=1 ring_aor=1001
INFO SIP server mode active — IP phones register here; no PBX is used
```

### Provisioning a handset

| Setting | Value |
|---|---|
| SIP server / registrar | the bridge's LAN IP |
| Port | `[sip_server].listen_port` |
| Username / auth ID | an `[[sip_server.account]].username` |
| Password | that account's password |
| Transport | UDP |

Confirm it took:

```bash
curl -s localhost:9100/metrics | grep sip_server
```

`sip_server_bindings 1` and `sip_server_ring_aor_registered 1` is a working
deployment. One `registrations_total{outcome="challenged"}` per `accepted` is
normal — every REGISTER is challenged once and succeeds on the retry.

### Troubleshooting

**The phone registers but never rings.** Check the handset's "accept SIP only
from the proxy" option — *Accept SIP Trust Server Only* on Yealink, *Accept
Incoming SIP from Proxy Only* on Grandstream, similar on Fanvil. **Turn it
off.** The bridge answers REGISTER on `listen_port` but dials from
`[sip].local_port`, and although those handset options compare source *IP*
(identical here) rather than port, this is the first thing to rule out. It is
the one operational cost of the two-port design; see
[architecture.md](architecture.md#two-sip-side-topologies).

**`no live registration for AOR "1001"` in the log.** A call arrived with
nothing registered under the ringing account. The carrier call is deliberately
left to ring out rather than answered into silence, so the existing missed-call
alert still fires. Check `sip_server_ring_aor_registered` — if it is 0, the
handset is not registered, whatever `sip_server_bindings` says about others.

**Registrations refused.** `registrations_total{outcome="rejected_auth"}` means
wrong credentials; `rejected_unknown_user` means the handset is claiming an
account that is not configured. The wire response is identical for both on
purpose — so the registrar cannot be used to discover which account names
exist — which makes these labels the only way to tell them apart.

**Calls stop ringing after some minutes.** The handset's registration lapsed
and was not refreshed. `ring_aor_registered` will read 0. Check the handset's
registration interval against `[sip_server].min_expires`; a phone asking for
less than the floor is told `423 Interval Too Brief` and may then give up
rather than retry with a longer value.

**`registrar could not listen on ...: Address already in use`.** Something else
holds the port — most often a previous run still shutting down, or an
`asterisk`/`kamailio` left on 5060.

### Limitations

- **Inbound only by default.** A phone cannot dial out through the mobile
  network unless `[outbound].enabled` is also set — without it, the attempt is
  refused with `403` and logged. See [Outbound calling](#outbound-calling)
  below.
- **One phone rings.** Others may register but are never called. Registering a
  second device on the *same* account replaces the first, so calls go to
  whichever registered most recently.
- **UDP only**, and no NAT between phone and bridge — this targets a
  single-site LAN.
- **No message-waiting or busy-lamp** subscriptions (answered `489 Bad Event`).

## Outbound calling

`specs/025-outbound-calling`. Reverses the normal direction: instead of only
answering calls the mobile network delivers, the bridge places one on
request — a PBX-sent `INVITE` to its trunk account, or (in
[SIP server mode](#sip-server-mode)) any currently-registered phone's own
`INVITE`, redirected back to the bridge instead of refused. Opt in with
`[outbound].enabled`; off by default. See
[configuration.md](configuration.md#outbound) for the setting and
[architecture.md](architecture.md#outbound-calling) for how a request is
validated and routed to a line.

### Enabling it

```toml
[outbound]
enabled = true
```

Nothing else to configure — the destination is the Request-URI's user part,
dialed verbatim on whichever line (circuit-switched, VoWiFi, or VoLTE) is
idle, no path preference and no allow-list. Restricting who may reach the
bridge and what they may dial is left to the PBX's dial plan and network
access controls, or, in SIP server mode, `[[sip_server.account]]`
credentials.

### Troubleshooting

**`403 Forbidden` on every dial-out attempt.** Either `[outbound].enabled` is
still `false`, or (SIP server mode only) the phone placing the call is not
currently registered — the registrar only redirects an INVITE from a source
address it already holds a live binding for, never an unauthenticated one.

**`503 Service Unavailable`.** No idle line in the process that received the
INVITE. Line selection is per-process with no cross-process fallback: if the
circuit-switched daemon owns the SIP side and every EC20 is busy, the call is
refused even if a VoWiFi/VoLTE line happens to be idle — see
[architecture.md#outbound-calling](architecture.md#outbound-calling).

**`484 Address Incomplete`.** The Request-URI's user part failed the
destination check (empty, or otherwise not dialable). The bridge dials it
verbatim, so check what the PBX or phone actually put there.

**Track outcomes** via `gsm_sip_bridge_outbound_attempts_total{outcome=...}`
— `placed`, `refused_no_idle_line`, `refused_invalid_destination`,
`refused_network_failure`, `unanswered`.

### Limitations

- **Coarse progress on the circuit-switched path.** `ATD`'s own response only
  confirms dialing started, not that the destination answered, so a
  CS-originated call is accepted (`200`) once dialing is confirmed rather
  than once truly answered.
- **The VoWiFi/VoLTE path blocks its own dispatch loop** for up to ~80s while
  a call is in flight. During that window a caller hanging up mid-ring cannot
  trigger a CANCEL, and an unrelated inbound call on that same process is
  dropped. See `docs/todo.md`.
