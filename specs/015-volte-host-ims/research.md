# Phase 0 Research: Host-Side IMS Registration over LTE

**Feature**: `015-volte-host-ims` | **Date**: 2026-07-22

This document records what was established by direct investigation of the
target hardware and the existing codebase, and what remains genuinely
unresolved. Everything under "Verified" was executed against the live
EC200U + Vi India SIM attached to the development machine on 2026-07-22.

---

## R1: Can the carrier grant an IMS PDN to a host-controlled context?

**Decision**: Yes. Establish the IMS PDN via `AT+CGDCONT` / `AT+CGACT` on a
dedicated context ID, and bind it to the host with `AT+QNETDEVCTL`.

**Status**: ✅ **VERIFIED on hardware.**

This was the single largest unknown in the feature. Transcript:

```
AT+CGDCONT=3,"IPV4V6","ims"          -> OK
AT+CGACT=1,3                          -> OK
AT+CGACT?                             -> +CGACT: 1,1 / 2,0 / 3,1
AT+CGCONTRDP=3                        -> +CGCONTRDP: 3,6,"ims.mnc043.mcc404.gprs",
                                         "36.2.129.0.111.254.138.230.0.0.0.12.222.43.56.1..."
AT+CGPADDR=3                          -> +CGPADDR: 3,"0.0.0.0,
                                         2402:8100:6FFE:8AE6:0:C:DE2B:3801"
AT+QNETDEVCTL=1,3,1                   -> OK
AT+QNETDEVCTL?                        -> +QNETDEVCTL: 1,3,1,1
```

After `QNETDEVCTL`, the host CDC-ECM interface `enx024bb3b9ebe5` transitioned
from `DOWN / NO-CARRIER` to `UP`, carrying context 3.

**Key findings**:
- The network resolved the bare APN `ims` to `ims.mnc043.mcc404.gprs` and
  assigned **bearer id 6** — a real, network-granted IMS PDN, not a local
  loopback of the `internet` APN (which is bearer 5, context 1).
- The PDN is **IPv6-only**. The IPv4 address is `0.0.0.0`; only the IPv6
  address is assigned. This matches Indian carrier IMS deployments.
- The modem's internal IMS stack is idle (`+CIREG: 0,0`), so it did not
  contend for the PDN.

**Rationale**: This is the standard 3GPP mechanism and requires no
vendor-specific data path. `QNETDEVCTL` is the only Quectel-specific command
and is documented for the EC200 series.

**Alternatives considered**:
- *QMI (`qmicli --wds-start-network`)* — **rejected, not available.** The
  EC200U is UNISOC UIS8910, not Qualcomm. There is no `/dev/cdc-wdm*` and no
  `/dev/wwan*`; `AT+QMBNCFG` and `AT+QNVFR` both return `+CME ERROR: 58`.
  Any plan predicated on QMI multi-PDN is not implementable on this hardware.
- *The modem's internal IMS stack (`AT+QCFG="ims",1,1`)* — this is the
  existing VoLTE path being replaced. It does not expose SIP signalling.

---

## R2: How is the P-CSCF address obtained?

**Decision**: Automatic discovery with an ordered fallback chain —
DHCPv6 → IPv6 RA → DNS NAPTR/SRV — plus a configuration override.

**Status**: ✅ **RESOLVED — Gate G1 executed 2026-07-22. Result is NEGATIVE:
all three automatic methods are definitively excluded on this carrier.**

**The modem does not expose it.** Verified:

| Attempt | Result |
|---|---|
| `AT+CGCONTRDP=3` | Returns only through DNS fields; **truncates before the P-CSCF fields** that TS 27.007 defines |
| `AT+QPCO?` / `AT+QPCO=3` | `+CME ERROR: 58` (not supported) |
| `AT+QNETDEVSTATUS=3` | `+CME ERROR: 58` |
| `AT+QCFG="pcscf"` | `+CME ERROR: 3` |
| `AT+CGPIAF?` | `+CME ERROR: 58` |

