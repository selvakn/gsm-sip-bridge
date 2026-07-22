# Contract: `volte-call` CLI Surface

**Feature**: `016-volte-calls` | **Satisfies**: FR-001–FR-010, FR-017, FR-018, FR-022, FR-025, FR-027

## Naming

Follows the established precedent: flat, kebab-case, matching `ims-call`,
`volte-register`, `volte-status`. A standalone diagnostic command — it does not
start the daemon or touch the CardPool.

```
gsm-sip-bridge volte-call --callee <e164> [--modem <path>] [--iface <if>]
                          [--pcscf <addr>] [--duration <secs>] [--ring-timeout <secs>]
                          [--audio <file> | --tone] [--record <path>] [--record-sent <path>]
                          [--one-way-threshold <percent>] [--force] [--keep-pdn]
```

| Option | Default | Notes |
|---|---|---|
| `--callee` | **required** | E.164. Dialled as a telephone number, not a resolvable address |
| `--duration` | 30s | How long to hold the call once answered (FR-027). Default satisfies SC-006 |
| `--ring-timeout` | existing default | How long to wait for an answer |
| `--audio` | unset | Send this recording instead of the generated speech-like signal |
| `--tone` | off | Send the legacy tone pattern — the cheap "is there an audio path at all" check |
| `--record` | generated | Where the far end's audio is written |
| `--record-sent` | generated | Where our outgoing audio is written, separately |
| `--one-way-threshold` | 10% | Proportion below which a direction counts as failed |
| `--pcscf` | from the ePDG capture | Same resolution order as `volte-register` |
| `--force` | off | Proceed despite a running Wi-Fi calling agent |
| `--keep-pdn` | off | Leave the attachment up afterwards, for inspection |

## Behavioural obligations

### Refuses before it dials

The command MUST fail **before placing a call** — not after — when:

| Condition | Requirement |
|---|---|
| No accepted registration | FR-006. Report it as the cause; do not dial |
| A Wi-Fi calling agent is running | FR-022. Both hold the same subscriber's registration. `--force` overrides |
| Another host-side registration or call is running | Same lock as `volte-register` (research R1) |
| The wideband codec is missing from the build | FR-010. Report it rather than making an offer that cannot succeed. **Gate C2** |

Refusing early matters: each of these otherwise surfaces much later as a
confusing rejection or a meaningless narrowband quality result.

### The registration is owned by the command

The command establishes its own registration and places the call on it
(research R1). It therefore **cannot run alongside `volte-register`**, and the
refusal message MUST say so plainly and name the remedy — stop the
registration loop, run the call, restart it.

This is a deliberate limitation of a diagnostic command, not a defect. The
follow-up bridging feature is where one long-lived registration serves both.

### Reports progress, and how the call ended

MUST report progress through attempting → ringing → answered → ended (FR-004),
and MUST name what ended it (FR-005): the duration elapsing, the far end
hanging up, operator interrupt, or the attachment being lost.

MUST end the call early when the far end hangs up rather than holding it open
for the remaining duration (FR-027).

### Names the stage on failure

MUST identify the stage reached (FR-017), distinguishing at minimum:

| Stage | Example |
|---|---|
| No registration | Nothing to place a call on |
| Rejected by the network | With the reason the network gave (FR-018) |
| Formats refused | Naming what was offered (FR-009) |
| Answered but silent | See the media report contract — this is a **failure** |
| Attachment lost mid-call | Distinct from the far end hanging up |

A bare "call failed" violates this contract.

### Exit status

Exit 0 **only** when the call was answered *and* audio flowed both ways. An
answered call with a one-way or absent audio path exits non-zero — the report
explains why (FR-016).

This is the rule that stops a silent call from looking like a success in a
script.

### Audio it sends

Speech-like audio by default (FR-025); `--audio` for a real recording; `--tone`
for the legacy pattern.

**MUST NOT default to anything under `samples/`.** Those are real call
recordings of real subscribers (research R3); transmitting one over a live
carrier to a test number would be a privacy problem, not a test.

## Cross-cutting

**Privilege**: requires `CAP_NET_ADMIN`; runs in the privileged container. Must
fail with a clear message naming the missing capability rather than a confusing
downstream error.

**Modem access**: quality-class sampling during the call MUST use a different
AT port from the one carrying call control, or the two collide (research R4).

**Non-regression**: `ims-call` — the only other caller of the shared call path
— MUST keep its current name, options, output and exit codes (FR-020). All
additions are optional.
