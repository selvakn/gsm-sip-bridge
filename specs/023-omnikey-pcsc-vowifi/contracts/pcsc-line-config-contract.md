# Contract: PC/SC Card-Reader-Backed `[[vowifi.line]]` Entry

The operator-facing configuration surface this feature adds, and the
obligations `supervise` (`gsm-sip-bridge/src/supervise/orchestrate.rs`) and
line resolution (`gsm-sip-bridge/src/vowifi/discovery.rs`) must satisfy for
any entry that sets it.

## Config shape

> **Superseded (post-v8.1.0):** `mcc`/`mnc` are no longer mandatory here. The
> original rationale — "no modem to derive them from" — was wrong: the MCC is
> the first three IMSI digits and the MNC length is in the card's own `EF_AD`
> (`6FAD`), both readable over PC/SC. Only the `AT+COPS` fallback for cards
> whose `EF_AD` lacks the MNC-length byte is modem-only. See the Unreleased
> section of `RELEASE_NOTES.md`. `imsi_override` remains mandatory, but as the
> reader-to-line binding key, not because the IMSI is unreadable.

```toml
[[vowifi.line]]
pcsc_reader = true            # marks this entry as card-reader-backed, not a modem matcher
imsi_override = "404940123456789"  # MANDATORY when pcsc_reader = true
mcc = "404"                        # optional (was mandatory) — derived from EF_IMSI/EF_AD
mnc = "043"                        # optional (was mandatory) — zero-padded to 3 digits
```

`modem_serial`/`modem_port` MUST NOT be set alongside `pcsc_reader = true`
(they are mutually exclusive matchers — a line is either modem-backed or
card-reader-backed, never both). `imei_override` is accepted but ignored for
a `pcsc_reader` line (no modem/IMEI concept applies).

## Obligations of config validation (startup, before line resolution)

1. **Mandatory fields**: a `pcsc_reader = true` entry missing `imsi_override`
   fails config load with an error naming the entry's position — never a
   partial or silently-skipped line. (`mcc`/`mnc` were also required here
   before the supersession noted above; they now auto-derive from the card.)
2. **Engine compatibility**: if any `pcsc_reader = true` entry exists while
   `[vowifi].tunnel_engine = "swu"`, `supervise` fails at startup with an
   error naming the incompatible line and setting, before any per-line
   thread is spawned (spec FR-008). `tunnel_engine = "strongswan"` (the
   default) is required for this feature to do anything.

## Obligations of line resolution (`resolve_lines`)

3. **Independent of modem scanning**: `pcsc_reader` entries become lines
   without any USB modem scan involvement — they do not compete for, block,
   or get excluded by circuit-switched/modem role assignment.
4. **Shared bound**: card-reader lines are appended to the modem-derived
   line list, continuing the same `index` counter, and are subject to the
   same `[vowifi].max_lines` cap as modem lines combined (spec FR-006) — an
   entry pushed past the cap is reported in `LineTableResult.failed` with
   reason `max_lines_exceeded`, identically to an excess modem line today.
5. **Per-index infra reuse**: netns, veth addresses/interfaces, strongswan
   `if_id`/tun-iface, and control port are derived from `index` exactly as
   for a modem line — no new derivation scheme.

## Obligations of orchestration (`start_vowifi_line*`)

6. **No modem preconditions**: for a `pcsc_reader` line, the modem-existence
   check and `modem-ims` reconcile step are skipped entirely.
7. **No usim bridge**: `vowifi-usim-bridge` is never spawned for a
   `pcsc_reader` line — pcscd reaches the physical reader directly (via the
   `ccid` driver) once the Docker image change lands.
8. **IMSI never re-read**: `resolve_imsi`'s existing override-first behavior
   is what supplies a `pcsc_reader` line's IMSI — since the override is
   mandatory (obligation 1), the modem-read fallback path is never reached
   for such a line.
9. **Recovery**: once a `pcsc_reader` line's charon process is up, it is
   supervised by the existing, unmodified `line_supervisor` establish/
   steady-state loop — same recovery semantics as a modem line, no new code
   (see research.md §4).

## Observability guarantee (spec FR-010/SC-005)

10. A `pcsc_reader` line appears in `vowifi-status`, Prometheus metrics, and
    alerting exactly as a modem-backed line does (same `index`/`card_id`
    keying) — no new field, label, or output distinguishes SIM source.