Note that for context 3 the DNS fields `CGCONTRDP` *does* return are all
zeros, which is consistent with the network delivering P-CSCF (not DNS) in
the PCO — the firmware simply is not surfacing that field.

**Chosen order and rationale**:

1. **DHCPv6 Information-Request, RFC 3319 options 21/22** (SIP Servers
   Domain Name List / IPv6 Address List). TS 24.229 §9.2.1 names this as a
   standard P-CSCF discovery mechanism, and it is the most commonly deployed
   on IPv6 IMS APNs. Tried first because it directly returns the answer.
2. **IPv6 Router Advertisement**. Needed regardless — the host must obtain
   its global address and prefix on the ECM link — so this step is not extra
   work. Some deployments carry P-CSCF in an RA option.
3. **DNS NAPTR/SRV on `ims.mnc043.mcc404.3gppnetwork.org`**. The TS 23.228
   fallback. Listed last because it depends on a working resolver on the IMS
   PDN, and `CGCONTRDP` returned no DNS servers for context 3 — so this may
   be unusable in practice.
4. **Configuration override** (FR-010). The escape hatch if all three fail.

### Gate G1 spike results (executed in the privileged container)

| Method | Result | Evidence |
|---|---|---|
| **DHCPv6 (RFC 3319)** | ❌ **Ran; carrier returned no SIP options** | A DHCPv6 server *does* exist at `fe80::5` and replies to INFORMATION-REQUEST (msg-type 7, server-ID enterprise 2011). ORO requested options 21, 22, 23, 24. The reply contained **only option 23 (DNS), valued `['::', '::']`** — null addresses. Options 21 and 22 were not returned at all |
| **IPv6 RA** | ❌ **Ran; carries no P-CSCF** | RA from `fe80::c:de2b:3840` contains exactly two options: prefix info (`2402:8100:6ffe:8ae6::/64`, autonomous) and MTU 1500. No RDNSS, no P-CSCF option |
| **DNS NAPTR/SRV** | ❌ **Cannot run** | No usable resolver. The only DNS servers offered are the null `::` addresses above, so NAPTR/SRV on `ims.mnc043.mcc404.3gppnetwork.org` cannot be issued at all |
| **AT / PCO** *(added during spike)* | ❌ **Not exposed even when enabled** | `AT+QCFG=?` revealed an undocumented `"pdn/pco"` toggle. Setting `AT+QCFG="pdn/pco",1` succeeded and **persisted across a modem reboot** (`AT+CFUN=1,1`), but produced no PCO URC on PDN activation and did not extend `AT+CGCONTRDP` output. `AT+QPCSCF`, `AT+QIMSCFG="pcscf"` → `+CME ERROR: 58/3` |

**Conclusion**: Vi India does not provision the P-CSCF by any mechanism
reachable from the host on this modem. The PCO almost certainly carries it —
the network must be sending it, since the modem's own IMS stack works — but
the EC200U firmware does not surface that field through any interface it
exposes.

**Revised decision**: **Configuration is the primary mechanism, not the
fallback.** The `--pcscf` override (FR-010) is promoted from escape hatch to
the supported path. The three probes are retained only as *diagnostics* — they
run, report what the carrier returned, and confirm the negative — because a
future firmware, carrier, or SIM may behave differently, and the reporting is
what makes that discoverable. They must not be presented to operators as the
expected route to a working address.

**Remaining ways to obtain the address**, for a follow-up spike:
1. **Try the ePDG-side P-CSCF from the LTE PDN.** The IMS core is the same;
   the P-CSCF may be the same node or reachable from the IMS APN. Cheapest
   next test. Note the production VoWiFi config targets Airtel
   (`mcc=404 mnc=094`), not this Vi SIM, so an address would have to be
   captured from a Vi ePDG tunnel first — `pcscf_source_path` in
   `config.toml.example` is where the VoWiFi path already deposits it.
2. **Firmware upgrade.** A build that honours `pdn/pco` would resolve this
   outright. The toggle's existence suggests the capability is intended.
