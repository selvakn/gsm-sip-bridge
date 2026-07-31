# Quickstart: SIP Server Mode

**Feature**: 024-sip-server-mode

Run the bridge with no PBX: point an IP phone straight at it and take mobile
calls on the handset.

---

## 1. Configure

Add to `config.toml`:

```toml
[sip]
# The bridge still needs its own SIP port for the leg it dials out on, and it
# must differ from the registrar's. Leave 5060 for the phones.
local_port = 5062
transport  = "udp"
# server / username / password are NOT set — there is no PBX.

[sip_server]
enabled     = true
listen_port = 5060          # what you point the phone at
realm       = "gsm-sip-bridge"
ring_aor    = "1001"        # which account rings

[[sip_server.account]]
username = "1001"
password = "env:PHONE_1001_PASSWORD"
```

Export the password:

```bash
export PHONE_1001_PASSWORD='a-long-random-string'
```

> Any mistake in this trio is reported at startup with a message naming the fix
> — a missing account, a `ring_aor` matching none of them, a leftover
> `[sip].server`, or both ports left at 5060. It is never reported by the phone
> silently failing to ring.

## 2. Start

```bash
make run          # or: docker compose up -d
```

Expected in the log:

```
INFO sip_server: registrar listening addr=0.0.0.0:5060 realm=gsm-sip-bridge accounts=1 ring_aor=1001
```

## 3. Point a phone at it

On the handset (Grandstream, Yealink, Fanvil, Linphone — anything that speaks
SIP):

| Setting | Value |
|---|---|
| SIP server / registrar | the bridge's LAN IP |
| Port | `5060` |
| Username / auth ID | `1001` |
| Password | whatever you exported |
| Transport | UDP |

Confirm it registered:

```bash
curl -s localhost:9100/metrics | grep sip_server
```

```
gsm_sip_bridge_sip_server_bindings 1
gsm_sip_bridge_sip_server_ring_aor_registered 1
gsm_sip_bridge_sip_server_registrations_total{outcome="challenged"} 1
gsm_sip_bridge_sip_server_registrations_total{outcome="accepted"} 1
```

One `challenged` per `accepted` is normal and expected — every REGISTER is
challenged once, then succeeds on the retry.

## 4. Call the SIM

The phone rings, shows the caller's number, and carries two-way audio once
answered. Hanging up at either end tears down both legs.

---

## Verifying without a phone

`sipsak` is enough to exercise the registrar end to end:

```bash
# Expect: 401 challenge, then 200 OK
sipsak -vv -U -s sip:1001@<bridge-ip>:5060 -u 1001 -a "$PHONE_1001_PASSWORD"

# Expect: 401, and registrations_total{outcome="rejected_auth"} to increment
sipsak -vv -U -s sip:1001@<bridge-ip>:5060 -u 1001 -a wrong-password
```

Or register a softphone (`linphonec`, `pjsua`) and leave it running.

The full wire contract — every status code, every header — is in
[contracts/sip-registrar.md](./contracts/sip-registrar.md), and each row of it is
covered by `gsm-sip-bridge/tests/test_sip_server_registrar.rs`, which needs no
phone, no SIM, and no PJSIP.

---

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Startup error naming both ports | `[sip].local_port` and `[sip_server].listen_port` are both 5060 | Set `[sip].local_port = 5062`. They are two separate SIP endpoints. |
| Startup error about `[sip].server` | A PBX address left in place | Remove `server`, `username` and `password` from `[sip]`. They do nothing in this mode. |
| Startup error about `ring_aor` | It matches no configured account | Make it equal one `[[sip_server.account]].username`. |
| Phone shows "registration failed" | Wrong password, or wrong realm on the handset | Check `registrations_total{outcome="rejected_auth"}`. The realm the phone expects is `[sip_server].realm`. |
| `bindings` is 1 but the phone never rings | The handset is set to accept SIP only from its proxy | Turn that option off — *Accept SIP Trust Server Only* (Yealink), *Accept Incoming SIP from Proxy Only* (Grandstream). The bridge dials from `[sip].local_port`, a different port from the one the phone registered to, though the same IP. |
| Calls stop ringing after a while | The handset's registration lapsed and was not refreshed | Check `ring_aor_registered` — if it is 0, the phone stopped refreshing. Lower `min_expires` or check the handset's registration interval. |
| Log: `no live registration for AOR 1001` | A call arrived while nothing was registered | The mobile call is deliberately left to ring out. `ring_target_missing_total` counts these. |

## Limitations in this version

- **Inbound only.** The phone cannot dial out through the mobile network; such
  an attempt is refused with `403`.
- **One phone rings.** Others may register but are never called. Registering a
  second device on the *same* account replaces the first — calls go to whichever
  registered most recently.
- **UDP only**, and no NAT between phone and bridge. This targets a single-site
  LAN.
- **No message-waiting or busy-lamp** subscriptions (`489 Bad Event`).
