# Migrating to strict config parsing

`config.toml` is now parsed by serde with `deny_unknown_fields`, replacing a
hand-written walk over the TOML document. **The file format did not change** —
every key means what it meant before, with the same default and the same
range. What changed is what happens when a config is *wrong*.

Most deployments need no edit at all. Start the bridge; if it starts, you are
done. If it refuses, the error names the offending key and this document
explains why.

---

## The one change that will affect you: unknown keys are fatal

**Before:** an unrecognised key logged `WARN unknown config key` and startup
continued with the setting at its default.

**After:** startup fails, listing every unrecognised key at once.

```
Error: configuration error: unknown config keys in /etc/gsm-sip-bridge/config.toml:
vowifi.max_line, volte.netns. Check for a typo, or a setting that was renamed
or removed — see docs/configuration.md. Nothing is applied from an
unrecognised line.
```

### Why

A typo silently did nothing. `max_line = 2` (missing the `s`) left
`max_lines` at its default of 8, and the single WARN was buried in a
container's modem-probing startup output — frequently emitted *before* the
configured log level had been applied, so it could be filtered out entirely.

The operator's mental model said "I set this". The bridge's behaviour said
otherwise, and nothing reconciled the two until something went wrong for a
reason that looked unrelated.

This system has already lost time to a config value that was accepted but
wrong: an empty APN made `AT+CGDCONT` request the network's default bearer
instead of the IMS one, producing a line that attached, looked fully
configured, and could not reach the P-CSCF. A key the operator believes they
set and the bridge silently ignored belongs in the same category.

### What to do

Start the bridge once and read the error. Every offending key is listed
together, qualified by section, so one run finds all of them.

The keys most likely to appear are the **derived per-line resources**, which
were never settable but used to be tolerated:

| Section | Keys that are now refused | Why they were never settings |
|---|---|---|
| `[vowifi]` | `netns`, `veth_local_addr`, `veth_peer_addr`, `veth_sip_iface`, `veth_ims_iface`, `strongswan_tun_iface`, `strongswan_if_id` | Derived from each line's index so lines cannot collide |
| `[vowifi]` | `mcc`, `mnc`, `modem_port`, `imsi_override`, `imei_override`, `pcsc_reader` | Per-line identity — set these on a `[[vowifi.line]]` entry |
| `[volte]` | `netns`, `veth_carrier_iface`, `veth_telephony_iface`, `veth_carrier_addr`, `veth_telephony_addr` | Same per-line derivation |
| `[volte]` | `modem_port`, `iface`, `cid`, `apn`, `pcscf`, `msisdn` | Per-line — set these on a `[[volte.line]]` entry |

**Delete these lines.** They have never had any effect. If you set one hoping
to pin a namespace or an interface name, that never worked — the value was
overwritten per line at startup. Setting the per-line ones (`mcc`, `cid`, …)
at the top level likewise did nothing; move them into a `[[vowifi.line]]` or
`[[volte.line]]` entry.

---

## Smaller behavioural changes

### `env:` now applies to string values only

`env:VAR` was previously also accepted on a few numeric fields, where the
resolved text was parsed as a number. It now applies to strings only.

In practice this affects nothing that was documented or shipped: the example
config and the reference use `env:` for `sip.password` and the Discord webhook
URLs, all of which are strings. If you were pulling a *port* or a *timeout*
from the environment, set it literally in `config.toml` instead — that file
holds all non-secret configuration by design, and a port is not a secret.

### A wrong *type* is now an error, not a fallback

Writing `enabled = "yes"` (a string where a boolean belongs) previously fell
back to the default in the `[scheduled_restart]` and `[alerts]` sections. It
is now a parse error naming the field.

Wrong *values* behave exactly as before — see the next section.

### Two `[vowifi]` values that were unchecked are now validated

- `tunnel_engine` must be `"strongswan"` or `"swu"`. Anything else was
  previously accepted and then failed later, opaquely, when the engine could
  not be started.
- Setting `mcc` without `mnc` (or vice versa) on a `[[vowifi.line]]` is
  rejected. Half a PLMN is not usable, and the previous behaviour silently
  fell back to auto-derivation.

---

## What deliberately did *not* change

**`[scheduled_restart]` and `[alerts]` still tolerate bad values.** An
unparseable cron expression, or a threshold outside its range, disables that
one feature and logs why; the bridge starts and answers calls.

This is intentional and is not an oversight left over from the old parser.
Both are *auxiliary*: a preventive nightly modem restart and a Discord alert
are worth having, but refusing to answer calls because a cron expression was
mistyped would be a strictly worse outcome than not restarting on schedule.
Every other section is strict, because a bridge running with a nonsensical SIP
port or backoff is not usefully "up".

**Every key, default, and range is unchanged.** The migration preserved them
field by field, and the existing config test suite — which encodes several
live-found bugs — passes unmodified apart from the tests that specifically
asserted the old lenient behaviour.

---

## For contributors: adding a config key

Previously this meant editing four places (the runtime struct, a `parse_*`
function, a `*_KEYS` constant, and the example), with the compiler checking
none of them.

Now:

1. Add the field to the section's struct in `src/config/raw.rs`. **Name it
   exactly as the TOML key** — there are no `#[serde(rename)]` attributes, so
   the field name and the accepted key cannot disagree.
2. Add its default to that struct's `Default` impl.
3. Map it (with any range check) in `src/config/build.rs`.
4. Document it in `docs/configuration.md` and add it to `config.toml.example`.

Step 4 is enforced: `tests/test_config_docs.rs` fails if a key the parser
accepts is absent from the reference, or if the example sets a key the parser
rejects. The key list it checks against is generated by the `section!` macro
from the struct itself, so there is no separate list to keep in sync.