3. **Vendor query to Quectel** for PCO/P-CSCF exposure on EC200U.

**Alternatives considered and rejected**:
- *Capture the P-CSCF the modem's own IMS stack uses* (enable
  `AT+QIMS=1`, packet-capture, read the address). Rejected: the internal IMS
  stack terminates its own signalling inside the modem, so that traffic never
  crosses the ECM link and cannot be captured from the host. `AT+QIMS?`
  reports `DISABLE`, confirming the stack is idle and not contending.

---

## R3: Is the existing IMS stack IPv6-capable?

**Decision**: Largely yes. This is **substantially less work than the spec
assumed**.

**Status**: ✅ **VERIFIED by code audit.**

The spec's Assumptions state that IPv6 "support for that family is therefore
new work, not a given." Auditing the code shows this is too pessimistic:

| Component | IPv6 status | Evidence |
|---|---|---|
| `ims/sip_client.rs` `SipTransport::connect` | ✅ Handled | Binds `[::]:0` vs `0.0.0.0:0` by matching on `SocketAddr::V4/V6` |
| `ims/sip_client.rs` `SipTransport::connect_from` | ✅ Handled | Selects `socket2::Domain::IPV6` from `dst.is_ipv6()`; binds `[::]:port` |
| `ims/sip_client.rs` `bind_gm_socket` | ✅ Handled | Same domain selection for the inbound Gm server |
| `ims/sip_client.rs` `format_sip_addr` | ✅ Handled | Emits `[v6]:port` bracket form required for SIP URIs |
| `ims/gm_ipsec.rs` XFRM | ✅ Expected to work | Takes `IpAddr` and renders via `to_string()`; `ip xfrm` parses v6 addresses natively in both `state` and `sel` clauses |
| `docker/entrypoint.sh` | ✅ Anticipated | Already issues `ip -6 route replace` and sets `disable_policy` on the v6 path |

**Rationale**: The VoWiFi path was evidently written with dual-stack in mind
even though it exercises IPv4. The remaining risk is not "add IPv6 support"
but "confirm the untested v6 code paths actually work" — a verification
task, not an implementation task.

**Residual risk**: `gm_ipsec` XFRM over IPv6 has never been executed. ESP
transport mode with IPv6 selectors is the one place where an untested
assumption could still bite. Task list must include an explicit v6 XFRM
verification step independent of registration.

---

## R4: Where does the VoLTE transport attach — netns or host?

**Decision**: Run in the **host network namespace**, binding to the ECM
interface's address. Do **not** create a dedicated netns.

**Status**: ✅ Decided by design review.

**Rationale**: The VoWiFi path needs a netns per line because each strongSwan
tunnel produces its own XFRM interface and multiple lines must not collide,
and because Agent A/Agent B are bridged over a veth pair. None of that applies
here: there is exactly one ECM interface, it is dedicated to the IMS PDN, and
this feature terminates at registration with no second agent to bridge to.
Constitution Principle V (Simplicity, YAGNI) directs the simpler option.

The compose service already runs `privileged: true` with `network_mode: host`,
so the ECM interface is directly visible with no additional plumbing.

**Alternatives considered**:
- *Move the ECM interface into a dedicated netns* (`ip link set enx… netns`).
  Rejected: adds a moving part with no current benefit. Revisit only if
  multi-card VoLTE (explicitly out of scope) is taken on later.

---

## R5: Control-channel contention between PDN setup and SIM authentication

**Decision**: Serialize. Establish the PDN, then release the AT port before
registration acquires it — which the CLI's separate-steps design (FR-021)
produces naturally.

**Status**: ✅ Decided; ⚠️ one detail to confirm on hardware.

`ims::register_session` opens the modem port via `AtCommander::open` to read
the IMSI/IMEI and drive the USIM AKA challenge. PDN establishment needs the
same control channel.

