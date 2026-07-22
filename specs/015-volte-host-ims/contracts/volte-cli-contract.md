# Contract: VoLTE CLI Surface

**Feature**: `015-volte-host-ims` | **Satisfies**: FR-021, FR-003, FR-009, FR-011, FR-015, FR-022

## Naming convention

Follows the established precedent in `cli.rs`: **flat, kebab-case
subcommands**, not nested groups — matching `ims-register`, `ims-call`,
`vowifi-status`, `vowifi-ims-agent`, `modem-ims`.

Each is a **standalone diagnostic command** in the same sense as
`ims-register`: it does not start the daemon or touch the CardPool. This is
what makes FR-021 hold — each stage is separately invokable and separately
diagnosable.

---

## `volte-pdn`

Manage the IMS PDN attachment. (US1)

```
gsm-sip-bridge volte-pdn --action <up|down|status> [--modem <path>] [--cid <n>] [--apn <name>]
```

| Option | Default | Notes |
|---|---|---|
| `--modem` | `/dev/ttyUSB0` | AT control port |
| `--iface` | unset | Host interface carrying the data path. Unset manages the PDN only and skips host interface configuration |
| `--cid` | `3` | PDP context id for the IMS PDN. Must not collide with the internet context |
| `--apn` | `ims` | What to request. The network's resolved value is reported back |

> **Defaults are CLI-level, not configuration-level.** An earlier draft of this
> contract said these came "from `[volte]` config". No such section exists yet;
> see `plan.md` → "Deferred from Phase 2".

### `--action up`

Exit 0 on an attachment that is active **and** bound to the host.

Must report: requested APN, **network-assigned APN**, bearer id, assigned
address, address family, and host interface (FR-003).

Must report, **before applying it**, that binding the host data path to the
IMS PDN displaces whatever it was bound to previously (FR-006).

If already active, reports reuse rather than creating a second attachment, and
still exits 0 (FR-004, US1 scenario 2).

On network refusal: non-zero exit, reporting the network's reason, with prior
host binding left untouched (US1 scenario 3).

### `--action down`

Releases the attachment and restores the previously bound context (FR-005).
Safe to run when nothing is attached — exits 0 with a no-op report.

### `--action status`

Reports current attachment state without changing it. Exit 0 whether or not an
attachment exists; the state is in the output, not the exit code.

---

## `volte-discover`

Probe for the P-CSCF and report what each mechanism returned. (US2)

**These probes are diagnostics, not the supported way to get an address.**
Gate G1 established that the tested carrier publishes no P-CSCF by any
mechanism reachable from the host, so an empty result is the expected outcome
there. The chain is `dhcpv6` → `pco` → `dns`.

```
gsm-sip-bridge volte-discover [--modem <path>] [--iface <if>] [--method <auto|dhcpv6|pco|dns>]
                              [--mcc <n>] [--mnc <n>] [--pcscf <addr>]
```

Requires an active attachment; if none exists, fails at stage
`AttachingPdn` and says so rather than reporting a discovery failure.

`--method` forces a single method instead of the chain — a diagnostic aid for
Gate G1, letting each method be evaluated in isolation.

**Output must always include the full `DiscoveryReport`**, listing every
method attempted in order with its individual result, whether or not discovery
succeeded (FR-009, FR-011). A run that succeeds on method 2 must still show
that method 1 was tried and what it returned.

Distinguishes `NoResult` (method ran; carrier returned nothing) from `Error`
(method could not run) — the distinction carries the diagnostic value.

When a configuration override is set, reports that the override is in effect
and skips discovery (FR-010, US2 scenario 4).

Exit 0 if an endpoint was determined by any means; non-zero if all failed.

---

## `volte-register`

Register to the IMS core over LTE. (US3, US4)

```
gsm-sip-bridge volte-register --pcscf <addr> [--modem <path>] [--iface <if>]
                              [--cid <n>] [--apn <name>] [--pcscf-port <n>]
                              [--tcp <bool>] [--sec-agree <bool>] [--msisdn <e164>]
                              [--once] [--keep-pdn] [--status-path <path>]
                              [--force] [--lock-path <path>]
```

| Option | Default | Notes |
|---|---|---|
| `--pcscf` | **required** | Automatic discovery does not work on the tested carrier |
| `--pcscf-port` | `5060` | |
| `--tcp` | `true` | |
| `--sec-agree` | `true` | Vodafone India rejects a plain digest REGISTER without it |
| `--once` | off | Register once and exit rather than staying up and renewing (US4) |
| `--keep-pdn` | off | Leave the IMS PDN attached afterwards, for inspection |
| `--status-path` | `/tmp/volte-registration-status` | Where state is published for `volte-status` |
| `--force` | off | Register despite a running VoWiFi agent — see below |
| `--lock-path` | `/tmp/volte-registration.lock` | Prevents two concurrent VoLTE registrations |

**Mutual exclusion.** The command MUST refuse to run while a VoWiFi agent is
registered, before touching the modem, so a refusal leaves the system exactly
as it was. Both paths present the same IMPU with the same IMEI-derived
`+sip.instance`, so the network treats one registration as a re-registration of
the other and tears the first binding down. `--force` overrides, for
deliberately testing that interference. A second concurrent `volte-register` is
refused by the lock file; a lock left by a crashed run is taken over rather
than requiring manual cleanup.

**Renewal.** By default the registration is kept alive, renewed ahead of expiry
using the lifetime the *network granted* (FR-016). Renewal failures are
recorded with the reason and retried on a bounded backoff.

Runs the full sequence: attach (reusing an existing attachment) then register,
so it works as a single command while each stage remains independently
invokable.

**On failure, must name the stage reached** (FR-015), distinguishing at
minimum: attachment failure, discovery failure, credential/identity rejection,
and signalling-protection failure. A bare "registration failed" violates this
contract.

Must recognise and distinctly report a network demand for protected signalling
that the bridge did not offer, since it is actionable and unlike a credential
failure (spec Edge Cases).

Must report a network-initiated deregistration arriving immediately after
acceptance as a failure, never as success (spec Edge Cases).

Exit 0 only on an accepted registration.

---

## `volte-status`

Report registration state. (US4, FR-022)

```
gsm-sip-bridge volte-status
```

Reports attachment state, P-CSCF in use with its discovery method, and current
registration state with time to expiry.

**Must use the same vocabulary as `vowifi-status`** for the states both share,
so operators do not learn a second dialect for the same concepts (FR-022,
spec Assumptions). Where the existing VoWiFi status output has an established
term for a state, that term is reused verbatim.

---

## Cross-cutting requirements

**Diagnostics.** Every command records enough about its attempt for an
operator to diagnose failure without re-running under additional
instrumentation (FR-023). Following existing convention, detail goes to
`tracing` at `debug`, and the human-facing summary to stdout.

**Non-regression.** No existing subcommand changes its name, options, output,
or exit codes (FR-019).

**Privilege.** All of these require `CAP_NET_ADMIN` and run inside the
existing privileged container. A command must fail with a clear message naming
the missing capability rather than producing a confusing downstream error —
the research phase lost time to exactly this failure mode.
