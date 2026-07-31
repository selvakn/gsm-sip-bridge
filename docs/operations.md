# Operations Guide

## CLI Card Management

The `card` subcommands talk to the running daemon over its control socket (default: `/tmp/gsm-sip-bridge.sock`). The daemon must be running.

```bash
# Show all known slots: state, phone number, network type
gsm-sip-bridge --config config.toml card list

# Restart a slot (safe to run while other cards are active; resets give-up state)
gsm-sip-bridge --config config.toml card restart --slot 0

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

Cause: the line's XFRM `if_id` is still claimed by something, so the interface
cannot be recreated. The supervisor detects it missing, recreates it, fails,
and tears down a perfectly healthy CHILD_SA to retry — forever.

This is confusing to confirm because everything visible says the if_id is
free: `/var/run/netns` is empty, no `gsm-sip-bridge`/`charon` process
survives, and no interface of that name exists in any namespace you can
enumerate. The kernel still refuses it:

```
$ ip link add tun23-1 type xfrm if_id 24
RTNETLINK answers: File exists
```

Confirm by checking whether *other* if_ids are free — if a nearby unused one
works, the name is not the problem, the if_id is:

```
$ ip link add probe type xfrm if_id 99 && ip link del probe   # succeeds
```

There are two distinct causes, and they need different remedies.

**Cause A — the line's own live SA holds it.** This is the common one, and
since 2026-07-31 the supervisor recovers from it by itself: the steady-state
`TunVanished` branch terminates the line's IKE_SA *before* recreating the
interface, rather than after. (Recreating first cannot work here — the kernel
refuses, the terminate releases the id a moment too late, and the reinitiate
immediately re-claims it. That inversion was the livelock.) If you are on an
older build, do by hand what the fixed branch now does, then leave the
supervisor to its next tick:

```
$ docker exec <container> swanctl --terminate --ike ims1
```

An `ip link add` issued immediately after the terminate may still fail; the
release is not synchronous. Only the named line is disturbed, so this is safe
to run while other lines are registered.

**Cause B — leftovers from an earlier container run.** XFRM state/policy and
namespaced interfaces both outlive the container:

```
$ ip xfrm policy | grep -c '^src'      # ours carry if_id 0x17, 0x18, ...
$ ip xfrm state  | grep -c '^src'
$ ip link show | grep veth-sip         # leaked default-netns veth ends
```

`supervise` already clears this class automatically at every startup, before
charon starts (`reclaim_stale_xfrm`) — but only when it can prove every entry
present is this deployment's, since the flush is unfiltered and iproute2 has
no `ip xfrm policy deleteall if_id N`. If it logged that it found foreign
XFRM state and left it alone, that judgement is yours to make and override:

```
$ docker stop <container>
$ ip xfrm policy flush && ip xfrm state flush
$ ip link del veth-sip0; ip link del veth-sip1     # any that remain
$ docker start <container>
```

Run that only when you know nothing else on the host uses IPsec. A host
reboot clears the same state.

**If neither works**, the id is leaked at kernel level and reachable from
nothing — observed 2026-07-30 with a flushed host, no state or policy naming
`0x18`, and no interface holding it in `ims0`/`ims1`, the host netns, or any
live process's netns. Two places worth checking that a process scan misses,
since a namespace held by either keeps its interfaces (and their if_ids)
alive with nothing in `ip netns list` to show for it:

```
$ find /proc/*/fd -lname 'net:*' 2>/dev/null    # netns held open by an fd
$ grep -l nsfs /proc/*/mountinfo                # netns kept by a bind mount
```

Failing that, a host reboot is the reliable clear.

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

### Discord forwarding failing

Check:
1. Webhook URL valid (test with curl)
2. Network connectivity from bridge host
3. Check `sms` table for `forwarding_status = 'failed'` with `discord_status_code`

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