Of `/dev/ttyUSB0`–`ttyUSB3`, **only `ttyUSB0` responded to `AT`**; `ttyUSB1`
and `ttyUSB2` returned nothing and `ttyUSB3` emitted a binary diagnostic
stream. Ports `ttyUSB4`–`ttyUSB6` were not probed. So a second independent AT
port cannot be assumed.

**Rationale**: Because FR-021 already requires attachment, discovery, and
registration to be separately invokable steps, each acquires and releases the
port in turn. No concurrent access arises in the normal flow, and no locking
layer is needed (Principle V).

**Resolved during the G1 spike**: `/dev/ttyUSB5` and `/dev/ttyUSB6` are both
usable AT ports (`ttyUSB4` is a binary diagnostic stream). A second AT port is
therefore available if a long-running supervisor ever needs to hold PDN state
while registration renews. Still not required for this feature — the
sequential design stands — but the option exists.

---

## R6: What happens to general internet connectivity?

**Decision**: Accept the displacement; report it before applying (FR-006).

**Status**: ✅ Verified as low-impact on the target machine.

`AT+QNETDEVCTL=1,3,1` re-points the single CDC-ECM data path from context 1
(`internet`) to context 3 (`ims`). The EC200U exposes exactly one ECM
interface, so the two cannot be carried simultaneously to the host.

On the target machine this costs nothing: `enx024bb3b9ebe5` was `DOWN` with
`NO-CARRIER` before the investigation, i.e. the modem's data path was unused —
the host reaches the internet via `enxa84a63261317`.

**Rationale**: Scope is registration only, so no general connectivity is
needed over the modem. FR-005 requires teardown to restore the prior binding.

---

---

## R7: The host must adopt the modem-assigned interface identifier

**Decision**: Before soliciting an RA, set the interface's link-local address
to the IID the modem assigned, and disable the kernel's own address
generation. Without this the IMS PDN is unusable from the host.

**Status**: ✅ **VERIFIED on hardware during the G1 spike.** This was the
single blocking defect standing between "PDN bound" and "host routed", and it
is not something the plan anticipated.

**Symptom**: with the interface up and bound to the IMS PDN, the host received
no address and no route. `dhcpcd` reported `no IPv6 Routers available`;
`rdisc6` and raw sockets failed with `Address not available`.

**Diagnosis**: the network **unicasts** the Router Advertisement to the
link-local form of the IID it assigned, not to `ff02::1`:

```
fe80::c:de2b:3840 > fe80::c:de2b:3801 : ICMP6, router advertisement
```

`AT+CGPADDR=3` had reported `2402:8100:6FFE:8AE6:0:C:DE2B:3801` — so the
assigned IID is `0:c:de2b:3801`, and the expected link-local is
`fe80::c:de2b:3801`. Linux had autogenerated a stable-privacy link-local
(`fe80::1443:91a4:aa1e:c3db`) instead, so every RA was addressed to someone
else and was silently discarded. The RAs were arriving the whole time — only
a packet capture revealed it.

**Fix** (verified working):

```sh
ip link set "$IF" down
ip -6 addr flush dev "$IF"
echo 1 > /proc/sys/net/ipv6/conf/$IF/addr_gen_mode    # 1 = none
ip link set "$IF" up
ip -6 addr add fe80::<assigned-iid>/64 dev "$IF" scope link
sysctl -w net.ipv6.conf.$IF.accept_ra=2
```

**Result**: RA accepted, SLAAC configured
`2402:8100:6ffe:8ae6:4b:b3ff:feb9:ebe5/64`, and a default route installed via
`fe80::c:de2b:3840`. **The host is fully routed on the carrier's IMS PDN.**

**Implementation consequences**:
- `volte/netcfg.rs` must derive the link-local from `AT+CGPADDR` and set
  `addr_gen_mode=none` before bringing the interface up. This is a hard
  requirement, not a tuning detail.
