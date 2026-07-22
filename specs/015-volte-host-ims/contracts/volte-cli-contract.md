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
| `--modem` | from `[volte]` config | AT control port |
| `--cid` | from config | PDP context id for the IMS PDN. Must not collide with the internet context |
| `--apn` | `ims` | What to request. The network's resolved value is reported back |

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

Discover the P-CSCF. (US2)

```
gsm-sip-bridge volte-discover [--modem <path>] [--method <auto|dhcpv6|ra|dns>]
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

Register to the IMS core over LTE. (US3)

```
gsm-sip-bridge volte-register [--modem <path>] [--pcscf <addr>] [--msisdn <e164>] [--once]
```

Runs the full sequence: attach (reusing an existing attachment), discover
(unless `--pcscf` overrides), then register — so it works as a single command
while each stage remains independently invokable.

`--pcscf` is the FR-010 override. `--once` performs a single registration and
exits rather than entering the renewal loop; the default keeps the
registration alive (FR-016).

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