- SLAAC derived the global address from the **ECM MAC (EUI-64)**, giving
  `…:4b:b3ff:feb9:ebe5`, *not* the modem-assigned `…:0:c:de2b:3801`. 3GPP
  expects the UE to use the assigned IID. Whether the network routes the
  EUI-64-derived address is **unverified** — the implementation should add
  the `CGPADDR` address explicitly and prefer it as the SIP source address.
  Getting this wrong would produce a registration that sends but never
  receives.
- The assigned IID changes on every PDN reactivation (it differed after the
  reboot), so it must be read fresh each time, never cached.

---

## Summary of corrections to the specification

Two spec assumptions should be read in light of this research:

1. **IPv6 support is mostly present** (R3), not new work. The spec's
   Assumptions section overstates this risk. No spec change is required —
   the requirement (FR-020) remains correct — but planning should treat it
   as verification rather than implementation.
2. **The QMI-based approach is impossible**, not merely unchosen (R1). Any
   future reader should not attempt it on EC200U hardware.

3. **Automatic P-CSCF discovery is not achievable on this carrier + modem**
   (R2, Gate G1). The spec's US2 and SC-002 assume it is. Both require
   amendment — see `plan.md` → "Post-G1 plan revision".
4. **The link-local IID requirement (R7) is a hard prerequisite** that no
   part of the original spec or plan anticipated.

---

## R10: "Attached" and "usable" are different states — test for the route

**Decision**: The signal that FR-024 worked is the **default route**, never the
presence of a global address.

**Status**: ✅ Verified — found by running the Phase 2 implementation against
live hardware.

The first working build reported success on an interface that had no route and
could not have carried a packet. The implementation waited for "a global
address to exist", but `configure_steps` **installs the network-assigned
address itself** (R9), so that condition was already true the instant
configuration finished and said nothing about whether the RA had been
accepted. Unit tests could not have caught this: the logic was self-consistent
and only the hardware disagreed.

The default route can only come from an accepted RA, so it is the honest test.
Two supporting details, both observed:

- The kernel emits no Router Solicitation while the link-local is still
  `tentative` (duplicate address detection), so a naive immediate poll can
  time out on a link that was about to come good. An explicit solicitation —
  toggling `accept_ra` — makes bring-up deterministic without needing `rdisc6`
  in the image.
- Once correct, the interface carries **both** addresses: the assigned `/128`
  we install and a SLAAC `/64` derived from the ECM MAC, with
  `default via fe80::…:2540 proto ra`.

**Consequence**: `AttachReport` carries an explicit `routed` flag, and the
operator-facing summary states routability in as many words. Reporting an
attachment that cannot carry traffic as success is exactly the kind of failure
FR-015 exists to prevent.

---

## R11: `CGACT=1` returns before the address is assigned

**Decision**: Poll `AT+CGPADDR` after activation until an address appears,
bounded. Never read it once.

**Status**: ✅ Verified — found by running Phase 3 against live hardware.

`AT+CGACT=1` returns `OK` as soon as the context is *active*, which is earlier
than the network assigning an address. Reading `AT+CGPADDR` immediately races
that assignment and intermittently reports a PDN with no address at all. The
downstream effect is badly misleading: address family reads `none`, host
interface configuration is skipped silently, and the operator sees an
interface that is administratively down with no addresses — which looks like a
bug in the interface code and is not one.

This is intermittent by nature. It did not appear in the first hardware runs
because the PDN had been active for some time already; it appeared reliably
only when attaching straight after a teardown.

**Consequence**: `read_pdn_when_addressed` waits up to 15s. An active context
that never produces an address is a genuine failure and is reported as one,
rather than being passed downstream as a PDN with no address.

## R12: Duplicate address detection and carrier gate everything else

**Decision**: Wait for carrier, then for DAD to complete on the link-local,
before soliciting a router or sending any link-local multicast.

**Status**: ✅ Verified alongside R11.

Two failures share this root cause, and neither names it:

- The kernel emits no Router Solicitation while the link-local is `tentative`,
  so the default route never arrives and the attachment reports unroutable.
- Sending to `ff02::1:2` (DHCPv6) with no settled link-local source fails as
  `Network is unreachable` — which reads as a routing problem and is not one.

The interface also needs a moment after `AT+QNETDEVCTL` before it has carrier,
and DAD cannot progress without it.

**Consequence**: `netcfg` gained `carrier_up`/`wait_for_carrier` and
`link_local_ready`/`wait_for_link_local`, and the DHCPv6 probe checks for a
ready link-local up front so it reports the real precondition rather than
`ENETUNREACH`.

---

## R13: The P-CSCF, captured from a Vi ePDG tunnel — Gate G3 PASSED

**Decision**: Obtain the P-CSCF from the existing VoWiFi/ePDG path and supply
it to the LTE transport. **Verified reachable from the LTE IMS PDN.**

**Status**: ✅ **Gate G3 exit criterion met, 2026-07-22.**

**Capture.** The production VoWiFi stack was run unmodified against the Vi SIM.
Its config needed no edits: `mcc`/`mnc` are commented out, so the home PLMN is
derived from the SIM, which produced `mcc=404 mnc=043` and resolved
`epdg.epc.mnc043.mcc404.pub.3gppnetwork.org`. The tunnel came up with live
EAP-AKA against the SIM over `AT+CSIM`:

```
EAP method EAP_AKA succeeded, MSK established
IKE_SA established  192.168.15.10 ... 203.88.4.88
received P-CSCF server IP  2400:5200:a100:819::6
installing new virtual IP  2402:8100:7889:1d45:0:22:1e10:e901
```

| Item | Value |
|---|---|
| **P-CSCF** | **`2400:5200:a100:819::6`** (SIP port 5060) |
| ePDG | `epdg.epc.mnc043.mcc404.pub.3gppnetwork.org` → `203.88.4.88` (**also has IPv4** — `203.88.11.33` too) |
| MSISDN | `+918807793613`, from the reg-event NOTIFY |
| IMPU realm | `ims.mnc043.mcc404.3gppnetwork.org` |

Note the ePDG publishes **both** A and AAAA records. An earlier `dig +short
NAME A AAAA` returned only the AAAA answers — `dig` treats the trailing
arguments as one type/class, so that was a single query, not two. The IPv4
path exists and is what the tunnel actually used, which matters because this
host has no IPv6 internet.

**Reachability from LTE.** With the VoWiFi stack stopped and the LTE IMS PDN
up (source `2402:3a80:2314:bb3d:0:25:ff2c:2501`):

| Target | Result |
|---|---|
| `[2400:5200:a100:819::6]:5060` | ✅ **TCP CONNECTED** (confirmed twice, Python and `nc`) |
| `[2400:5200:a100:819::6]:6000` | timed out — expected; 6000 was that ePDG session's negotiated Gm port, not a listener |
| IMS PDN gateway `:5060` | timed out — confirms this is not a blanket "everything connects" |
| ICMP to the P-CSCF | no reply — normal, ICMP to a P-CSCF is typically blocked |

**Remaining risk**: reachability is not acceptance. The P-CSCF answers TCP from
the LTE access, but whether it will *accept* an IMS-AKA REGISTER over a
different IP-CAN is unproven. Expect to need `P-Access-Network-Info` describing
E-UTRAN rather than IEEE-802.11, and note the LTE PDN address
(`2402:3a80:…`) sits in a different prefix from the ePDG inner address
(`2402:8100:…`). Only an actual registration settles it.

## R14: Gate G2 answered as a side effect — Gm IPsec over IPv6 works

**Status**: ✅ **Gate G2 met.** No separate verification task is needed.

The same VoWiFi run carried the authenticated REGISTER over Gm IPsec **on
IPv6**, against this exact carrier:

```
network proposed Gm IPsec  alg=hmac-md5-96; ealg=aes-cbc;
                           spi-c=7192031; spi-s=7192030; port-c=32813; port-s=6000
Gm IPsec SAs installed
reconnected over Gm IPsec transport  peer=[2400:5200:a100:819::6]:6000
REGISTER response  status=200 OK
```

This was the one untested surface flagged in R3: `ims/gm_ipsec.rs` had never
had its XFRM states and policies exercised with IPv6 selectors. They install
and carry traffic correctly. Note the cipher is `aes-cbc`, not the null cipher
— so the encryption path is exercised too, not just authentication.

---

## R15: The IMS PDN does not outlive the registration

**Decision**: Re-establish the network attachment before every renewal, and
treat a down attachment as its own failure rather than letting it surface as a
connect error.

**Status**: ✅ Found by the SC-004 soak; fixed and the fix verified on hardware.

The soak registered at 09:21:38 and renewed cleanly at 10:16:40 and 11:11:42 —
both exactly 3300s apart (3600s granted expiry − 300s headroom), confirming
SC-004. It then kept running, and at 12:06:47 the third renewal began failing:

```
renewal failed ... No route to host   retry_in_secs=5
renewal failed ... connection timed out retry_in_secs=20
renewal failed ... No route to host   retry_in_secs=80
```

**The carrier had torn the IMS PDN down** after roughly two hours, while the
radio stayed perfectly healthy:

| Check | Value |
|---|---|
| `AT+CGACT?` cid 3 | `0` — deactivated |
| `AT+QNETDEVCTL?` | `0,0,0,0` — netdev unbound |
| `AT+CGPADDR=3` | all zeros |
| Host interface | `DOWN`, route marked `linkdown`, stale addresses still installed |
| `AT+CEREG?` / `AT+CSQ` | `0,1` / `21` — radio fine |

**Two defects, both invisible to unit tests:**

1. The renewal loop retried `run_register` forever but never re-established the
   *transport*. A dropped PDN can never be recovered by retrying REGISTER, so
   the loop would have backed off to its 300s ceiling and spun indefinitely
   against a dead attachment.
2. `LteImsPdnTransport::prepare` cached its handle and returned early, making
   the transport blind to exactly this. "Idempotent" in the contract means
   *does not create a second attachment* — which `attach` already guarantees at
   the modem level — not *cached*.

A third, smaller one: `gsm_bridge_volte_pdn_up` was set at attach and cleared
only at detach, so it kept reporting `1` throughout the outage — the metric
would have lied to a dashboard during precisely the incident it exists for.

**Fix**: `prepare` re-verifies rather than caching; the renewal loop calls
`refresh_attachment` before each renewal (a cheap no-op when healthy); a down
attachment is reported as `attachment_down` with its own reason and skips the
doomed REGISTER entirely; and the PDN gauge follows the refresh result.

**Verified**: restarted against the dead PDN left by the soak — re-attached,
routable, and registered within ~4 seconds.

**What the soak validated in passing**: the bounded backoff (5→10→20→40→80,
heading for the 300s cap, FR-016), failure recording with a reason (FR-023),
and stage-attributed errors (FR-015) all behaved correctly under a real
failure, not just in tests.

## Unresolved items carried into planning

| ID | Item | Blocking? | Status |
|---|---|---|---|
| R2 | Which P-CSCF discovery method works on Vi India | — | ✅ **Resolved: none do.** Configuration is now the primary mechanism |
| R5 | Whether `ttyUSB4`–`6` offer a second AT port | No | ✅ Resolved: `ttyUSB5`/`ttyUSB6` are AT ports |
| R7 | Link-local IID must match the modem-assigned one | — | ✅ Resolved and fix verified |
| **R8** | **Obtaining a Vi India P-CSCF address at all** | **Yes, for US3** | ⚠️ **OPEN — new blocker.** See R2 "Remaining ways to obtain the address" |
| R9 | Whether the network routes the EUI-64 SLAAC address, or requires the `CGPADDR`-assigned IID | Yes, for US3 | ⚠️ OPEN — verify when first registration is attempted |
| R3 | Gm IPsec XFRM over IPv6 never executed | Yes, for US3 | ⚠️ OPEN — Gate G2 |
